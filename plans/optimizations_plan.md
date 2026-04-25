# План оптимизаций APEX ECS

## Общая информация

Данный план составлен на основе анализа исходного кода всех крейтов APEX ECS
и рекомендаций из первичного анализа. Каждая оптимизация разбита на шаги,
содержит конкретные изменения в коде (файл, строки, что менять),
требования к тестовому покрытию и ожидаемый эффект.

---

## Фаза 0: Подготовка

### 0.1 Проверить текущее состояние тестов

**Действие:** Запустить `cargo test --workspace` и убедиться, что все тесты проходят.

**Файлы:** Весь workspace.

```bash
cargo test --workspace 2>&1
```

---

## Фаза 1: Критические оптимизации (Priority: Critical)

### 1.1 Оптимизация `has_edge_between` → `HashSet` в графе зависимостей scheduler

**Проблема:**  
[`scheduler/src/lib.rs:739-741`](../crates/apex-scheduler/src/lib.rs:739) — метод `has_edge_between()` выполняет линейный
проход по всем successors: `self.dependency_graph.successors(from).any(|succ| succ == to)`.
Вызывается O(N²) раз в [`add_new_nodes_and_edges()`](../crates/apex-scheduler/src/lib.rs:748)
при каждой компиляции графа.

**Решение:**  
Добавить `FxHashSet<(NodeIndex, NodeIndex)>` (множество рёбер) в структуру Scheduler.
Проверять наличие ребра через `self.edge_set.contains(&(from, to))` — O(1) amortized.
Обновлять `edge_set` при каждом добавлении ребра.

**Конкретные изменения:**

1. В структуру [`Scheduler`](../crates/apex-scheduler/src/lib.rs:282-329) добавить поле:
```rust
edge_set: FxHashSet<(petgraph::graph::NodeIndex, petgraph::graph::NodeIndex)>,
```

2. Инициализировать в [`Scheduler::new()`](../crates/apex-scheduler/src/lib.rs:332).

3. Переписать [`has_edge_between()`](../crates/apex-scheduler/src/lib.rs:739):
```rust
fn has_edge_between(&self, from: Index, to: Index) -> bool {
    self.edge_set.contains(&(from, to))
}
```

4. При каждом `self.dependency_graph.add_edge(...)` добавлять в `edge_set`.

5. При инвалидации плана ([`invalidate_plan()`](../crates/apex-scheduler/src/lib.rs:563)) очищать `edge_set`.

**Тесты:** Существующие тесты планировщика (24 теста в конце файла) должны покрыть.
Добавить тест на корректность `has_edge_between` после серии add_edge/add_dependency.

**Ожидаемый эффект:** Ускорение `compile()` с O(N²) до O(N²) с константой ~100× меньше
(линейный поиск → HashMap lookup). Для 100 систем: ~4950 проверок, каждая с линейным
поиском по ~50 successors в среднем → ~247500 операций → ~4950 операций.

---

### 1.2 Добавление `Column::reserve(n)` и оптимизация `spawn_many_inner`

**Проблема:**  
[`Column::grow()`](../crates/apex-core/src/archetype.rs:155) только удваивает capacity (64→128→256→...).
В [`spawn_many_inner()`](../crates/apex-core/src/world.rs:357-399) вызов `grow()` в цикле
(строка 376) приводит к многократным realloc+copy (до log₂(count) раз на колонку).

**Решение:**  
Добавить [`Column::reserve(n)`](../crates/apex-core/src/archetype.rs:155-174) — метод,
который увеличивает capacity сразу до `target_cap` (или ближайшей степени двойки ≥ target_cap)
одним alloc + copy.

**Конкретные изменения:**

1. В [`Column`](../crates/apex-core/src/archetype.rs:22-32) добавить:
```rust
pub(crate) fn reserve(&mut self, additional: usize) {
    let needed = self.len + additional;
    if needed <= self.capacity { return; }
    // Вычисляем новую capacity как степень двойки >= needed
    let new_cap = if needed < 64 { 64 } else { needed.next_power_of_two() };
    // ... аналогично grow() но с new_cap вместо self.capacity * 2
}
```

2. В [`spawn_many_inner()`](../crates/apex-core/src/world.rs:357) заменить:
```rust
// Было:
for col in &mut self.archetypes[arch_idx].columns {
    while col.capacity < target_cap { col.grow(); }
}
// Стало:
for col in &mut self.archetypes[arch_idx].columns {
    col.reserve(target_cap);
}
```

