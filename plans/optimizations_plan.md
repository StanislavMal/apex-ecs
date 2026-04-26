# APEX ECS — План оптимизации

> Версия 1.0 | Апрель 2026  
> Документ предназначен для ИИ-программиста. Каждый раздел содержит точные указатели на файлы, сигнатуры функций и конкретный код изменений.

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
| Межсистемный параллелизм (12 систем) | 3.30x speedup | 7x–9x speedup |
| CPU-bound 2 системы (изол. архетипы) | 1.13x speedup | 1.8x–2.0x speedup |
| `insert component` | 72 ns/op | 45–55 ns/op |
| `spawn_bundle loop` | 105 ns/op | уже OK, цель — batch |
| `compile()` при N=50 | 110 800 ns | < 30 000 ns |

### Принципы изменений

- **API stability first.** Фазы 1–2 не меняют публичный API.
- **Измеряемый прогресс.** Каждая задача сопровождается конкретным бенчмарком.
- **Атомарность.** Каждый пункт — отдельный PR/commit с тестами.

---

## 2. Фаза 1 — Быстрые победы

> Срок: 1–2 недели. Сложность: низкая. Никаких изменений публичного API.

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

---

## 3. Фаза 2 — Структурные оптимизации ядра

> Срок: 2–4 недели. Сложность: средняя. Изменения внутренней архитектуры, API совместим.

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

---

## 4. Фаза 3 — Параллелизм нового уровня

> Срок: 3–5 недель. Сложность: высокая. Это главная задача для масштабирования.

---

### 3.1. Task-based параллелизм: чанк как единица работы

**Файл:** `crates/apex-scheduler/src/lib.rs`  
**Функция:** `Scheduler::run_hybrid_parallel`  
**Выигрыш:** 3.30x → 7x–9x speedup при 12 ядрах и 12 независимых системах

**Корневая проблема текущей архитектуры:**

```
Текущая модель:          Желаемая модель:
┌─────────────────┐      ┌──────────────────────────────────────┐
│ Stage           │      │ Stage                                │
│  task(System A) │      │  task(A_chunk_0) task(A_chunk_1) ..│
│  task(System B) │      │  task(B_chunk_0) task(B_chunk_1) ..│
│  task(System C) │      │  task(C_chunk_0) task(C_chunk_1) ..│
└─────────────────┘      └──────────────────────────────────────┘
   3 задачи на 12 ядер      36 задач на 12 ядер — лучший балансинг
```

При 3 системах и 12 ядрах текущая модель даёт максимум 3x. Task-based — до 12x.

**Новый тип `ParTask`:**

```rust
/// Единица параллельной работы — чанк одного архетипа для одной системы.
struct ParTask {
    /// Указатель на систему (SendPtr для Rayon)
    system_ptr: SendPtr<SystemDescriptor>,
    /// Индекс архетипа
    arch_idx: usize,
    /// Диапазон строк [start, end)
    start: usize,
    end: usize,
    /// Ссылка на мир (const — только чтение структуры, данные через ptr)
    world_ptr: *const World,
}

unsafe impl Send for ParTask {}
unsafe impl Sync for ParTask {}
```

**Новая функция выполнения Stage:**

```rust
#[cfg(feature = "parallel")]
fn run_stage_parallel(&mut self, stage: &[(SystemId, bool)], world: &World) {
    let num_threads = rayon::current_num_threads();

    // Собираем все задачи для всех систем Stage
    let mut tasks: Vec<ParTask> = Vec::new();

    for &(sys_id, _) in stage {
        let sys_idx = match self.system_indices.get(&sys_id) { Some(&i) => i, None => continue };
        let arch_indices = self.system_archetype_indices.get(&sys_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        for &arch_idx in arch_indices {
            let arch_len = world.archetypes()[arch_idx].len();
            if arch_len == 0 { continue; }

            let chunk_size = adaptive_chunk_size(arch_len, num_threads);
            let num_chunks = (arch_len + chunk_size - 1) / chunk_size;

            for chunk_i in 0..num_chunks {
                let start = chunk_i * chunk_size;
                let end   = (start + chunk_size).min(arch_len);
                tasks.push(ParTask {
                    system_ptr: SendPtr(&mut self.systems[sys_idx] as *mut _),
                    arch_idx,
                    start,
                    end,
                    world_ptr: world as *const World,
                });
            }
        }
    }

    // Запускаем все задачи параллельно
    use rayon::prelude::*;
    tasks.par_iter().for_each(|task| {
        unsafe {
            let world  = &*task.world_ptr;
            let system = &mut *task.system_ptr.0;

            if let SystemKind::Parallel { system: sys, .. } = &mut system.kind {
                // Создаём SubWorld только для диапазона строк этого чанка
                let indices = std::slice::from_ref(&task.arch_idx);
                let ranges  = [(task.arch_idx, task.start, task.end)];
                let sub     = apex_core::SubWorld::with_ranges(world, indices, &ranges);
                sys.run(SystemContext::from_sub_world(&sub));
            }
        }
    });
}
```

