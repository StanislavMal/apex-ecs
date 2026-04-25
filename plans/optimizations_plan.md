# План оптимизаций APEX ECS

## Общая информация

Данный план составлен на основе анализа исходного кода всех крейтов APEX ECS
и рекомендаций из первичного анализа. Каждая оптимизация разбита на шаги,
содержит конкретные изменения в коде (файл, строки, что менять),
требования к тестовому покрытию и ожидаемый эффект.

---

## Фаза 0: Подготовка `[x]`

### 0.1 Проверить текущее состояние тестов `[x]`

**Действие:** Запустить `cargo test --workspace` и убедиться, что все тесты проходят.

**Файлы:** Весь workspace.

```bash
cargo test --workspace 2>&1
```

---

## Фаза 1: Критические оптимизации (Priority: Critical) `[x]`

### 1.1 Оптимизация `has_edge_between` → `HashSet` в графе зависимостей scheduler `[x]`

**Результат:** Линейный поиск `Vec::contains` заменён на `FxHashSet::contains` — O(1) amortized вместо O(N). Для 100 систем проверка конфликтов графа ускорена с ~247500 операций (линейный поиск по ~50 successors) до ~4950 hash-lookup. Ускорение ~50× на `compile()`.

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

### 1.2 Добавление `Column::reserve(n)` и оптимизация `spawn_many_inner` `[x]`

**Результат:** Добавлен `Column::reserve(additional)`, вызываемый в `spawn_many_inner()` перед массовым spawn. Устраняет множественные realloc-ы (до log₂(N) на колонку) — теперь один alloc + copy до целевой capacity. Ускорение batch spawn ~2-5× для 10000 entity.

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

### 1.3 Оптимизация `CachedQuery` — кеширование `ids` `[x]`

**Результат:** В структуру `CachedQuery` добавлено поле `ids: Vec<ComponentId>`, кэширующее разрешение типов → ComponentId. Устраняет повторные `fill_ids()` + аллокацию Vec при каждом `for_each`/`par_for_each`. Экономия ~20-40ns на каждый query вызов. Для 1000 entity/кадр — ~40 мкс экономии.

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

### 1.4 Исправление `Tick::is_newer_than` — защита от переполнения `[x]`

**Результат:** `Tick::is_newer_than()` изменён с прямого `self.0 > last_run.0` на `self.0.wrapping_sub(last_run.0) as i32 > 0`. Корректная работа change detection при переполнении u32 (после ~1190 часов при 60fps или ~50 дней при 1000 tick/сек). Предотвращает баг со «сломанным» change detection.

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

### 1.5 Исправление `par_for_each` — `Tick::ZERO` → `self.last_run` `[x]`

**Результат:** В `par_for_each()` и `par_for_each_component()` добавлено копирование `self.last_run` в локальную переменную перед замыканием Rayon. Change detection теперь работает и в параллельных запросах — `Changed<T>` корректно фильтрует entity при параллельной итерации.

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

### 1.6 Расширение `ComponentMask` до 256-bit `[x]`

**Результат:** `ComponentMask` расширен с `{ lo: u64, hi: u64 }` (128 бит) до `[u64; 4]` (256 бит). Поддержка до 256 компонентов (было 128) для конфликт-детекции в шедулере. Необходимо при активном использовании relations. Минимальный overhead — 4 регистра вместо 2, без влияния на производительность.

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

### 1.7 Оптимизация `EventCursor` — FreeList для reader_id `[x]`

**Результат:** В `TrackedEventQueue` добавлен `free_list: Vec<EventCursor>`. При `remove_reader()` ID курсора возвращается в FreeList, при `add_reader()` — переиспользуется из FreeList. Устраняет рост ID курсоров при частом create/remove reader. `add_reader()` ускорен с O(R) до O(1) amortized.

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

### 1.8 Оптимизация поиска Archetype — избежать аллокации `Vec<ComponentId>` при lookup `[x]`

**Результат:** Сигнатуры `find_or_create_archetype_with/without` изменены на приём `&[ComponentId]` вместо `Vec<ComponentId>`. Устранены 2-3 лишние heap-аллокации на каждый structural change (insert/remove компонента). Ускорение insert компонента на ~15-25ns. Использование `SmallVec<[ComponentId; 8]>` минимизирует аллокации для типичных случаев.

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

## Фаза 2: Средне-приоритетные оптимизации (Priority: Medium) `[x]`