3. В макросе [`impl_bundle!`](../crates/apex-core/src/world.rs:1081-1163) в `write_into_batch`
заменить вызов `if col.len >= col.capacity { col.grow(); }` на `col.grow()` если capacity < row+1,
либо тоже использовать reserve.

**Тесты:**  
- unit тест на Column::reserve — проверить что после reserve(n) capacity ≥ n и данные целы.
- spawn_many с 1000 entity — проверить что все созданы корректно.
- Существующие тесты spawn_bundle / spawn_many должны проходить.

**Ожидаемый эффект:** Ускорение spawn_many для 10000 entity с 3+ realloc'ов на колонку
до 1 realloc'а. Ускорение ~2-5× на batch spawn.

---

### 1.3 Оптимизация `CachedQuery` — кеширование `ids`

**Проблема:**  
[`CachedQuery::new()`](../crates/apex-core/src/world.rs:914) и все методы итерации
([`for_each`](../crates/apex-core/src/world.rs:934), [`for_each_component`](../crates/apex-core/src/world.rs:951),
[`par_for_each`](../crates/apex-core/src/world.rs:1008)) каждый раз выделяют `Vec`
и вызывают `Q::fill_ids()` — аллокация + поиск ComponentId в реестре.
Для частых запросов (каждый кадр) это избыточно.

**Решение:**  
Кешировать `Vec<ComponentId>` в структуре `CachedQuery` вместе с `arch_indices`.

**Конкретные изменения:**

1. В [`CachedQuery`](../crates/apex-core/src/world.rs:906-911) добавить поле:
```rust
ids: Vec<ComponentId>,
```

2. В [`new()`](../crates/apex-core/src/world.rs:914) сохранять `ids`:
```rust
pub fn new(world: &'w World, last_run: Tick) -> Self {
    let mut ids = Vec::with_capacity(Q::component_count());
    Q::fill_ids(world, &mut ids);
    // ...
    Self { world, arch_indices, ids, last_run, _phantom: std::marker::PhantomData }
}
```

3. В методах `for_each`, `for_each_component`, `par_for_each`, `par_for_each_component`
убрать повторные `fill_ids()` и использовать `self.ids`.

**Тесты:**  
Существующие тесты query/cached_query (через [`World`](../crates/apex-core/src/world.rs:617-625)).
Добавить тест на CachedQuery — проверить что после изменения компонентов query возвращает
корректные данные (кеш инвалидируется при изменении archetypes).

**Ожидаемый эффект:** Ускорение ~20-40ns на каждый query for_each (убираем аллокацию Vec
+ fill_ids). Для систем с 1000 entity/кадр: ~40 мкс экономии.

---

### 1.4 Исправление `Tick::is_newer_than` — защита от переполнения

**Проблема:**  
[`Tick::is_newer_than()`](../crates/apex-core/src/component.rs:14) использует `self.0 > last_run.0`.
После ~4 миллиардов тиков (wrapping u32) сравнение сломается: tick=1 будет считаться
"новее" чем last_run=4_000_000_000.

**Решение:**  
Использовать wrapping_sub со знаком как в Bevy:
```rust
pub fn is_newer_than(&self, last_run: Tick) -> bool {
    self.0.wrapping_sub(last_run.0) as i32 > 0
}
```

**Конкретные изменения:**

1. [`component.rs:14`](../crates/apex-core/src/component.rs:14):
```rust
pub fn is_newer_than(&self, last_run: Tick) -> bool {
    self.0.wrapping_sub(last_run.0) as i32 > 0
}
```

2. Аналогично проверить все места где сравниваются Tick'и:
   - [`Changed<T>::fetch_item`](../crates/apex-core/src/query.rs:199-206)
   - [`Column::get_tick`](../crates/apex-core/src/archetype.rs:178) — только чтение, ок
   - Поискать `Tick::ZERO`, `change_ticks` во всём коде

**Тесты:**  
Добавить unit тест для Tick:
```rust
#[test]
fn tick_wrapping_comparison() {
    let a = Tick(u32::MAX - 5);
    let b = Tick(3);  // после wrapping: 0,1,2,3
    assert!(b.is_newer_than(a));  // 3 новее чем MAX-5
    assert!(!a.is_newer_than(b));
}
```