**Обновить `run_hybrid_parallel` для использования нового метода:**

```rust
#[cfg(feature = "parallel")]
fn run_hybrid_parallel(&mut self, world: &mut World) {
    let plan = self.execution_plan.as_ref().unwrap();

    for stage in &plan.stages {
        // Фильтруем Startup
        if stage.label == StageLabel::Startup && self.startup_completed { continue; }

        let parallel_ids: Vec<(SystemId, bool)> = stage.system_ids.iter()
            .map(|&id| (id, true))
            .collect();

        if stage.all_parallel && stage.system_ids.len() >= self.parallel_threshold {
            // НОВОЕ: task-based параллелизм
            let const_world = unsafe { &*(world as *const World) };
            self.run_stage_parallel(&parallel_ids, const_world);
        } else {
            // Последовательно (sequential системы или мало систем)
            for &sys_id in &stage.system_ids {
                if let Some(&idx) = self.system_indices.get(&sys_id) {
                    let sw = self.make_sub_world(idx, unsafe { &*(world as *const World) });
                    match &mut self.systems[idx].kind {
                        SystemKind::Sequential(f) => f(world),
                        SystemKind::Parallel { system, .. } => {
                            system.run(SystemContext::from_sub_world(&sw));
                        }
                    }
                }
            }
        }
    }

    self.startup_completed = true;
}
```

**Критически важно:** Системы в одном Stage не должны иметь Write-конфликтов (это гарантирует `compile()`). Поэтому параллельный доступ к разным компонентам через `*mut` безопасен.

**Тест:** `cargo bench parallel_scheduler` — все секции. Целевой speedup ≥ 5x при 12 системах.

---

### 3.2. Автоматическое разделение работы по архетипам между системами

**Файл:** `crates/apex-scheduler/src/lib.rs`  
**Функция:** `Scheduler::prepare_sub_worlds`  
**Выигрыш:** Устранение дублированной обработки одних и тех же строк архетипа двумя системами

**Контекст:** Если система A и система B обе имеют доступ к архетипу [Position, Velocity], они обе будут обрабатывать все его строки. Но если они не конфликтуют по компонентам, можно разделить строки между ними.

**Уточнение:** Row-level splits имеет смысл только когда системы НЕ читают одни и те же компоненты (иначе разделение не даёт прироста). Алгоритм:

```rust
fn compute_row_splits(
    stage_systems: &[SystemId],
    world: &World,
    system_archetype_indices: &FxHashMap<SystemId, Vec<usize>>,
) -> FxHashMap<SystemId, Vec<(usize, usize, usize)>> {
    let mut splits: FxHashMap<SystemId, Vec<(usize, usize, usize)>> = FxHashMap::default();

    // Группировать системы по overlapping архетипам
    // Для каждого архетипа определить, сколько систем его используют
    let mut arch_system_count: FxHashMap<usize, usize> = FxHashMap::default();
    for &sys_id in stage_systems {
        if let Some(indices) = system_archetype_indices.get(&sys_id) {
            for &arch_idx in indices {
                *arch_system_count.entry(arch_idx).or_insert(0) += 1;
            }
        }
    }

    // Для архетипов с >1 системой — разделить строки
    for (arch_idx, count) in &arch_system_count {
        if *count <= 1 { continue; }
        let arch_len = world.archetypes()[*arch_idx].len();
        let chunk    = (arch_len + count - 1) / count;

        let mut offset = 0;
        for (i, &sys_id) in stage_systems.iter().enumerate() {
            if system_archetype_indices.get(&sys_id)
                .map(|v| v.contains(arch_idx))
                .unwrap_or(false)
            {
                let start = offset.min(arch_len);
                let end   = (offset + chunk).min(arch_len);
                splits.entry(sys_id).or_default().push((*arch_idx, start, end));
                offset = end;
            }
        }
    }

    splits
}
```