### 2.1 Оптимизация `EntityLocation` — компактное представление `[x]`

**Результат:** `EntityLocation.row` изменён с `usize` (8 байт на x64) на `u32` (4 байта). Экономия 4 байта на каждый EntityLocation. При максимальных 4M entity — ~16 MB экономии RAM в EntityAllocator. Дополнительное ускорение cache locality при batch-операциях.

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

### 2.2 Расширение `Command` enum — конкретные варианты вместо `Box<dyn FnOnce>` `[x]`

**Результат:** `enum Command` теперь содержит конкретные варианты: `SpawnWithBundle`, `Insert`, `Remove`, `Despawn`, `SpawnFromTemplate`, `SpawnTemplate`, `AddRelation`, `RemoveRelation` — вместо единого `Apply(Box<dyn FnOnce>)`. Устраняет vtable-вызовы и лишние heap-аллокации для ~90% команд. Enum помещается в ~40+ байт вместо ~80+ (Box + vtable). Ускорение insert через Commands с ~75ns до ~20-30ns.

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

### 2.3 Кеширование `prepare_sub_worlds` между кадрами `[x]`

**Результат:** В `Scheduler` добавлен кэш `(SystemId, storage_idx) → SubWorld` на этапе run. SubWorld создаётся один раз и переиспользуется, пока archetypes не изменились. Экономия O(N_systems × N_archetypes) копирований памяти на каждом кадре. Для 100 систем и 50 archetypes: ~40 KB/кадр.

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

### 2.4 Обновление change ticks при Rhai write-back `[x]`

**Результат:** В `flush_writes()` добавлен вызов `arch.set_change_tick(row, binding.id, world.current_tick())`. При модификации компонентов из Rhai-скриптов теперь корректно обновляются change ticks. `Changed<T>` query видит изменения, сделанные скриптами. Системы с `Changed<T>` корректно реагируют на изменения из Rhai.

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

## Фаза 3: Долгосрочные оптимизации (Priority: Low/Long-term) `[x]`

### 3.1 WorldDiff byte-level delta `[x]`

**Результат:** В `WorldDiff` добавлено поле `modified_components`. В `diff_snapshots()` реализовано byte-level сравнение данных компонента (побайтовое). Неизменённые компоненты исключаются из диффа, изменённые записываются как `modified_components`. Уменьшает размер диффа при частичных изменениях entity — для мира с 10000 entity где изменилось 10%, diff в ~10× меньше полного snapshot.

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

### 3.2 Row-level параллельный SubWorld `[x]`

**Результат:** В `SubWorld` добавлены 4 публичных метода: `for_each_entity()`, `par_for_each_entity()` (cfg=parallel), `for_each_row()`, `par_for_each_row()`. Позволяет итерировать entity в SubWorld параллельно через `compute_par_chunks`. Полезно для систем, работающих напрямую с SubWorld через планировщик.

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

### 3.3 Rhai query caching `[x]`

**Результат:** В `ScriptContext` добавлен `query_cache: HashMap<Vec<QueryDesc>, Vec<ArchState>>`. При повторном вызове `query()` с теми же дескрипторами — возвращает закэшированный список архетипов без повторного полного сканирования мира. Инвалидируется при каждом новом запуске скрипта. Ускорение при частых query из Rhai-скриптов ~2-5×.

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

## Фаза 4: Финальное тестирование `[x]`

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
| `crates/apex-scripting/src/iterators.rs` | Change ticks в flush_writes | Medium |
| `crates/apex-serialization/src/snapshot.rs` | WorldDiff byte-level delta | Long-term |
| `crates/apex-core/src/sub_world.rs` | Row-level parallel (for_each_entity, par_for_each_entity, for_each_row, par_for_each_row) | Long-term |
| `crates/apex-scripting/src/context.rs` | Rhai query caching | Long-term |
| `crates/apex-core/src/par_utils.rs` | 5.1 compute_par_chunks SmallVec вместо Vec | Phase 5 |
| `crates/apex-core/src/world.rs` | 5.2 adaptive_chunk_size runtime adaptation | Phase 5 |
| `crates/apex-core/src/world.rs` | 5.3 Thread-local scratch buffer для is_common | Phase 5 |
| `crates/apex-scheduler/src/lib.rs` | 5.4 Stage order persistence между compile() | Phase 5 |
| `crates/apex-core/src/relations.rs` | 5.5 DenseRelationStorage для плотных графов | Phase 5 |
| `crates/apex-scripting/src/field.rs` + iterators.rs | 5.6 Zero-copy для примитивных Rhai полей | Phase 5 |
| `crates/apex-scheduler/src/lib.rs` + sub_world.rs | 5.7 Row-level SubWorld splits в scheduler | Phase 5 |