**Ожидаемый эффект:** Предотвращение бага с change detection после ~50 часов работы
при 60 FPS (4e9 / (60*3600) ≈ 18500 часов, но для серверов с 1000 tick/сек — ~50 дней).

---

### 1.5 Исправление `par_for_each` — `Tick::ZERO` → `self.last_run`

**Проблема:**  
[`Query::par_for_each()`](../crates/apex-core/src/query.rs:408-440) использует
`Tick::ZERO` вместо `self.last_run` (строка 426 в вызове fetch_state).
То же в [`par_for_each_component()`](../crates/apex-core/src/query.rs:366-396) (строка 382).
Change detection не работает в параллельных запросах.

**Причина:** `par_for_each` берёт `&self` (shared ref), а `self.last_run` — поле `Query`.
В замыкании для rayon нужно скопировать `last_run` заранее.

**Решение:**

```rust
pub fn par_for_each<F>(&self, f: F)
where
    Q: Send,
    F: Fn(Entity, Q::Item<'_>) + Send + Sync,
{
    let last_run = self.last_run;  // <-- копируем ДО замыкания
    // ...
    let state = unsafe { Q::fetch_state(arch, &ids, last_run) };  // <-- используем
}
```

**Конкретные изменения:**

1. [`query.rs:419`](../crates/apex-core/src/query.rs:419) — `par_for_each`:
   - Перед `compute_par_chunks` скопировать `self.last_run` в локальную переменную
   - В замыкании `.par_iter().for_each(...)` использовать эту переменную вместо `Tick::ZERO`

2. [`query.rs:377`](../crates/apex-core/src/query.rs:377) — `par_for_each_component`:
   - Аналогичное изменение

**Тесты:**  
- Добавить тест на `Changed<T>` в параллельном запросе:
  1. Создать 10 entity с компонентом Position
  2. Изменить 5 из них
  3. Выполнить `query_changed::<Changed<Position>>().par_for_each(...)`
  4. Проверить что итерация видит только 5 изменённых

**Ожидаемый эффект:** Исправление бага — change detection теперь работает и в параллельных
запросах. Для систем, полагающихся на `Changed<T>` в `par_for_each`, критично.

---

### 1.6 Расширение `ComponentMask` до 256-bit

**Проблема:**
[`ComponentMask`](../crates/apex-core/src/access.rs:11-14) сейчас `{ lo: u64, hi: u64 }` — поддерживает
максимум 128 различных компонентов. При активном использовании relations (каждый relation вид
занимает слот в маске) лимит может быть быстро достигнут.

**Решение:**
Заменить на `pub struct ComponentMask(pub [u64; 4])` — 256 бит.

**Конкретные изменения:**

1. [`access.rs:11-14`](../crates/apex-core/src/access.rs:11):
```rust
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ComponentMask(pub [u64; 4]);
```

2. Метод [`set(&mut self, idx: u8)`](../crates/apex-core/src/access.rs:20-26):
```rust
pub fn set(&mut self, idx: u8) {
    if (idx as usize) < 256 {
        self.0[(idx / 64) as usize] |= 1u64 << (idx % 64);
    }
}
```

3. Метод [`get(&self, idx: u8) -> bool`](../crates/apex-core/src/access.rs:29-35):
```rust
pub fn get(&self, idx: u8) -> bool {
    if (idx as usize) < 256 {
        (self.0[(idx / 64) as usize] & (1u64 << (idx % 64))) != 0
    } else {
        false
    }
}
```

4. Метод [`overlaps`](../crates/apex-core/src/access.rs:38-44):
```rust
pub fn overlaps(&self, other: &Self) -> bool {
    (self.0[0] & other.0[0]) != 0
        || (self.0[1] & other.0[1]) != 0
        || (self.0[2] & other.0[2]) != 0
        || (self.0[3] & other.0[3]) != 0
}
```

5. Метод [`is_subset_of`](../crates/apex-core/src/access.rs:47-55):
```rust
pub fn is_subset_of(&self, other: &Self) -> bool {
    (self.0[0] & !other.0[0]) == 0
        && (self.0[1] & !other.0[1]) == 0
        && (self.0[2] & !other.0[2]) == 0
        && (self.0[3] & !other.0[3]) == 0
}
```

