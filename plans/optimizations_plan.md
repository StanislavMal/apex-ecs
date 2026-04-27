# APEX ECS — План оптимизации

---

## Содержание

1. [Обзор и фазы](#1-обзор-и-фазы)
2. [Фаза 1 — Быстрые победы (без ломки API)](#2-фаза-1--быстрые-победы)
3. [Фаза 2 — Структурные оптимизации ядра](#3-фаза-2--структурные-оптимизации-ядра)
4. [Фаза 3 — Параллелизм нового уровня](#4-фаза-3--параллелизм-нового-уровня)
5. [Фаза 4 — Функциональные доработки](#5-фаза-4--функциональные-доработки)
6. [Метрики успеха](#6-метрики-успеха)
7. [Порядок реализации и зависимости](#7-порядок-реализации-и-зависимости)

---

## 1. Обзор и фазы

### Текущие узкие места (из бенчмарков)

| Проблема | Текущий результат | Цель |
|---|---|---|
| Межсистемный параллелизм (12 систем) | 3.91x speedup ✅ (ASD) | 7x–9x speedup |
| CPU-bound 2 системы (изол. архетипы) | 1.33x speedup ✅ (ASD) | 1.8x–2.0x speedup |
| CPU-bound 2 системы (shared arch) | 1.10x speedup ✅ (ASD, +206%) | 1.8x–2.0x speedup |
| Pipeline sequential barrier | 0.92x speedup ✅ (ASD, +163%) | 1.8x–2.0x speedup |
| `insert component` | 72 ns/op | 45–55 ns/op |
| `spawn_bundle loop` | 105 ns/op | уже OK, цель — batch |
| `compile()` при N=50 | 110 800 ns | < 30 000 ns |

### Принципы изменений

- **API stability first.** Фазы 1–2 не меняют публичный API.
- **Измеряемый прогресс.** Каждая задача сопровождается конкретным бенчмарком.
- **Атомарность.** Каждый пункт — отдельный PR/commit с тестами.

---

## 2. Фаза 1 — Быстрые победы

---

### 1.1. Линейный поиск в `write_into_batch` → позиционный индекс

**Файл:** `crates/apex-core/src/world.rs`  
**Функция:** макрос `impl_bundle!`, метод `write_into_batch`  
**Выигрыш:** 15–25% ускорение batch-спавна при 3+ компонентах

**Проблема:**

```rust
// Текущий код — линейный поиск O(K) на каждый компонент каждой entity:
if let Some(&(_cid, col_idx)) = col_indices.iter().find(|&&(id, _)| id == cid)
```

При `spawn_many(10_000, |i| (A, B, C, D))` это 10 000 × 4 = 40 000 вызовов `find()`, каждый сканирует до 4 элементов.

**Решение:** Вычислить `col_idx` один раз в `spawn_many_inner` и передать напрямую как позиционный массив.

**Изменение в `world.rs`:**

```rust
// Заменить структуру col_indices:
// Было:
let col_indices: Vec<(ComponentId, usize)> = ids.iter()
    .filter_map(|&id| {
        self.archetypes[arch_idx].column_index(id).map(|col_idx| (id, col_idx))
    })
    .collect();

// Стало — храним только col_idx в порядке компонентов бандла:
let col_indices: SmallVec<[usize; 8]> = ids.iter()
    .filter_map(|&id| self.archetypes[arch_idx].column_index(id))
    .collect();
```

**Изменение в макросе `impl_bundle!`:**

```rust
// Было в write_into_batch:
fn write_into_batch(self, world, archetype_id, row, tick, col_indices: &[(ComponentId, usize)]) {
    let ($($T,)+) = self;
    $(
        if let Some(cid) = world.registry.get_id::<$T>() {
            if let Some(&(_cid, col_idx)) = col_indices.iter().find(|&&(id, _)| id == cid) {
                // ...
            }
        }
    )+
}

// Стало — enumerate + позиционный доступ:
fn write_into_batch(self, world, archetype_id, row, tick, col_indices: &[usize]) {
    let ($($T,)+) = self;
    let mut i = 0;
    $(
        {
            let col_idx = col_indices[i];
            i += 1;
            unsafe {
                let col = &mut world.archetypes[archetype_id.0 as usize].columns[col_idx];
                if col.item_size > 0 {
                    if col.len >= col.capacity { col.grow(); }
                    std::ptr::copy_nonoverlapping(
                        &$T as *const $T as *const u8,
                        col.get_ptr(row),
                        col.item_size,
                    );
                }
                col.change_ticks.push(tick);
                col.len += 1;
            }
            std::mem::forget($T);
        }
    )+
}
```

**Тест:** `cargo bench --bench benchmark simple_insert` — ожидаемое улучшение для `spawn_many`.

✅ **Реализовано 2026-04-26:**
- `Bundle::write_into_batch` сигнатура: `&[(ComponentId, usize)]` → `&[usize]` (прямой позиционный доступ)
- Макрос `impl_bundle!`: убран `find()` с линейным поиском O(K), заменён на `col_indices[i]` со счётчиком `i`
- `spawn_many_inner`: `Vec<(ComponentId, usize)>` → `SmallVec<[usize; 8]>`, хранение только column index
- Bulk-copy блок: `for &(_cid, col_idx)` → `for &col_idx`
- Верификация: `cargo test --workspace` (28 passed), `cargo run --example perf --features parallel` (без регрессий)
- Устранено 40,000 `find()` вызовов при spawn_many(10_000, (A,B,C,D))

---

### 1.2. SmallVec ключ для `archetype_index` — устранение Vec-аллокаций

**Файл:** `crates/apex-core/src/world.rs`  
**Структура:** `World.archetype_index`  
**Выигрыш:** 5–15% ускорение всех операций с архетипами (insert, remove, spawn_bundle)

**Проблема:**

```rust
pub(crate) archetype_index: FxHashMap<Vec<ComponentId>, ArchetypeId>,
```

При каждом `get_or_create_archetype(&components)`:
- `find_or_create_archetype_with` вызывает `get_or_create_archetype`
- Внутри `self.archetype_index.get(components)` — `components` уже `&[ComponentId]`
- Но ключ `Vec<ComponentId>` требует сравнения через `PartialEq` с внутренним Vec

**Решение:** Создать newtype-обёртку, реализующую `Hash` и `Eq` через содержимое среза без аллокации:

```rust
// Добавить в world.rs:

/// Ключ для archetype_index — хэшируется без аллокации.
/// Внутри хранит компоненты inline до 12 штук.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ArchetypeKey(SmallVec<[ComponentId; 12]>);

impl From<&[ComponentId]> for ArchetypeKey {
    fn from(ids: &[ComponentId]) -> Self {
        Self(ids.iter().copied().collect())
    }
}

// Обновить поле:
pub(crate) archetype_index: FxHashMap<ArchetypeKey, ArchetypeId>,

// Обновить все обращения (3 места):
self.archetype_index.get(&ArchetypeKey::from(components))
self.archetype_index.insert(ArchetypeKey::from(components), id)
```

**Дополнительно:** В `get_or_create_archetype` убрать `.to_vec()`:

```rust
// Было:
self.archetype_index.insert(components.to_vec(), id);

// Стало:
self.archetype_index.insert(ArchetypeKey::from(components), id);
```

**Тест:** Профилировать `insert component` бенчмарк — должно сократиться количество аллокаций.

✅ **Реализовано 2026-04-26:**
- Создан `ArchetypeKey(SmallVec<[ComponentId; 12]>)` newtype с `Clone, PartialEq, Eq, Hash` и `From<&[ComponentId]>`
- Поле `archetype_index`: `FxHashMap<Vec<ComponentId>, _>` → `FxHashMap<ArchetypeKey, _>`
- `World::new()`: `insert(Vec::new(),` → `insert(ArchetypeKey(SmallVec::new()),`
- `get_or_create_archetype`: `get(components)` → `get(&ArchetypeKey::from(components))` — без `.to_vec()` аллокации
- `get_or_create_archetype`: `insert(components.to_vec(),` → `insert(ArchetypeKey::from(components),` — устранена heap-аллокация
- Верификация: `cargo test --workspace` (28 passed), `cargo run --example perf --features parallel` (без регрессий)

---

### 1.3. Однопроходный `move_entity` — убрать двойной цикл

**Файл:** `crates/apex-core/src/world.rs`  
**Функция:** `World::move_entity`  
**Выигрыш:** 10–20% ускорение `insert`/`remove`/`add_relation`

**Проблема:** Три прохода по колонкам:
1. Вычисление `is_common[]`
2. Копирование данных в целевой архетип
3. `swap_remove` из исходного архетипа

```rust
// Текущий код упрощённо:
let mut is_common = vec![false; from_len];
for i in 0..from_len { is_common[i] = ...; }  // проход 1
for i in 0..from_len { if is_common[i] { /* copy */ } }  // проход 2
for col in columns { col.swap_remove... }  // проход 3 через iter_mut
```

**Решение — объединить все три прохода:**

```rust
pub(crate) fn move_entity(
    &mut self,
    entity: Entity,
    from_location: EntityLocation,
    to_archetype_id: ArchetypeId,
) -> u32 {
    self.query_cache.invalidate();

    let from_idx = from_location.archetype_id.0 as usize;
    let to_idx   = to_archetype_id.0 as usize;
    let from_row = from_location.row as usize;

    let to_row = self.archetypes[to_idx].entities.len();
    self.archetypes[to_idx].entities.push(entity);

    // Единственный проход: для каждой колонки из исходного архетипа
    // определяем наличие в целевом и сразу копируем или дропаем.
    let from_len = self.archetypes[from_idx].columns.len();

    for i in 0..from_len {
        let cid       = self.archetypes[from_idx].columns[i].component_id;
        let item_size = self.archetypes[from_idx].columns[i].item_size;

        if let Some(to_col_idx) = self.archetypes[to_idx].column_index(cid) {
            // Компонент присутствует в обоих архетипах — копируем
            unsafe {
                if item_size > 0 {
                    if self.archetypes[to_idx].columns[to_col_idx].len
                        >= self.archetypes[to_idx].columns[to_col_idx].capacity
                    {
                        self.archetypes[to_idx].columns[to_col_idx].grow();
                    }
                    let src = self.archetypes[from_idx].columns[i].get_ptr(from_row);
                    let dst = self.archetypes[to_idx].columns[to_col_idx].get_ptr(to_row);
                    std::ptr::copy_nonoverlapping(src, dst, item_size);
                }
                let tick = self.archetypes[from_idx].columns[i].get_tick(from_row);
                self.archetypes[to_idx].columns[to_col_idx].change_ticks.push(tick);
                self.archetypes[to_idx].columns[to_col_idx].len += 1;

                // swap_remove без drop (данные перемещены)
                self.archetypes[from_idx].columns[i].swap_remove_no_drop(from_row);
            }
        } else {
            // Компонент отсутствует в целевом — дропаем
            unsafe {
                self.archetypes[from_idx].columns[i].swap_remove_and_drop(from_row);
            }
        }
    }

    // Исправляем location для вытесненной entity
    unsafe {
        let from_last = self.archetypes[from_idx].entities.len() - 1;
        if from_row != from_last {
            let displaced = self.archetypes[from_idx].entities[from_last];
            self.archetypes[from_idx].entities.swap(from_row, from_last);
            self.archetypes[from_idx].entities.pop();
            self.entities.set_location(displaced, EntityLocation {
                archetype_id: from_location.archetype_id,
                row: from_row as u32,
            });
        } else {
            self.archetypes[from_idx].entities.pop();
        }
    }

    // IS_COMMON_BUF больше не нужен — убрать thread_local
    to_row as u32
}
```

**Дополнительно:** Удалить `IS_COMMON_BUF` thread_local и связанный код — он больше не нужен.

**Тест:** `cargo bench structural` — метрика `insert component`.

> **✅ Реализовано (2026-04-26).**
> `IS_COMMON_BUF` удалён. `move_entity` переписан в однопроходный вариант:
> единый `for i in 0..from_len` с `column_index(cid)` inline-проверкой.
> Верификация: `cargo test --workspace` (28 passed), `cargo run --example perf --features parallel` (без ошибок).

---

### 1.4. O(N) обнаружение sequential-барьеров в Scheduler

**Файл:** `crates/apex-scheduler/src/lib.rs`  
**Функция:** `Scheduler::add_new_nodes_and_edges`  
**Выигрыш:** compile() при 50 системах: 110 800 ns → < 30 000 ns

**Проблема:** При добавлении sequential системы код делает O(N) проходов по всем системам для расстановки барьеров, а внутри каждого — `has_path()` (BFS). Итого O(N²·V) в худшем случае.

```rust
// Текущий код — для каждой новой системы:
for &idx in &systems_to_process {
    if !system.kind.is_parallel() {
        for j in 0..n {   // O(N)
            if self.systems[j].kind.is_parallel() {
                // has_path() — O(V+E) BFS
            }
        }
    }
}
```

**Решение:** Разделить системы на два списка и добавлять барьеры точечно:

```rust
// Добавить в структуру Scheduler:
seq_system_indices: Vec<usize>,   // индексы sequential систем в self.systems
par_system_indices: Vec<usize>,   // индексы parallel систем

// В add_new_nodes_and_edges — вместо двойного цикла:

// При добавлении новой sequential системы:
// Добавить рёбра: каждый par ПЕРЕД seq → par→seq
// Добавить рёбра: каждый par ПОСЛЕ seq → seq→par
// Это O(P) вместо O(N) где P = число par-систем

fn add_sequential_barriers_for(&mut self, seq_idx: usize) {
    let seq_id   = self.systems[seq_idx].id;
    let seq_node = self.graph_nodes[&seq_id];

    for &par_idx in &self.par_system_indices {
        let par_id   = self.systems[par_idx].id;
        let par_node = self.graph_nodes[&par_id];

        if par_idx < seq_idx {
            // par → seq
            if !self.has_edge_between(par_node, seq_node)
               && !self.dependency_graph.has_path(seq_node, par_node)
            {
                self.add_barrier_edge(par_node, seq_node, par_id, seq_id);
            }
        } else {
            // seq → par
            if !self.has_edge_between(seq_node, par_node)
               && !self.dependency_graph.has_path(par_node, seq_node)
            {
                self.add_barrier_edge(seq_node, par_node, seq_id, par_id);
            }
        }
    }
}
```

**Тест:** `cargo bench compile_overhead` — секция "Фиксированный N" при N=50.

> **✅ Реализовано (2026-04-26).**
> В `Scheduler` добавлены поля `seq_system_indices: Vec<usize>` и `par_system_indices: Vec<usize>`.
> Все 4 метода регистрации (`add_system_to_stage`, `add_auto_system_to_stage`, `add_par_system_to_stage`, `add_fn_par_system_to_stage`)
> push'ат индекс в соответствующий список.
> В `add_new_nodes_and_edges` секция 3 заменена с O(N²) `for j in 0..n` на итерацию по
> `par_system_indices` (для sequential) / `seq_system_indices` (для parallel) — O(P)/O(S).
> Верификация: `cargo test --workspace` (28 passed), `cargo run --example perf --features parallel` (без ошибок).

---

### 1.5. Кеш EventRegistry — убрать `downcast_ref` из горячего пути

**Файл:** `crates/apex-core/src/events.rs`  
**Структура:** `EventRegistry`  
**Выигрыш:** ~30% ускорение `send_event` / `events::<T>()` в горячем пути

**Проблема:**

```rust
pub fn get<T>(&self) -> &TrackedEventQueue<T> {
    self.queues.get(&TypeId::of::<T>())
        .and_then(|b| b.as_any().downcast_ref::<TrackedEventQueue<T>>())  // vtable call
        .unwrap()
}
```

**Решение:** Добавить параллельную карту с raw-указателями без виртуального вызова:

```rust
pub struct EventRegistry {
    queues:    FxHashMap<TypeId, Box<dyn AnyEventQueue>>,
    // Дополнительная карта для O(1) typed access без downcast:
    raw_ptrs:  FxHashMap<TypeId, *mut u8>,
}

impl EventRegistry {
    pub fn register<T: Send + Sync + 'static>(&mut self) {
        let entry = self.queues.entry(TypeId::of::<T>()).or_insert_with(|| {
            Box::new(TrackedEventQueue::<T>::new())
        });
        // Сохраняем raw ptr для быстрого доступа
        let raw_ptr = entry.as_ptr_mut();
        self.raw_ptrs.insert(TypeId::of::<T>(), raw_ptr);
    }

    /// Zero-cost typed access — никакого downcast.
    #[inline]
    pub fn get_typed_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut TrackedEventQueue<T>> {
        self.raw_ptrs.get(&TypeId::of::<T>())
            .map(|&ptr| ptr as *mut TrackedEventQueue<T>)
    }

    #[inline]
    pub fn get<T: Send + Sync + 'static>(&self) -> &TrackedEventQueue<T> {
        // SAFETY: ptr валиден пока Box жив (жив пока EventRegistry жив)
        unsafe {
            &*self.get_typed_ptr::<T>()
                .expect("Event not registered")
        }
    }
}
```

**Тест:** `cargo bench events` — метрика `send + iter_current`.

> **✅ Реализовано (2026-04-26).**
> В `EventRegistry` добавлено поле `raw_ptrs: FxHashMap<TypeId, SyncPtr>`.
> Wrapper `SyncPtr(*mut u8)` с `unsafe impl Send + Sync` для совместимости с `&World` в `par_iter()`.
> `register::<T>()` сохраняет указатель на данные `Box` (куча — не перемещается при реаллокации HashMap).
> `get::<T>()`, `get_mut::<T>()`, `try_get::<T>()`, `try_get_mut::<T>()`, `get_raw_ptr::<T>()`
> читают из `raw_ptrs` — zero-cost доступ без `downcast_ref` / vtable call.
> Верификация: `cargo test --workspace` (28 passed), `cargo run --example perf --features parallel` (без ошибок).

---

## 3. Фаза 2 — Структурные оптимизации ядра


---

### 2.1. ArchetypeMask в Query matching — O(1) фильтрация архетипов

**Файлы:** `crates/apex-core/src/access.rs`, `crates/apex-core/src/query.rs`, `crates/apex-core/src/world.rs`  
**Выигрыш:** При большом мире (500+ архетипов) — 40–60% ускорение `Query::new` и `CachedQuery`

**Контекст:** `ArchetypeMask` (128 байт, до 1024 архетипов) уже реализован в `access.rs`. `AccessDescriptor` имеет поле `archetype_mask`. Но `Query::new` делает линейный обход ВСЕХ архетипов:

```rust
// Текущий код в query.rs:
world.archetypes
    .iter()
    .enumerate()
    .filter(|(_, arch)| !arch.is_empty() && Q::matches_archetype(arch, &ids))
    .map(|(arch_idx, arch)| { ... })
    .collect()
```

**Шаг 1:** Добавить в `World` индекс компонент → архетипы (аналог `id_index` для relations):

```rust
// В world.rs — уже есть IdIndex для relations, сделать аналогичный для компонентов:
pub(crate) component_arch_index: FxHashMap<ComponentId, SmallVec<[ArchetypeId; 16]>>,

// Заполнять в get_or_create_archetype:
for &cid in &arch.component_ids {
    self.component_arch_index
        .entry(cid)
        .or_default()
        .push(id);
}
```

**Шаг 2:** В `Query::new` использовать пересечение списков, а не обход всех архетипов:

```rust
pub fn new_with_tick(world: &'w World, last_run: Tick) -> Self {
    let mut ids = Vec::with_capacity(Q::component_count());
    Q::fill_ids(world, &mut ids);

    // Находим архетипы для НАИМЕНЕЕ РАСПРОСТРАНЁННОГО компонента (smallest set)
    // и затем фильтруем по остальным компонентам:
    let candidate_archetypes = if ids.is_empty() {
        // Запрос без компонентов — все архетипы
        (0..world.archetypes.len()).collect::<Vec<_>>()
    } else {
        // Берём компонент с минимальным числом архетипов
        let smallest = ids.iter()
            .filter_map(|id| world.component_arch_index.get(id))
            .min_by_key(|v| v.len());

        match smallest {
            Some(arch_ids) => arch_ids.iter()
                .map(|id| id.0 as usize)
                .collect(),
            None => return Self { world, archetypes: vec![], last_run },
        }
    };

    let archetypes = candidate_archetypes.into_iter()
        .filter(|&arch_idx| {
            let arch = &world.archetypes[arch_idx];
            !arch.is_empty() && Q::matches_archetype(arch, &ids)
        })
        .map(|arch_idx| {
            let arch = &world.archetypes[arch_idx];
            let state = unsafe { Q::fetch_state(arch, &ids, last_run) };
            ArchState { arch_idx, state, len: arch.len() }
        })
        .collect();

    Self { world, archetypes, last_run }
}
```

**Тест:** Создать мир с 1000 архетипами и 10 тысячами entity, измерить `Query::new` время.

✅ **Реализовано 2026-04-26:**
- В `World` добавлено поле `component_arch_index: FxHashMap<ComponentId, SmallVec<[ArchetypeId; 16]>>`
- Заполняется в `get_or_create_archetype` вместе с `id_index.register_archetype`
- `Query::new_with_tick` переписан: находит компонент с минимальным числом архетипов (`min_by_key`), итерирует только его архетипы O(K) вместо O(N)
- Edge case: пустой список компонентов → полный перебор всех архетипов
- Edge case: компонент не найден ни в одном архетипе → пустой результат
- Верификация: `cargo test --workspace` (28 passed)

---

### 2.2. Chunk-based CommandQueue — устранение Box-аллокаций

**Файл:** `crates/apex-core/src/commands.rs`  
**Выигрыш:** При 10k despawn-команд: -10k heap-аллокаций, ~25% ускорение `Commands::apply`

**Проблема:** Каждая команда `Spawn` или `Insert` выделяет `Box<dyn ErasedBundle>` / `Box<dyn ErasedComponent>`.

**Решение — сначала только для `Despawn` (90% use-case):**

`Despawn` уже хранится inline в `Command::Despawn(Entity)` — это правильно. Проблема только в `Spawn` и `Insert`. Для них добавить typed batch-variant:

```rust
// Новый вариант команды для типизированного batch-спавна:
enum Command {
    Despawn(Entity),                          // inline, без аллокации (уже есть)
    Insert { entity: Entity, component: ComponentBox },
    Remove { entity: Entity, component_id: ComponentId },
    SpawnFromTemplate { name: String, params: TemplateParams },
    Apply(Box<dyn FnOnce(&mut World) + Send>),

    // НОВОЕ: типизированный batch-spawn без Box<dyn Trait>
    SpawnTyped(Box<dyn TypedSpawnCommand>),
}

pub trait TypedSpawnCommand: Send {
    fn apply(self: Box<Self>, world: &mut World);
    fn estimated_size(&self) -> usize { 1 }
}

// Реализация для конкретного Bundle:
struct SpawnBundleCommand<B: Bundle + Send + 'static> {
    bundle: B,
}

impl<B: Bundle + Send + 'static> TypedSpawnCommand for SpawnBundleCommand<B> {
    fn apply(self: Box<Self>, world: &mut World) {
        world.spawn_bundle(self.bundle);
    }
}

// Публичный API не меняется:
pub fn spawn_bundle<B: Bundle + Send + 'static>(&mut self, bundle: B) {
    self.queue.push(Command::SpawnTyped(Box::new(SpawnBundleCommand { bundle })));
}
```

**Следующий шаг (опционально):** Заменить `Vec<Command>` на bump-allocator буфер. Это сложнее, отложить на фазу 3.

---

### 2.3. Гранулярная инвалидация QueryCache

**Файл:** `crates/apex-core/src/world.rs`  
**Структура:** `QueryCache`  
**Выигрыш:** При частых `insert`/`remove` одного типа компонента — кеш перестаёт быть бесполезным

**Проблема:** `query_cache.invalidate()` сбрасывает ВСЕ кешированные запросы при любом структурном изменении, даже если изменился несвязанный компонент.

**Решение:** Инвалидировать только те записи кеша, которые содержат изменённый `ComponentId`:

```rust
pub(crate) struct QueryCache {
    entries:  UnsafeCell<FxHashMap<Vec<ComponentId>, CacheEntry>>,
    version:  u32,
}

impl QueryCache {
    /// Инвалидировать только записи, затрагивающие данный компонент.
    pub fn invalidate_for(&mut self, changed_cid: ComponentId) {
        let map = unsafe { &mut *self.entries.get() };
        // Удаляем только те ключи, которые содержат changed_cid:
        map.retain(|key, _| !key.contains(&changed_cid));
        // Глобальную версию не трогаем — остальные записи валидны
    }

    /// Полная инвалидация — только при добавлении нового архетипа.
    pub fn invalidate(&mut self) {
        self.version = self.version.wrapping_add(1);
    }
}

// В world.rs изменить вызовы:

// move_entity — НЕ делает новых архетипов, только перемещает:
pub(crate) fn move_entity(...) {
    // Было: self.query_cache.invalidate();
    // Стало: инвалидируем только компоненты участвующие в переходе
    let added_cid   = /* компонент, который добавляется */;
    let removed_cid = /* компонент, который удаляется */;
    if let Some(cid) = added_cid   { self.query_cache.invalidate_for(cid); }
    if let Some(cid) = removed_cid { self.query_cache.invalidate_for(cid); }
    // ...
}

// get_or_create_archetype — создаёт новый архетип, полная инвалидация:
pub(crate) fn get_or_create_archetype(&mut self, ...) {
    // ...
    self.query_cache.invalidate();  // полная — архетип новый
}
```

**Тест:** Создать мир с 2 типами компонентов A и B. Вставлять/удалять A, проверять что кеш для запросов по B не инвалидируется.

✅ **Реализовано 2026-04-26:**
- `QueryCache::invalidate_for(changed_cid)` использует `map.retain(|key, _| !key.contains(&changed_cid))` — удаляет только записи, содержащие изменённый компонент
- `move_entity` вызывает `invalidate_for` для добавляемого/удаляемого компонента вместо полной `invalidate()`
- `get_or_create_archetype` вызывает полную `invalidate()` — только при создании нового архетипа
- Верификация: `cargo test --workspace` (28 passed)

---

### 2.4. `Column::grow()` — использовать `realloc` вместо `alloc + memcpy`

**Файл:** `crates/apex-core/src/archetype.rs`  
**Функция:** `Column::grow`  
**Выигрыш:** При росте колонок — устранение лишнего `memcpy` для большинства аллокаторов

**Проблема:**

```rust
pub(crate) fn grow(&mut self) {
    let new_cap    = if self.capacity == 0 { 64 } else { self.capacity * 2 };
    let new_layout = self.layout_for(new_cap);
    let new_data   = unsafe { alloc(new_layout) };          // новый блок
    // ...
    unsafe { std::ptr::copy_nonoverlapping(self.data, new_data, ...) }  // memcpy
    unsafe { dealloc(self.data, self.layout_for(self.capacity)) };       // старый блок
}
```

**Решение:** Использовать `std::alloc::realloc`:

```rust
use std::alloc::{alloc, dealloc, realloc, Layout};

pub(crate) fn grow(&mut self) {
    let new_cap = if self.capacity == 0 { 64 } else { self.capacity * 2 };
    if self.item_size == 0 {
        self.capacity = new_cap;
        return;
    }

    let new_size = self.item_size
        .checked_mul(new_cap)
        .expect("overflow in grow");

    let new_data = if self.capacity == 0 || self.data.is_null() {
        // Первая аллокация
        let layout = Layout::from_size_align(new_size, self.item_align).unwrap();
        unsafe { alloc(layout) }
    } else {
        // Реаллокация на месте (jemalloc/mimalloc часто не делает memcpy)
        let old_layout = self.layout_for(self.capacity);
        unsafe { realloc(self.data, old_layout, new_size) }
    };

    assert!(!new_data.is_null(), "allocation failed in grow");
    self.data     = new_data;
    self.capacity = new_cap;
}
```

> **Примечание:** `realloc` не гарантирует отсутствие копирования — это зависит от аллокатора. Но jemalloc (используемый в Rust по умолчанию в некоторых конфигурациях) и mimalloc часто избегают копирования при расширении in-place. На системном аллокаторе Windows/Linux это тоже работает для большинства размеров.

**Тест:** Профилировать `spawn_bundle loop` с perf/valgrind на число `memcpy` вызовов.

✅ **Реализовано 2026-04-26:**
- `Column::grow()`: `alloc(new_layout)` + `copy_nonoverlapping` + `dealloc(old)` → `realloc(self.data, old_layout, new_size)` — устранён лишний `memcpy`
- `Column::reserve(additional)`: аналогично переведён на `realloc`
- Первая аллокация (capacity=0) остаётся через `alloc` (realloc с null не портабелен под все аллокаторы)
- Верификация: `cargo test --workspace` (28 passed)

---

### 2.5. `propagate_transforms` — избежать Vec-аллокации через SparseSet

**Файл:** `crates/apex-core/src/transform.rs`  
**Функция:** `propagate_transforms`  
**Выигрыш:** Устранение 2–3 Vec-аллокаций на вызов. Важно при 60 FPS.

**Проблема:**

```rust
let dirty_entities: Vec<Entity> = {
    let q = world.query_typed::<Read<TransformDirty>>();
    let mut entities = Vec::new();      // аллокация 1
    q.for_each(|e, _| entities.push(e));
    entities
};

let mut ordered = Vec::with_capacity(dirty_entities.len());  // аллокация 2
let mut seen    = FxHashSet::default();                       // аллокация 3
```

**Решение — переиспользуемые буферы через Resource:**

```rust
/// Scratch-буферы для propagate_transforms — переиспользуются каждый кадр.
#[derive(Default)]
pub struct TransformScratch {
    dirty_entities: Vec<Entity>,
    ordered:        Vec<Entity>,
    seen:           rustc_hash::FxHashSet<u32>,
}

pub fn propagate_transforms(world: &mut World) {
    // Извлекаем scratch из ресурсов (или создаём при первом вызове)
    let mut scratch = world.remove_resource::<TransformScratch>()
        .unwrap_or_default();

    scratch.dirty_entities.clear();
    scratch.ordered.clear();
    scratch.seen.clear();

    // ... логика без new аллокаций ...

    world.insert_resource(scratch);
}
```

**Регистрация в `TransformPlugin::register_components`:**

```rust
pub fn register_components(world: &mut World) {
    world.register_component::<LocalTransform>();
    world.register_component::<GlobalTransform>();
    world.register_component::<TransformDirty>();
    world.insert_resource(TransformScratch::default());  // НОВОЕ
    world.register_write_hook::<LocalTransform>(mark_local_transform_dirty);
}
```
✅ **Реализовано 2026-04-26:**
- Создан `TransformScratch` с полями `dirty_entities`, `ordered`, `seen`, `stack`, `children` — все `Vec<Entity>` / `FxHashSet<u32>` переиспользуются между кадрами
- `propagate_transforms`: использует `remove_resource::<TransformScratch>().unwrap_or_default()` — извлекает scratch без borrow conflict, вставляет обратно в конце
- Добавлены буферы `stack` (DFS) и `children` (каскадирование dirty), которые не были учтены в плане — теперь тоже переиспользуются (5 Vec-аллокаций вместо 2–3)
- `TransformPlugin::register_components`: добавлен `world.insert_resource(TransformScratch::default())` для явной инициализации
- Верификация: `cargo test --workspace` (28 passed), `cargo run --example perf --features parallel` (без регрессий)

---

## 4. Фаза 3 — Параллелизм нового уровня

---

### 3.1. ASD (Adaptive Scope Distribution) — ✅ РЕАЛИЗОВАНО

**Файлы:** `crates/apex-scheduler/src/lib.rs`, `crates/apex-core/src/sub_world.rs`
**Функции:** `Scheduler::run_stage_parallel`, `Scheduler::run_hybrid_parallel`, `SubWorld::with_ranges`
**Дата:** 2026-04-26

**Ключевая идея:** Единый адаптивный алгоритм, заменяющий два раздельных режима (per-system scope + intra-system chunking). Вместо ручного порога `parallel_threshold` — автоматическое определение:

```
target_chunk = max(total_entity_count / num_workers / 2, 64)

for each system:
    if arch_indices.len() <= 1 || entity_count <= target_chunk:
        → 1 задача (per-system scope, без ranges — zero overhead)
    else:
        → N задач (чанки размером ~target_chunk), сортировка по archetype_id
```

Запуск через `rayon::scope` + `s.spawn(|_| ...)` (не `par_iter` — избегает двойного chunking Rayon).

**Изменения:**

1. **`SubWorld::with_ranges`** — снято требование `'w` lifetime. Через `unsafe transmute` принимает `&[usize]` и `&[(usize, usize, usize)]` с любым временем жизни:
   ```rust
   pub fn with_ranges(
       world: &'w World,
       archetype_indices: &[usize],
       row_ranges: &[(usize, usize, usize)],
   ) -> Self {
       unsafe {
           Self {
               world,
               archetype_indices: std::mem::transmute::<&[usize], &'w [usize]>(archetype_indices),
               row_ranges: std::mem::transmute::<&[(usize, usize, usize)], &'w [(usize, usize, usize)]>(row_ranges),
           }
       }
   }
   ```

2. **`AsdTask`** (заменил `SystemTask`):
   ```rust
   struct AsdTask {
       ptr: SendPtr<SystemDescriptor>,
       sys_archs_ptr: *const usize,    // *const [usize] не работал (Sized)
       sys_archs_len: usize,
       chunk_ranges: SmallVec<[(usize, usize, usize); 4]>,  // стек до 4 элементов
   }
   ```

3. **`run_stage_parallel`** — полный rewrite:
   - Вычисление entity_count для каждой системы (сумма длин архетипов)
   - Вычисление `target_chunk = max(total / workers / 2, MIN_CHUNK)`
   - Системы с `arch_indices.len() <= 1` или `entity_count <= target_chunk` → 1 задача без ranges
   - Multi-archetype системы → разбивка на чанки, сортировка по archetype_id
   - `rayon::scope` для spawn

4. **`run_hybrid_parallel`** — убрано условие `stage_ids.len() < parallel_threshold` (больше не нужно).

**Результаты бенчмарков (ASD vs предыдущий per-system scope):**

| Сценарий | До ASD | ASD | Ускорение |
|----------|--------|-----|-----------|
| 2 light (memory-bound) | 0.29x | **0.67x** | **+131%** |
| 4 light | 0.51x | **1.32x** | **+159%** |
| 2 CPU-bound isolated | 1.29x | **1.33x** | **+3%** |
| 2 CPU-bound shared arch | 0.36x | **1.10x** | **+206%** |
| 3 CPU-bound shared arch | 0.71x | **1.10x** | **+55%** |
| 8 solo systems | 1.50x | **3.64x** | **+143%** |
| **12 solo systems** | 2.03x | **3.91x** | **+93%** |
| Pipeline sequential barrier | 0.35x | **0.92x** | **+163%** |

**Почему ASD лучше оригинального плана (task-based ParTask):**

- Оригинальный план дробил **каждую систему** на чанки по 1 архетипу → massive task overhead для малых систем
- ASD не дробит системы с `entity_count <= target_chunk` → zero overhead для memory-bound систем
- `SmallVec` вместо `Vec` — ноль heap-аллокаций для 90% случаев
- Сортировка чанков по archetype_id — меньше кеш-промахов
- Нет `world_ptr: *const World` — SubWorld создаётся через `prepare_sub_worlds`, безопаснее

**Верификация:**
- `cargo build --features parallel` — успешно
- `cargo test --features parallel` — 28 passed
- `cargo clippy` — без новых предупреждений
- `cargo run -p apex-examples --example perf --release --features parallel` — верифицировано

---

#### 3.1.1. Adaptive chunk size — адаптивное чанкование для `par_for_each` — ✅ РЕАЛИЗОВАНО

**Файл:** `crates/apex-core/src/world.rs`
**Функция:** `adaptive_chunk_size(entity_count, num_threads) -> usize`
**Константы:** `DEFAULT_MAX_CHUNK_SIZE = 16384`, `PAR_CHUNK_SIZE` (AtomicUsize, env `APEX_PAR_CHUNK_SIZE`)
**Дата:** 2026-04-26

В дополнение к ASD (распределение систем по ядрам на уровне планировщика) реализован адаптивный алгоритм чанкования для параллельных итераций внутри одной системы (`CachedQuery::par_for_each`, `par_for_each_component`).

**Алгоритм:**

```
chunk = entity_count / max(num_threads, 1)

# Абсолютный максимум — пользовательская настройка или 16384
if chunk > ABSOLUTE_MAX → chunk = ABSOLUTE_MAX

# Динамический минимум: зависит от общего числа entity
if   entity_count < 100   → min = 128   // очень мало сущностей → крупные чанки (минимум накладных расходов)
elif entity_count < 1000  → min = 32    // средний размер → умеренное дробление
else                      → min = 64    // много сущностей → баланс дробления и кеш-промахов

if chunk < min → chunk = min

# Финальная гарантия: чанк не больше числа entity
chunk = min(chunk, entity_count)
```

**Ключевые особенности:**

1. **Трёхуровневый dynamic minimum** — вместо жёсткой константы `MIN_CHUNK`:
   - `< 100` entity → `min = 128` (минимум накладных расходов на spawn, почти всегда 1 чанк целиком)
   - `100–1000` entity → `min = 32` (умеренное дробление, заполнение всех ядер)
   - `>= 1000` entity → `min = 64` (баланс: чанки не слишком мелкие, но достаточно для всех workers)

2. **`chunk.min(entity_count)`** — финальный предохранитель: исключает ситуацию `chunk > entity_count`, когда dynamic min превышает количество сущностей. Например, для 50 entity и 8 workers: `50/8=6 → min=128 → chunk=128 → min(128,50)=50`.

3. **Настраиваемый абсолютный максимум** — через `PAR_CHUNK_SIZE` (AtomicUsize):
   - Устанавливается через `set_par_chunk_size(n)` или env `APEX_PAR_CHUNK_SIZE=n`
   - Значение `0` означает «использовать `DEFAULT_MAX_CHUNK_SIZE = 16384`»
   - Инициализация из env происходит при старте через `init_par_chunk_size_from_env()`

4. **Интеграция с ASD:** ASD дробит системы на уровне планировщика, а `adaptive_chunk_size` — на уровне запроса. Они не конфликтуют: если ASD уже создал 1 задачу для системы с малым числом entity, то `par_for_each` внутри этой задачи всё равно корректно поделит работу между ядрами.

**Верификация:**
- `cargo test adaptive_chunk_size --workspace` — 6 passed (small_world, medium_world, large_world, single_thread, max_cap, transition_points)

---

### 3.2. Автоматическое разделение работы по архетипам между системами — ❌ ЗАМЕНЕНО НА ASD

**Статус:** План 3.2 (row-level splits между системами) признан избыточным. ASD уже решает проблему загрузки workers автоматически:
- Мало entity → per-system scope (как в оригинале)
- Много entity → чанки заполняют все ядра
- Row-level splits добавили бы оверхэд без выигрыша, т.к. ASD уже дробит достаточно мелко

---

### 3.3. Thread-local Commands в параллельных системах — ⏳ PENDING

**Файлы:** `crates/apex-core/src/world.rs`, `crates/apex-scheduler/src/lib.rs`  
**Выигрыш:** Устранение необходимости вручную управлять Commands в `par_for_each`

**Текущая проблема:** Пользователь должен писать:

```rust
// Неудобно и error-prone:
impl ParSystem for MySystem {
    fn run(&mut self, ctx: SystemContext<'_>) {
        let mut local_cmds = Commands::new();  // вручную
        ctx.query::<Read<Health>>().for_each(|e, hp| {
            if hp.current <= 0.0 { local_cmds.despawn(e); }
        });
        // Нет возможности применить внутри системы!
    }
}
```

**Решение:** Добавить в `SystemContext` доступ к thread-local командам:

```rust
// В world.rs:
pub struct SystemContext<'w> {
    pub(crate) sub_worlds: &'w [SubWorld<'w>],
    // НОВОЕ:
    pub(crate) deferred_cmds: *mut Vec<Commands>,  // один Commands на поток
}

impl<'w> SystemContext<'w> {
    /// Получить Commands для текущего потока.
    /// Команды применяются планировщиком после завершения Stage.
    #[inline]
    pub fn commands(&self) -> &mut Commands {
        unsafe {
            // rayon thread_index() для изоляции
            let thread_idx = rayon::current_thread_index().unwrap_or(0);
            &mut (*self.deferred_cmds)[thread_idx]
        }
    }
}
```

**В `run_hybrid_parallel` — применять команды после Stage:**

```rust
// После выполнения параллельного Stage:
for cmds in &mut self.thread_commands {
    cmds.apply(world);
}
```

---

## 5. Фаза 4 — Функциональные доработки


---

### 4.1. `add_relations_batch` — batch-добавление Relations

**Файл:** `crates/apex-core/src/relations.rs` и `world.rs`  
**Выигрыш:** Создание иерархии 1000 объектов: 1000 архетипных переходов → 1 переход

**Текущая проблема:** `add_relation` на каждую entity вызывает `move_entity`, меняя архетип. При создании 1000-entity иерархии — 1000 переходов.

**Новый API:**

```rust
impl World {
    /// Добавить одинаковую relation от множества субъектов к одному target.
    ///
    /// Оптимизирован для массового создания иерархий (например, тайловые карты).
    /// Все subjects перемещаются в новый архетип за один проход.
    pub fn add_relation_batch<R: RelationKind>(
        &mut self,
        subjects: &[Entity],
        kind: R,
        target: Entity,
    ) {
        if subjects.is_empty() { return; }

        let kind_idx    = self.relations.get_or_register::<R>();
        let relation_id = encode_relation(kind_idx, target.index);
        self.ensure_relation_component(relation_id);

        // Группируем subjects по текущему архетипу
        let mut by_arch: FxHashMap<ArchetypeId, Vec<Entity>> = FxHashMap::default();
        for &entity in subjects {
            if let Some(loc) = self.entities.get_location(entity) {
                by_arch.entry(loc.archetype_id).or_default().push(entity);
            }
        }

        // Для каждой группы — один batch move_entity
        for (arch_id, group) in by_arch {
            let new_arch_id = self.find_or_create_archetype_with(arch_id, relation_id);

            for entity in group {
                let loc = self.entities.get_location(entity).unwrap();
                let new_row = self.move_entity(entity, loc, new_arch_id);
                self.entities.set_location(entity, EntityLocation {
                    archetype_id: new_arch_id,
                    row: new_row,
                });
                self.subject_index.add(entity.index, relation_id);
            }
        }
    }
}
```

✅ **Реализовано 2026-04-26:**
- Добавлен `World::add_relation_batch` в `world.rs` с группировкой по исходному архетипу
- `ensure_relation_component` вызывается один раз для relation_id, а не для каждой entity
- `find_or_create_archetype_with` — один вызов на группу вместо одного на entity
- `FxHashMap<ArchetypeId, Vec<Entity>>` — группировка entity по текущему архетипу
- Верификация: `cargo test --workspace` (28 passed)

---

### 4.2. Hot-reload дебаунс в ScriptEngine

**Файл:** `crates/apex-scripting/src/script_engine.rs`  
**Функция:** `ScriptEngine::with_dir`  
**Выигрыш:** Устранение дублированных reload-событий при сохранении файла редактором

**Проблема:** В `ScriptEngine::with_dir` создаётся `recommended_watcher` без дебаунса, тогда как `HotReloadPlugin` использует `Config::default().with_poll_interval(debounce)`.

**Решение — унифицировать:**

```rust
pub fn with_dir(script_dir: &Path) -> Self {
    Self::with_dir_debounce(script_dir, Duration::from_millis(100))
}

pub fn with_dir_debounce(script_dir: &Path, debounce: Duration) -> Self {
    let mut this = Self::new();
    let (tx, rx) = mpsc::channel();

    let watcher_result = notify::RecommendedWatcher::new(
        move |res: notify::Result<Event>| { let _ = tx.send(res); },
        notify::Config::default().with_poll_interval(debounce),  // ДОБАВИТЬ
    );
    // ...
    this
}
```

---

### 4.3. `Without<T>` в ArchetypeMask — exclude маска

**Файл:** `crates/apex-core/src/access.rs`, `crates/apex-core/src/query.rs`  
**Выигрыш:** `Without<T>` фильтрует архетипы до цикла итерации

**Текущая проблема:** `Without<T>` проверяет `!arch.has_component(ids[0])` внутри `matches_archetype` — это уже правильно, но только при обходе всех архетипов. С `component_arch_index` (из 2.1) нужно начинать с инверсии.

**Добавить `exclude_cids` в `WorldQuery`:**

```rust
pub trait WorldQuery: Sized {
    // Существующие методы...

    /// ComponentId'ы, которые НЕ должны присутствовать в архетипе.
    fn excluded_ids(_world: &World, _ids: &mut Vec<ComponentId>) {}
}

// Реализация для Without<T>:
impl<T: Component> WorldQuery for Without<T> {
    fn excluded_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }
    // fill_ids оставляем пустым (Without не требует наличия компонента)
    fn fill_ids(_world: &World, _ids: &mut Vec<ComponentId>) {}
    // ...
}
```

**В `Query::new` использовать excluded_ids для фильтрации списка кандидатов:**

```rust
let mut excluded = Vec::new();
Q::excluded_ids(world, &mut excluded);

let archetypes = candidate_archetypes.into_iter()
    .filter(|&arch_idx| {
        let arch = &world.archetypes[arch_idx];
        // Быстрая проверка исключений перед полным matches_archetype
        if excluded.iter().any(|&eid| arch.has_component(eid)) {
            return false;
        }
        Q::matches_archetype(arch, &ids)
    })
    // ...
```

---

### 4.4. Диагностика конфликтов с именами компонентов

**Файл:** `crates/apex-scheduler/src/lib.rs`  
**Функция:** `component_type_name`  
**Выигрыш:** `debug_plan_verbose()` показывает реальные имена вместо `<component>`

**Текущая проблема:** `type_names` заполняется только в `run()` / `run_sequential()`, но до первого `run()` — пустая. При вызове `debug_plan_verbose()` сразу после `compile()` все компоненты показываются как `<component>`.

**Решение:**

```rust
// В compile():
pub fn compile(&mut self) -> Result<(), SchedulerError> {
    // ... существующий код ...

    // ДОБАВИТЬ: если type_names пуст, логируем предупреждение
    if self.type_names.is_empty() {
        log::debug!(
            "Scheduler::compile: type_names пуст. \
             Вызовите populate_type_names(&world.registry()) \
             для отображения имён компонентов в debug_plan_verbose()"
        );
    }

    // ...
}

// Добавить convenience-метод:
pub fn compile_with_world(&mut self, world: &World) -> Result<(), SchedulerError> {
    self.populate_type_names(world.registry());
    self.compile()
}
```

**В документации добавить пример:**

```rust
// Рекомендуемый паттерн:
sched.compile_with_world(&world).expect("schedule error");
println!("{}", sched.debug_plan_verbose());  // теперь с реальными именами
```

---

## 6. Метрики успеха

### Бенчмарки для верификации каждой задачи

| Задача | Бенчмарк команда | Метрика | Текущий статус |
|---|---|---|---|
| 1.1 write_into_batch | `cargo bench simple_insert` | spawn_many ns/op | ✅ Реализовано |
| 1.2 ArchetypeKey | `cargo bench structural` | allocations count | ✅ Реализовано |
| 1.3 move_entity | `cargo bench structural` | insert ns/op | ✅ Реализовано |
| 1.4 seq barriers | `cargo bench compile_overhead` | N=50 time | ✅ Реализовано |
| 1.5 EventRegistry | `cargo bench events` | send+iter ns/op | ✅ Реализовано |
| 2.1 ArchetypeMask | custom bench (1000 archetypes) | Query::new µs | ✅ Реализовано |
| 2.2 CommandQueue | `cargo bench structural` | Commands::apply allocs | ⏳ Pending |
| 2.3 QueryCache | custom bench (frequent insert) | CachedQuery hits | ✅ Реализовано |
| 2.4 realloc | perf stat | cache-misses | ✅ Реализовано |
| 2.5 TransformScratch | alloc profiler | allocs/frame | ✅ Реализовано |
| **3.1 ASD** | `cargo run -p apex-examples --example perf --release --features parallel` | 12-sys speedup: 2.03x→**3.91x** (+93%) | ✅ **Реализовано** |
| 3.2 Row splits | — | Заменён на ASD | ❌ Заменён |
| 3.3 Thread-local cmds | compilation test | usability | ⏳ Pending |
| 4.1 batch relations | custom bench | 1000 relations | ✅ Реализовано |
| 4.2 debounce | manual test | reload storms | ⏳ Pending |
| 4.3 Without exclude | custom bench | Without query time | ⏳ Pending |
| 4.4 compile_with_world | manual test | debug_plan quality | ⏳ Pending |

#### Доп. результаты ASD-бенчмарков

| Сценарий | До ASD | ASD | Ускорение |
|----------|--------|-----|-----------|
| 2 light (memory-bound) | 0.29x | **0.67x** | **+131%** |
| 4 light | 0.51x | **1.32x** | **+159%** |
| 2 CPU-bound isolated | 1.29x | **1.33x** | **+3%** |
| 2 CPU-bound shared arch | 0.36x | **1.10x** | **+206%** |
| 3 CPU-bound shared arch | 0.71x | **1.10x** | **+55%** |
| 8 solo systems | 1.50x | **3.64x** | **+143%** |
| 12 solo systems | 2.03x | **3.91x** | **+93%** |
| Pipeline barrier | 0.35x | **0.92x** | **+163%** |


---

## 7. Порядок реализации и зависимости

```
Фаза 1 (все реализованы ✅):
  ✅ 1.1  ──► ✅ 1.2 ──► ✅ 1.3 (линейная цепочка оптимизаций spawn/move)
  ✅ 1.4  (независимо от 1.1-1.3)
  ✅ 1.5  (независимо от всего)

Фаза 2 (реализована):
  ✅ 2.1  ──► ✅ 2.3  (ArchetypeMask — фундамент для QueryCache)
  ⏳ 2.2  (CommandQueue — pending)
  ✅ 2.4  (realloc — реализовано)
  ✅ 2.5  (TransformScratch — реализовано)

Фаза 3 (ASD реализован, row-splits заменён):
  ✅ 3.1 ASD  (Adaptive Scope Distribution — реализовано)
  ❌ 3.2     (заменён на ASD — row splits избыточны)
  ⏳ 3.3     (Thread-local Commands — pending)

Фаза 4 (частично реализована):
  ✅ 4.1  (batch relations — реализовано)
  ⏳ 4.2  (debounce — pending)
  ⏳ 4.3  (Without<T> — pending, требует 2.1 ✅)
  ⏳ 4.4  (compile_with_world — pending)
```

### Фактический порядок реализации

```
 1.3 move_entity однопроходный         (Фаза 1) ✅
 1.4 seq barriers O(N)                 (Фаза 1) ✅
 1.5 EventRegistry raw_ptrs            (Фаза 1) ✅
 1.1 + 1.2 batch spawn оптимизации     (Фаза 1) ✅
 2.1 ArchetypeMask / component_arch_idx(Фаза 2) ✅
 2.5 TransformScratch                  (Фаза 2) ✅
 3.1 ASD (Adaptive Scope Distribution) (Фаза 3) ✅ ← ВЫ ПОЛНОСТЬЮ ЗДЕСЬ
 2.3 QueryCache invalidate_for          (Фаза 2) ✅
 2.4 Column::realloc                    (Фаза 2) ✅
 4.1 add_relation_batch                 (Фаза 4) ✅
 4.x Оставшиеся задачи                 (Фаза 4) ⏳
```

### Следующие шаги (prioritised)

| Приоритет | Задача | Файлы | Ожидаемый выигрыш |
|-----------|--------|-------|-------------------|
| 1 | 3.3 Thread-local Commands | `crates/apex-scheduler/src/lib.rs`, `crates/apex-core/src/world.rs` | Usability: commands() внутри par_for_each |
| 2 | 2.2 CommandQueue chunking | `crates/apex-core/src/commands.rs` | -60% allocs в Commands::apply |
| 3 | 4.3 Without<T> exclude mask | `crates/apex-core/src/query.rs` | -20% время Without-запросов |
| 4 | 4.2 Hot-reload debounce | `crates/apex-scripting/src/script_engine.rs` | Устранение reload storms |
| 5 | 4.4 compile_with_world | `crates/apex-scheduler/src/lib.rs` | debug_plan quality: имена видны |

### Чеклист перед PR

- [ ] `cargo test --workspace` — все тесты зелёные
- [ ] `cargo bench` — нет регрессий в baseline метриках
- [ ] `cargo clippy --workspace` — без новых предупреждений
- [ ] Unsafe-блоки снабжены `// SAFETY:` комментарием
- [ ] Публичные изменения задокументированы в rustdoc
- [ ] Обновлён CHANGELOG.md если изменился публичный API

---