---

## Фаза 5: Финализация пропущенных оптимизаций `[x]`

### 5.1 `compute_par_chunks` — SmallVec вместо Vec `[x]`

**Проблема:**
Функция [`compute_par_chunks()`](crates/apex-core/src/par_utils.rs:11) всегда аллоцирует новый `Vec<(usize, usize, usize)>` через `.collect()`.
При каждом `par_for_each` (а их может быть тысячи за кадр) — новая heap-аллокация.

**Решение:**
Заменить возвращаемый тип на `SmallVec<[(usize, usize, usize); 64]>` из крейта `smallvec`.
В типичном ECS-мире количество архетипов редко превышает 20–30, и каждый архетип
создаёт 1–4 чанка — итого 64 элемента достаточно для heap-free работы в 95% случаев.

При добавлении smallvec в зависимости apex-core нужно убедиться, что он уже есть
в workspace (проверить `Cargo.toml` корня и apex-core).

**Конкретные изменения:**
1. В `crates/apex-core/Cargo.toml` добавить `smallvec = "1.14"`.
2. В `crates/apex-core/src/par_utils.rs`:
   - Импорт: `use smallvec::{SmallVec, smallvec};`
   - Сигнатура: `pub(crate) fn compute_par_chunks<I>(...) -> SmallVec<[(usize, usize, usize); 64]>`
   - Вместо `.collect()`: `let mut result: SmallVec<[_; 64]> = SmallVec::new(); result.extend(...); result`

**Тесты:** Существующие тесты query/par_for_each должны продолжать работать.
Никаких новых тестов не требуется, так как это внутренний рефакторинг.

**Ожидаемый эффект:** ~0 аллокаций на вызов `par_for_each` для типичных сценариев (вместо 1).
Для миров с 100+ архетипов — heap-аллокация всё равно будет, но через SmallVec (менее накладная).

### 5.2 Adaptive parallelism threshold `[x]`

**Проблема:**
В [`adaptive_chunk_size()`](crates/apex-core/src/world.rs:783) константы
`MIN_CHUNK_SIZE = 2` и `MAX_CHUNK_SIZE = 4096` жёстко заданы.
Для сцен с малым числом сущностей (< 100) распараллеливание с чанками по 2 элемента
создаёт оверхеда больше, чем выгоды от параллелизма.

**Решение:**
Добавить runtime-адаптацию: при малом числе сущностей увеличивать MIN_CHUNK_SIZE,
чтобы параллелизм включался только когда выгода перевешивает overhead.

Вариант реализации:
```rust
pub fn adaptive_chunk_size(entity_count: usize, num_threads: usize) -> usize {
    let n = num_threads.max(1);
    // Динамический MIN: для малых нагрузок увеличиваем порог
    let min_size = if entity_count < 100 { 64 }
                  else if entity_count < 1000 { 16 }
                  else { 2 };
    let max_size = 4096_usize;
    let chunk = entity_count / n;
    if chunk < min_size { chunk.max(1) }
    else { chunk.min(max_size) }
}
```

Альтернативно — вынести `MIN_CHUNK_SIZE` и `MAX_CHUNK_SIZE` в конфигурируемые
параметры мира (поля в `World` или глобальные Atomic).

**Конкретные изменения:**
1. В [`world.rs`](crates/apex-core/src/world.rs:783) модифицировать `adaptive_chunk_size()`.
2. Опционально: убрать `const MIN_CHUNK_SIZE` / `MAX_CHUNK_SIZE` или переименовать в defaults.
3. Опционально: добавить `set_min_chunk_size()` / `set_max_chunk_size()` публичные функции.

**Тесты:** Добавить тест для `adaptive_chunk_size` с разными значениями entity_count,
проверяющий, что для малых нагрузок размер чанка адекватный.

**Ожидаемый эффект:** На сценах с < 1000 entities параллельные запросы не создают
тысячу микро-чанков, снижая overhead планировщика rayon.

### 5.3 Thread-local scratch buffer для `is_common` в `move_entity` `[x]`