6. [`assign_masks`](../crates/apex-core/src/access.rs:218-227) — параметр `type_to_idx: &HashMap<TypeId, u8>`:
убедиться что маппинг поддерживает до 256 значений. Параметр уже `u8`, что даёт 0..255 — достаточно.

7. Все места где используется `mask.lo` / `mask.hi`:
```rust
// Было:
if mask.lo & other.lo != 0 || mask.hi & other.hi != 0 { ... }
// Стало:
for i in 0..4 {
    if (mask.0[i] & other.0[i]) != 0 { ... }
}
```

**Тесты:**
- unit тест ComponentMask: set/get для граничных значений (0, 127, 128, 255)
- test overlaps с разными комбинациями битов
- test is_subset_of
- Интеграционный: создать 200+ различных компонентов и проверить что маска работает

**Ожидаемый эффект:** Увеличение максимального числа компонентов со 128 до 256.
Небольшой overhead на регистры (4 u64 вместо 2), но практически без влияния на производительность.

---

### 1.7 Оптимизация `EventCursor` — FreeList для reader_id

**Проблема:**
[`TrackedEventQueue::add_reader()`](../crates/apex-core/src/events.rs:69-82) делает O(R) линейный
поиск `None` слотов в `cursors: Vec<Option<u32>>`.
[`all_readers_caught_up()`](../crates/apex-core/src/events.rs:234-240) тоже O(R) — итерирует все
слоты включая удалённые.

**Решение:**
Добавить FreeList (список свободных индексов) для переиспользования reader_id.

**Конкретные изменения:**

1. В [`TrackedEventQueue<T>`](../crates/apex-core/src/events.rs:33-42) добавить:
```rust
pub struct TrackedEventQueue<T> {
    events:    Vec<T>,
    cursors:   Vec<Option<u32>>,
    free_list: Vec<EventCursor>,  // <-- новый
}
```

2. [`add_reader()`](../crates/apex-core/src/events.rs:69):
```rust
pub fn add_reader(&mut self) -> EventCursor {
    if let Some(free) = self.free_list.pop() {
        let idx = free.0 as usize;
        self.cursors[idx] = Some(self.events.len() as u32);
        return free;
    }
    let id = EventCursor(self.cursors.len() as u32);
    self.cursors.push(Some(self.events.len() as u32));
    id
}
```

3. [`remove_reader()`](../crates/apex-core/src/events.rs:88-97):
```rust
pub fn remove_reader(&mut self, reader_id: EventCursor) {
    let idx = reader_id.0 as usize;
    if idx < self.cursors.len() {
        self.cursors[idx] = None;
        self.free_list.push(reader_id);  // <-- возвращаем в FreeList
    }
}
```

4. [`all_readers_caught_up()`](../crates/apex-core/src/events.rs:234):
   - Итерировать только `Some` слоты, либо хранить счётчик `active_readers`.

**Тесты:**
- Существующие тесты send_and_read, two_readers_independent, reader_removed_still_works
- Добавить тест: создать 1000 readers, удалить 500, создать ещё 500 — проверить что
  add_reader O(1) и free_list переиспользуется

**Ожидаемый эффект:** `add_reader()` — с O(R) до O(1) amortized.

---

### 1.8 Оптимизация поиска Archetype — избежать аллокации `Vec<ComponentId>` при lookup

**Проблема:**
[`get_or_create_archetype(components: Vec<ComponentId>)`](../crates/apex-core/src/world.rs:665-680)
принимает `Vec<ComponentId>` как ключ, каждый раз аллоцируя новую `Vec` при вызове.
Кроме того, [`find_or_create_archetype_with()`](../crates/apex-core/src/world.rs:629-645)
собирает `Vec` через clone + push + sort — 3 аллокации на один insert компонента.

**Решение:**
Использовать interned (кэшированные) ID для наборов компонентов, чтобы lookup
работал через `u64` или `&[ComponentId]` без аллокаций.

**Вариант A (лёгкий):** Изменить сигнатуру `get_or_create_archetype` на
`get_or_create_archetype(&mut self, components: &[ComponentId])` — принимает слайс,
внутри клонирует только при создании нового archetype (редкий случай).

**Вариант B (средний):** Добавить `ArchetypeKey` — хэш от набора ComponentId,
вычисляемый без аллокации:
```rust
fn archetype_key(components: &[ComponentId]) -> u64 {
    let mut state = FxHasher::default();
    for &id in components { id.hash(&mut state); }
    state.finish()
}
```