---

### 3.3. Thread-local Commands в параллельных системах

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

> Срок: 1–2 недели. Сложность: низкая–средняя. Новые возможности.

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

| Задача | Бенчмарк команда | Метрика | Цель |
|---|---|---|---|
| 1.1 write_into_batch | `cargo bench simple_insert` | spawn_many ns/op | -15% |
| 1.2 ArchetypeKey | `cargo bench structural` | allocations count | -50% for insert |
| 1.3 move_entity | `cargo bench structural` | insert ns/op | 72→55 ns |
| 1.4 seq barriers | `cargo bench compile_overhead` | N=50 time | 110µs→30µs |
| 1.5 EventRegistry | `cargo bench events` | send+iter ns/op | -30% |
| 2.1 ArchetypeMask | custom bench (1000 archetypes) | Query::new µs | -50% |
| 2.2 CommandQueue | `cargo bench structural` | Commands::apply allocs | -60% |
| 2.3 QueryCache | custom bench (frequent insert) | CachedQuery hits | >80% |
| 2.4 realloc | perf stat | cache-misses | -10% |
| 2.5 TransformScratch | alloc profiler | allocs/frame | 0 in hot path |
| 3.1 Task-based par | `cargo bench parallel_scheduler` | 12-sys speedup | 3.3x→7x |
| 3.2 Row splits | `cargo bench parallel_scheduler` | 2-sys CPU-bound | 1.1x→1.8x |
| 3.3 Thread-local cmds | compilation test | usability | API improvement |
| 4.1 batch relations | custom bench | 1000 relations | -90% transitions |
| 4.2 debounce | manual test | reload storms | eliminated |
| 4.3 Without exclude | custom bench | Without query time | -20% |
| 4.4 compile_with_world | manual test | debug_plan quality | names visible |

### Запуск всех бенчмарков

```bash
# Последовательный режим (для baseline):
cargo bench --features "" 2>&1 | tee bench_baseline.txt

# Параллельный режим:
cargo bench --features parallel 2>&1 | tee bench_parallel.txt

# Сравнение:
cargo bench --features parallel -- --baseline bench_baseline
```

---

## 7. Порядок реализации и зависимости

```
Фаза 1 (независимые, начать любую):
  1.1  ──► 1.2 ──► 1.3 (линейная цепочка оптимизаций spawn/move)
  1.4  (независимо от 1.1-1.3)
  1.5  (независимо от всего)

Фаза 2 (требует завершения Фазы 1):
  2.1  ──► 2.3  (ArchetypeMask используется в QueryCache)
  2.2  (независимо)
  2.4  (независимо)
  2.5  (независимо)

Фаза 3 (требует 2.1 для корректного SubWorld):
  3.1  ──► 3.2  (task-based par, потом row splits как расширение)
  3.3  (можно параллельно с 3.1)

Фаза 4 (независимые, можно в любой момент):
  4.1  (независимо)
  4.2  (независимо)
  4.3  требует 2.1
  4.4  (независимо)
```

### Рекомендуемый порядок для ИИ-программиста

1. **Начать с 1.3** (move_entity однопроходный) — изолированное изменение, хорошо тестируется, даёт реальный прирост
2. **Затем 1.4** (seq barriers O(N)) — compile() становится намного быстрее  
3. **Затем 1.5** (EventRegistry кеш) — независимое, простое
4. **Затем 1.1 + 1.2** вместе (batch spawn оптимизации)
5. **Затем 2.1** (ArchetypeMask) — фундамент для 2.3 и 4.3
6. **Затем 2.5** (TransformScratch) — простое, заметное
7. **Затем 3.1** (task-based par) — главная задача, максимальный выигрыш
8. **Затем 4.1, 4.2, 4.4** в любом порядке

### Чеклист перед PR

- [ ] `cargo test --workspace` — все тесты зелёные
- [ ] `cargo bench` — нет регрессий в baseline метриках
- [ ] `cargo clippy --workspace` — без новых предупреждений
- [ ] Unsafe-блоки снабжены `// SAFETY:` комментарием
- [ ] Публичные изменения задокументированы в rustdoc
- [ ] Обновлён CHANGELOG.md если изменился публичный API

---

*APEX ECS Optimization Plan v1.0 — Апрель 2026*