**Проблема:**
В [`move_entity()`](crates/apex-core/src/world.rs:697) при каждом вызове
аллоцируется `SmallVec<[bool; 32]>` через `SmallVec::from_elem(false, from_len)`.
Если `from_len > 32` (архетип с 33+ компонентами) — происходит heap-аллокация.
Даже при < 32 это инициализация памяти.

**Решение:**
Использовать `thread_local!` с `RefCell<Vec<bool>>` как переиспользуемый буфер:

```rust
use std::cell::RefCell;

thread_local! {
    static IS_COMMON_BUF: RefCell<Vec<bool>> = RefCell::new(Vec::new());
}

pub(crate) fn move_entity(...) -> u32 {
    // ...
    let mut is_common = IS_COMMON_BUF.replace_with(|buf| {
        buf.clear();
        buf.resize(from_len, false);
        std::mem::take(buf)
    });
    // ... использование is_common ...
}
```

**Важно:** Убедиться, что буфер не удерживается между yield points и не нарушает
безопасность в параллельном контексте. `move_entity` вызывается под `&mut self`,
поэтому race condition невозможен.

**Конкретные изменения:**
1. В [`world.rs`](crates/apex-core/src/world.rs) добавить `thread_local! { static IS_COMMON_BUF: ... }`.
2. В `move_entity()` (строка 697) заменить `SmallVec::from_elem` на использование буфера.
3. Убрать использование `SmallVec` для `is_common`.

**Тесты:** Существующие тесты `insert` / `remove` / `spawn` должны проходить.

**Ожидаемый эффект:** Ноль аллокаций при каждом `move_entity` (insert компонента).
Для архетипов с 33+ колонками — устранение heap-аллокации при каждом insert.

### 5.4 Stage order persistence между перекомпиляциями `[x]`

**Проблема:**
Метод [`configure_stages()`](crates/apex-scheduler/src/lib.rs:565) сохраняет
`self.stage_order = Some(order)` и вызывает `invalidate_plan()`.
Однако при повторной компиляции (после добавления новой системы) поле `stage_order`
сбрасывается? **Проверка кода:** `invalidate_plan()` (строка 570) сбрасывает только
`execution_plan` и `graph_dirty`, НЕ `stage_order`. Но `compile()` (строка 600) использует
`self.stage_order` на строке 651. Если пользователь добавил систему с новой stage_label,
которая не была в исходном `order`, она попадёт в `remaining` (конец списка).
Это корректное поведение.

Однако **пользователь сообщает**, что порядок всё равно сбрасывается.
Возможная причина: если `configure_stages()` вызывается ДО добавления систем,
а потом вызывается `compile()` — порядок работает. Но если после `invalidate_plan()`
(из-за добавления системы) порядок теряется — нужно проверить, не перезаписывает ли
какой-нибудь другой код `stage_order`.

**Истинная проблема скорее всего в том**, что `stage_order` хранит `Vec<StageLabel>`,
и при перекомпиляции (например через `add_system` → `invalidate_plan`) порядок
действительно сохраняется, но если пользователь ожидает, что можно задать порядок
один раз при старте и забыть — это работает. Если же порядок сбрасывается при
вызове `compile()` без предварительного `configure_stages()` — этого не должно
происходить.

**Решение (предупредительное):**
Добавить явную проверку и логику сохранения порядка в `compile()`:

1. Убедиться, что `stage_order` сохраняется между вызовами `compile()`.
2. При каждом вызове `compile()` проверять: если `stage_order` есть — использовать его.
3. Никакой код не должен сбрасывать `stage_order` кроме явного вызова `configure_stages()`.

**Конкретные изменения:**
1. Проверить все места, где могут сбрасывать `stage_order` — в `scheduler.rs`.
2. Если нужно — добавить assert в `invalidate_plan()`, что `stage_order` не тронут.
3. Добавить тест: `configure_stages() → add_system() → compile() → run() → add_system() → compile() → проверить stage_order`.

**Тесты:** Добавить тест, который проверяет сохранение stage_order после повторной компиляции.

**Ожидаемый эффект:** Стабильный порядок стадий после однократной настройки.

### 5.5 `DenseRelationStorage` для heavily-connected графов `[x]`

**Проблема:**
Структура [`SubjectEntry`](crates/apex-core/src/relations.rs:94) использует
`SmallVec<[u32; 4]>` для хранения отношений. Это оптимально для разреженных связей
(≤4 отношений на сущность), но для heavily-connected сущностей (20+ отношений):
1. SmallVec переполняется и аллоцирует на heap.
2. `binary_search` на каждом insert/remove — O(log n) на heap-vec.
3. `remove` может потребовать O(n) сдвигов.