**Вариант C (продвинутый):** ArchetypeId interner — хранить `Vec<Vec<ComponentId>>`
и искать по хэшу + сравнению слайсов.

**Конкретные изменения (Вариант A — рекомендуется):**

1. [`world.rs:665-680`](../crates/apex-core/src/world.rs:665):
```rust
pub(crate) fn get_or_create_archetype(
    &mut self,
    components: &[ComponentId],
) -> ArchetypeId {
    // Используем &[ComponentId] для lookup
    if let Some(&id) = self.archetype_index.get(components) { return id; }
    let id = ArchetypeId(self.archetypes.len() as u32);
    let owned: Vec<ComponentId> = components.to_vec();
    let infos: Vec<&ComponentInfo> = components.iter()
        .filter_map(|&cid| self.registry.get_info(cid))
        .collect();
    let arch = Archetype::new(id, components.iter().copied().collect(), &infos);
    for &cid in &arch.component_ids { self.id_index.register_archetype(cid, id); }
    self.archetypes.push(arch);
    self.archetype_index.insert(owned, id);
    self.query_cache.invalidate();
    id
}
```

2. [`world.rs:629-645`](../crates/apex-core/src/world.rs:629):
```rust
pub(crate) fn find_or_create_archetype_with(
    &mut self,
    current: ArchetypeId,
    add: ComponentId,
) -> ArchetypeId {
    if let Some(&id) = self.archetypes[current.0 as usize].add_edges.get(&add) {
        return id;
    }
    // Используем SmallVec для избежания heap-аллокации для малых наборов
    let current_ids = &self.archetypes[current.0 as usize].component_ids;
    let mut new_components: SmallVec<[ComponentId; 8]> = current_ids.iter().copied().collect();
    new_components.push(add);
    new_components.sort_unstable();
    let new_id = self.get_or_create_archetype(&new_components);
    self.archetypes[current.0 as usize].add_edges.insert(add, new_id);
    self.archetypes[new_id.0 as usize].remove_edges.insert(add, current);
    new_id
}
```

3. [`world.rs:647-663`](../crates/apex-core/src/world.rs:647):
```rust
pub(crate) fn find_or_create_archetype_without(
    &mut self,
    current: ArchetypeId,
    remove: ComponentId,
) -> ArchetypeId {
    if let Some(&id) = self.archetypes[current.0 as usize].remove_edges.get(&remove) {
        return id;
    }
    let current_ids = &self.archetypes[current.0 as usize].component_ids;
    let new_components: SmallVec<[ComponentId; 8]> = current_ids.iter()
        .copied().filter(|&id| id != remove).collect();
    let new_id = self.get_or_create_archetype(&new_components);
    // ... остальное без изменений
}
```

4. Тип `archetype_index`: `FxHashMap<Vec<ComponentId>, ArchetypeId>` → `FxHashMap<SmallVec<[ComponentId; 8]>, ArchetypeId>`
   или лучше оставить `Vec<ComponentId>` для ключа, но изменить метод поиска.

**Тесты:**
- Существующие тесты spawn_bundle, insert, remove — должны проходить
- Добавить микро-тест на скорость: 10000 insert'ов компонентов, замерить
  количество аллокаций (через `#[cfg(debug_assertions)]` счётчик)

**Ожидаемый эффект:** Ускорение insert компонента на ~15-25ns за счёт
устранения 2-3 лишних heap-аллокаций на каждый structural change.

---

## Фаза 2: Средне-приоритетные оптимизации (Priority: Medium)

### 2.1 Оптимизация `EntityLocation` — компактное представление

**Проблема:**  
[`EntityLocation`](../crates/apex-core/src/entity.rs:20-23) занимает 16 байт
(`archetype_id: ArchetypeId(u32)` = 4б + выравнивание 4б + `row: usize` = 8б).

**Решение:**  
Использовать `archetype_id.0: u16` (максимум 65536 archetypes — более чем достаточно)
и `row: u32` (максимум 4 млрд entity в одном archetype). Итого 8 байт вместо 16.

**Конкретные изменения:**

1. [`entity.rs:20-23`](../crates/apex-core/src/entity.rs:20):
```rust
pub struct EntityLocation {
    pub archetype_id: ArchetypeId,  // ArchetypeId уже u32, ничего не меняем
    pub row: u32,                   // было: usize
}
```

2. [`EntityRecord`](../crates/apex-core/src/entity.rs:25-28):
```rust
struct EntityRecord {
    generation: u32,
    location: Option<EntityLocation>,  // EntityLocation теперь компактнее
}
```

3. Все места, где `location.row` используется как индекс массива (нужен каст `as usize`):
   - [`archetype.rs:262-266`](../crates/apex-core/src/archetype.rs:262) — `allocate_row`
   - [`archetype.rs:279-292`](../crates/apex-core/src/archetype.rs:279) — `remove_row`
   - [`world.rs:419-451`](../crates/apex-core/src/world.rs:419) — `insert`
   - [`world.rs:682-747`](../crates/apex-core/src/world.rs:682) — `move_entity`
   - и все остальные места использования `location.row`

   Везде добавить ` as usize` при обращении к массиву:
   ```rust
   self.archetypes[current_idx].columns[col_idx].write_at(location.row as usize, ...)
   ```

4. [`EntityAllocator::set_locations_batch`](../crates/apex-core/src/entity.rs:94-109) —
   параметр `start_row: usize` → `start_row: u32`.

**Тесты:**  
- Существующие 4 теста entity (allocate_free_reuse, allocate_batch_basic, allocate_batch_uses_free_list, set_locations_batch)
- Добавить тест на EntityLocation — проверить что row корректно конвертируется в usize.

**Ожидаемый эффект:** Уменьшение `EntityLocation` с 16 до 8 байт. В `EntityAllocator`
с 1M entity: экономия ~8 MB RAM. Ускорение за счёт cache locality при копировании
Location в batch-операциях.

---

### 2.2 Расширение `Command` enum — конкретные варианты вместо `Box<dyn FnOnce>`

**Проблема:**  
[`Command` enum](../crates/apex-core/src/commands.rs:13-19) содержит только
`Despawn(Entity)` и `Apply(Box<dyn FnOnce>)`. Все `spawn_bundle`, `insert`, `remove`
идут через `Apply(Box::new(...))` — heap аллокация + vtable dispatch.

**Решение:**  
Добавить конкретные варианты для часто используемых команд.

**Конкретные изменения:**

1. [`commands.rs:13-19`](../crates/apex-core/src/commands.rs:13):
```rust
enum Command {
    Spawn { bundle: BundleBox },
    Insert { entity: Entity, component: ComponentBox },
    Remove { entity: Entity, component_id: ComponentId },
    Despawn(Entity),
    SpawnFromTemplate { name: String, params: TemplateParams },
    Apply(Box<dyn FnOnce(&mut World) + Send>),
}
```

2. Определить трейты для type-erased bundle и component:
```rust
// В commands.rs или bundle.rs
pub trait ErasedBundle: Send {
    fn apply(self: Box<Self>, world: &mut World);
}
type BundleBox = Box<dyn ErasedBundle>;

pub trait ErasedComponent: Send {
    fn apply(self: Box<Self>, world: &mut World, entity: Entity);
}
type ComponentBox = Box<dyn ErasedComponent>;
```

3. В [`Commands::spawn_bundle`](../crates/apex-core/src/commands.rs:56-60):
```rust
pub fn spawn_bundle<B: Bundle + Send + 'static>(&mut self, bundle: B) {
    self.queue.push(Command::Spawn {
        bundle: Box::new(bundle),
    });
}
```

4. В [`Commands::insert`](../crates/apex-core/src/commands.rs:63-67):
```rust
pub fn insert<T: Component + Send + 'static>(&mut self, entity: Entity, component: T) {
    self.queue.push(Command::Insert {
        entity,
        component: Box::new(component),
    });
}
```

5. В [`Commands::apply`](../crates/apex-core/src/commands.rs:109-116):
```rust
match cmd {
    Command::Spawn { bundle } => { bundle.apply(world); }
    Command::Insert { entity, component } => { component.apply(world, entity); }
    Command::Remove { entity, component_id } => { world.remove_raw(entity, component_id); }
    Command::Despawn(entity) => { world.despawn(entity); }
    Command::SpawnFromTemplate { name, params } => { world.spawn_from_template(&name, &params); }
    Command::Apply(f) => { f(world); }
}
```

**Тесты:**  
- Добавить unit тест на Commands:
  1. spawn_bundle через Commands → проверить entity создана
  2. insert через Commands → проверить компонент добавлен
  3. remove через Commands → проверить компонент удалён
  4. despawn через Commands → проверить entity удалена