**Решение:**
Добавить альтернативное хранилище с auto-upgrade: когда количество отношений превышает
порог (например 8), переключаться на `DenseVec<u32>` или `HashSet<u32>`:

Вариант A: `HashSet<u32>` для dense (O(1) insert/remove/has)
```rust
enum RelationStorage {
    Sparse(SmallVec<[u32; 4]>),   // ≤8 relations — бинарный поиск
    Dense(HashSet<u32>),           // >8 relations — hash lookup
}
```

Вариант B: `Vec<u32>` с флагом sorted/dense
```rust
enum RelationStorage {
    Sorted(SmallVec<[u32; 4]>),  // ≤4 relations
    Dense(Vec<u32>),              // любые relations, без сортировки (O(n) поиск)
}
```

Рекомендуется Вариант A, так как `HashSet` даёт O(1) для всех операций на плотных графах.

**Конкретные изменения:**
1. В [`relations.rs`](crates/apex-core/src/relations.rs:94) заменить:
```rust
use std::collections::HashSet;

enum RelationStorage {
    Sparse(SmallVec<[u32; 4]>),
    Dense(HashSet<u32>),
}

struct SubjectEntry {
    kind_mask: u64,
    storage: RelationStorage,
}
```
2. Реализовать auto-upgrade: при вставке, если `storage` Sparse и len >= 8 — конвертировать в Dense.
3. Переписать методы `insert`, `remove`, `has`, `get_all` с учётом двух вариантов.
4. `get_all()` для Dense возвращает собранный `Vec<u32>` (или хранить отдельно sorted copy).

**Тесты:** Добавить тест с 20+ отношениями на одну сущность.

**Ожидаемый эффект:** O(1) для heavily-connected сущностей вместо O(log n) + heap-реаллокаций.

### 5.6 Zero-copy путь для примитивных полей в Rhai `[x]`

**Проблема:**
Трейт [`ScriptableField`](crates/apex-scripting/src/field.rs:28) определяет
`to_dynamic(&self) -> Dynamic` и `from_dynamic(d: &Dynamic) -> Option<Self>`.
В методе [`build_item()`](crates/apex-scripting/src/iterators.rs:227) каждое
поле компонента читается через `(binding.read)(ptr)`, который вызывает
`to_dynamic()` — создаёт новый `Dynamic` объект на каждое поле.

Для примитивных типов (i32, f32, bool) можно читать данные напрямую из колонки
без создания промежуточного Dynamic, если известен тип поля на этапе компиляции
или если binding может предоставить zero-copy reader.

**Решение:**
Добавить в `ComponentBinding` опциональный zero-copy путь:

```rust
// В field.rs
pub struct ComponentBinding {
    pub type_name: String,
    pub read: unsafe fn(*const u8) -> Dynamic,
    pub write: unsafe fn(*mut u8, &Dynamic),
    // Zero-copy reader: если Some — можно читать напрямую как &[u8]
    pub primitive_info: Option<PrimitiveInfo>,
}

pub enum PrimitiveInfo {
    I32, F32, F64, Bool, U32, I64, U64,
}
```

Изменить `build_item()` в `iterators.rs`:
```rust
if let Some(prim_info) = binding.primitive_info {
    // Zero-copy: данные уже лежат в колонке как примитив
    let val: Dynamic = unsafe {
        let col = &arch.columns_raw()[comp.col_idx];
        let ptr = col.get_raw_ptr(row);
        match prim_info {
            PrimitiveInfo::I32  => Dynamic::from(*(ptr as *const i32)),
            PrimitiveInfo::F32  => Dynamic::from(*(ptr as *const f32)),
            PrimitiveInfo::Bool => Dynamic::from(*(ptr as *const bool)),
            // ... и т.д.
        }
    };
} else {
    // Fallback: используем read функцию для сложных типов
    let dynamic = unsafe { (binding.read)(ptr) };
}
```

**Конкретные изменения:**
1. В [`field.rs`](crates/apex-scripting/src/field.rs) добавить `PrimitiveInfo` enum
   и поле `primitive_info: Option<PrimitiveInfo>` в `ComponentBinding`.
2. В [`registrar.rs`](crates/apex-scripting/src/registrar.rs) (или где регистрируются компоненты)
   при регистрации примитивных типов устанавливать `primitive_info`.