**Ожидаемый эффект:** Ускорение insert через Commands с ~75ns до ~20-30ns
(убираем динамическую диспетчеризацию для большинства случаев).

---

### 2.3 Кеширование `prepare_sub_worlds` между кадрами

**Проблема:**  
[`prepare_sub_worlds()`](../crates/apex-scheduler/src/lib.rs:1167-1185) вызывается каждый
кадр и клонирует `Vec<usize>` для каждой системы. Если archetypes не менялись,
это избыточно.

**Решение:**  
Кешировать `archetype_indices_storage` между вызовами run, инвалидировать только
при изменении archetypes.

**Конкретные изменения:**

1. В [`Scheduler`](../crates/apex-scheduler/src/lib.rs:282-329) добавить поля:
```rust
sub_worlds_ready: bool,
cached_arch_count_for_sub_worlds: usize,
```

2. В [`prepare_sub_worlds()`](../crates/apex-scheduler/src/lib.rs:1167):
```rust
fn prepare_sub_worlds(&mut self, world: &World) {
    let arch_count = world.archetypes().len();
    if self.sub_worlds_ready && arch_count == self.cached_arch_count_for_sub_worlds {
        return;  // Кеш актуален
    }
    // ... существующий код ...
    self.cached_arch_count_for_sub_worlds = arch_count;
    self.sub_worlds_ready = true;
}
```

3. Инвалидировать в [`add_new_nodes_and_edges()`](../crates/apex-scheduler/src/lib.rs:748)
и при добавлении/удалении систем.

4. Инвалидировать при изменении archetypes в World (вызывать из планировщика).

**Тесты:**  
- Тест: создать 2 системы, запустить run 2 раза, проверить что prepare_sub_worlds
  не пересчитывается на втором вызове (можно через debug_plan или счётчик).

**Ожидаемый эффект:** Экономия O(N_systems * N_archetypes) копирований памяти
на каждом кадре. Для 100 систем и 50 archetypes: клонирование 5000 usize = ~40 KB/кадр.

---

### 2.4 Обновление change ticks при Rhai write-back

**Проблема:**  
[`apply_deferred_resources_and_events()`](../crates/apex-scripting/src/context.rs:206-247)
записывает компоненты из скриптов, но не обновляет change ticks (не вызывает
`world.current_tick`).

**Решение:**  
При каждой записи компонента через ComponentBinding установить change_tick.

**Конкретные изменения:**

1. В [`context.rs`](../crates/apex-scripting/src/context.rs) — найти место записи компонента
(скорее всего в [`apply_deferred`](../crates/apex-scripting/src/context.rs:191-200)
или внутри [`apply_deferred_resources_and_events`](../crates/apex-scripting/src/context.rs:206)):

Добавить:
```rust
// При записи компонента:
let tick = world.current_tick;
// После записи данных:
if let Some(col_idx) = arch.column_index(component_id) {
    arch.columns[col_idx].change_ticks[row] = tick;
}
```

**Тесты:**  
- Создать Rhai скрипт, который изменяет компонент Position
- После выполнения скрипта проверить что `Changed<Position>` query видит изменения
- (Тест может быть в apex-scripting крейте или в интеграционном тесте с примером scripting.rs)

**Ожидаемый эффект:** Change detection работает корректно после модификации компонентов
из Rhai скриптов. Без этого изменения скрипты не триггерят системы с `Changed<T>`.

---

## Фаза 3: Долгосрочные оптимизации (Priority: Low/Long-term)

### 3.1 WorldDiff byte-level delta

**Проблема:**  
[`WorldDiff`](../crates/apex-serialization/src/snapshot.rs:224-271) хранит полные
снапшоты компонентов. Для частых сохранений это избыточно по размеру.

**Решение:**  
Добавить опциональное delta-кодирование: при сериализации сравнивать с предыдущим
снапшотом и сохранять только изменённые байты (XOR diff).

**Изменения:**
- В [`ComponentSnapshot`](../crates/apex-serialization/src/snapshot.rs:160-167)
  добавить поле `is_delta: bool`.
- При сериализации: если есть предыдущий snapshot, вычислить XOR diff.
- При десериализации: применить diff к предыдущему snapshot'у.

**Тесты:**  
- Расширить тесты snapshot_bincode_roundtrip и world_diff_bincode_roundtrip