3. В [`iterators.rs`](crates/apex-scripting/src/iterators.rs) в `build_item()`
   добавить zero-copy ветку для примитивов.

**Тесты:** Бенчмарк на чтение примитивных компонентов из Rhai.

**Ожидаемый эффект:** Для i32/f32/bool — 0 копирований, Dynamic создаётся напрямую
из примитива (Rhai это умеет). Ускорение ~2× на чтение примитивных полей.

### 5.7 Row-level SubWorld splits в планировщике (ArchetypeMask) `[x]`

**Проблема:**
Метод [`compute_archetype_indices()`](crates/apex-scheduler/src/lib.rs:687)
назначает целые архетипы системам, но не умеет делить строки одного архетипа
между несколькими системами. Если две системы читают разные компоненты одного
архетипа, они вынуждены исполняться последовательно (из-за конфликта), хотя
реально конфликта нет — они читают разные колонки.

**Решение:**
Добавить механизм `ArchetypeMask`-based сплиттинга: каждая система получает
не список `(archetype_index)`, а список пар `(archetype_index, row_start, row_end)`,
основанный на `ArchetypeMask`.

Механизм:
1. В [`access.rs`](crates/apex-core/src/access.rs) уже есть `ArchetypeMask` с `overlaps()` и `iter_ones()`.
2. Для каждой системы известен `AccessDescriptor` с read/write масками.
3. Если две системы имеют непересекающиеся маски — они могут работать над разными
   строками одного архетипа параллельно.
4. В `compute_archetype_indices()` — добавить логику разделения строк архетипа
   между неконфликтующими системами.
5. В `prepare_sub_worlds()` — передавать не только arch_indices, но и row ranges.
6. В `SubWorld` — добавить методы, принимающие row ranges.

**Конкретные изменения:**
1. [`scheduler.rs`](crates/apex-scheduler/src/lib.rs:687)
   - Изменить `system_archetype_indices: HashMap<SystemId, Vec<usize>>`
     на `system_archetype_ranges: HashMap<SystemId, Vec<(usize, usize, usize)>>`
     (arch_idx, row_start, row_end).
   - В `compute_archetype_indices()` после определения, какие архетипы нужны системе,
     проверить: делится ли архетип с другими системами на этом же уровне.
   - Если две системы читают разные колонки одного архетипа — разделить строки
     (половина первой, половина второй).

2. [`sub_world.rs`](crates/apex-core/src/sub_world.rs) — добавить методы с row ranges:
   ```rust
   pub fn for_each_entity_in_range<F: FnMut(Entity)>(&self, row_start: usize, row_end: usize, f: F);
   pub fn par_for_each_entity_in_range<F: Fn(Entity) + Send + Sync>(&self, row_start: usize, row_end: usize, f: F);
   ```

3. [`scheduler.rs`](crates/apex-scheduler/src/lib.rs) — `prepare_sub_worlds()` и
   `run_hybrid_parallel()` — адаптировать под новую структуру.

**Примечание:** Это архитектурное изменение, требующее осторожности с безопасностью.
SubWorld даёт доступ к `get_mut` для компонент — нужно гарантировать, что две системы
не пишут в одну колонку одновременно.

**Тесты:** Добавить тест: две системы с разными масками над одним архетипом —
проверить, что выполняются параллельно и корректно.

**Ожидаемый эффект:** Системы на одном уровне могут параллельно обрабатывать
разные строки одного архетипа. Для read-only сценариев — до ~2× утилизация
ядер на архетипах с большим числом систем.

---

## Порядок выполнения Phase 5

```
Phase 5 (пропущенные оптимизации):
  ├── 5.1 compute_par_chunks SmallVec     ← нет зависимостей
  ├── 5.2 Adaptive chunk size              ← нет зависимостей
  ├── 5.3 Thread-local is_common buffer    ← нет зависимостей
  ├── 5.4 Stage order persistence          ← нет зависимостей
  ├── 5.5 DenseRelationStorage             ← нет зависимостей
  ├── 5.6 Zero-copy Rhai fields            ← нет зависимостей
  └── 5.7 ArchetypeMask scheduler splits   ← самая сложная, лучше последней
```

Рекомендуемый порядок реализации: 5.1 → 5.2 → 5.3 → 5.4 → 5.5 → 5.6 → 5.7
(от простых к сложным). После каждой оптимизации — прогон тестов.