---

### 3.2 Row-level параллельный SubWorld

**Проблема:**  
Текущий SubWorld работает на уровне archetype — все entity одного archetype'а
обрабатываются последовательно.

**Решение:**  
Подготовить инфраструктуру для row-level параллелизма:
- Chunk-based итерация (уже есть в par_for_each через `compute_par_chunks`)
- Разделение SubWorld на диапазоны строк

**Изменения:**
- В [`SubWorld`](../crates/apex-core/src/sub_world.rs:16-21) добавить row range
- В scheduler'е разбивать системы на sub-tasks для rayon

---

### 3.3 Rhai query caching

**Проблема:**  
Rhai скрипты выполняют запросы каждый кадр без кеширования, что приводит
к повторному созданию Query.

**Решение:**  
Добавить query cache в ScriptContext, инвалидируемый при изменении archetypes.

**Изменения:**
- В [`ScriptContext`](../crates/apex-scripting/src/context.rs:65-104)
  добавить `query_cache: FxHashMap<String, CachedQuery>`
- При запросе из скрипта: искать в кеше, если есть и не инвалидирован — использовать

---

## Фаза 4: Финальное тестирование

### 4.1 Запуск всех тестов

```bash
cargo test --workspace 2>&1
```

Убедиться, что все тесты проходят после всех изменений.

### 4.2 Бенчмарки

```bash
cargo bench 2>&1
```

Сравнить результаты с baseline (до оптимизаций).

### 4.3 Проверка примеров

```bash
cargo run --example basic
cargo run --example perf
cargo run --example scripting
```

---

## Порядок выполнения (dependency order)

```
Phase 0: Подготовка
  └── 0.1 Проверить тесты
      │
Phase 1 (критические):
  ├── 1.4 Tick overflow protection    ← нет зависимостей
  ├── 1.1 has_edge_between→HashSet    ← нет зависимостей
  ├── 1.2 Column::reserve             ← нет зависимостей
  ├── 1.6 ComponentMask 256-bit       ← нет зависимостей
  ├── 1.7 EventCursor FreeList        ← нет зависимостей
  ├── 1.8 Archetype lookup intern     ← нет зависимостей
  ├── 1.5 par_for_each Tick::ZERO     ← зависит от 1.4
  └── 1.3 CachedQuery cache ids       ← нет зависимостей
      │
Phase 2 (средние):
  ├── 2.1 EntityLocation compact      ← влияет на весь код, лучше после Phase 1
  ├── 2.2 Command enum expansion      ← нет зависимостей
  ├── 2.3 prepare_sub_worlds cache    ← зависит от 1.1
  └── 2.4 Rhai change ticks           ← нет зависимостей
      │
Phase 3 (долгосрочные):
  ├── 3.1 WorldDiff delta             ← нет зависимостей
  ├── 3.2 Row-level parallel          ← зависит от всех Phase 1-2
  └── 3.3 Rhai query caching          ← зависит от 1.3
```

---

## Краткая сводка изменений по файлам

| Файл | Изменения | Приоритет |
|------|-----------|-----------|
| `crates/apex-core/src/component.rs` | Tick::is_newer_than → wrapping_sub | Critical |
| `crates/apex-core/src/query.rs` | par_for_each Tick::ZERO → last_run | Critical |
| `crates/apex-core/src/archetype.rs` | Добавить Column::reserve(n) | Critical |
| `crates/apex-core/src/access.rs` | ComponentMask 256-bit [u64;4] | Critical |
| `crates/apex-core/src/events.rs` | EventCursor FreeList | Critical |
| `crates/apex-core/src/world.rs` | CachedQuery кешировать ids, spawn_many_inner reserve, Archetype lookup &[ComponentId] | Critical |
| `crates/apex-scheduler/src/lib.rs` | has_edge_between→HashSet, prepare_sub_worlds кеширование | Critical+Medium |
| `crates/apex-core/src/entity.rs` | EntityLocation row: u32 вместо usize | Medium |
| `crates/apex-core/src/commands.rs` | Конкретные варианты Command enum | Medium |
| `crates/apex-scripting/src/context.rs` | Change ticks при write-back | Medium |
| `crates/apex-serialization/src/snapshot.rs` | WorldDiff byte-level delta | Long-term |
| `crates/apex-core/src/sub_world.rs` | Row-level parallel | Long-term |
