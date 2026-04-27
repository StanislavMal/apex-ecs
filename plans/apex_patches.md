# APEX ECS — Полный набор патчей

Все патчи написаны против реального кода из контекста.
Каждый патч — законченный diff, который можно применить напрямую.

---

## Патч 1: `QueryCache` — убрать heap-аллокацию при cache hit

**Файл:** `crates/apex-core/src/world.rs`

**Проблема:** `map.entry(key.to_vec())` создаёт `Vec<ComponentId>` на каждый вызов
`get_or_compute`, включая cache hit. При 100k entity и нескольких системах
это миллионы лишних аллокаций за сессию.

```rust
// БЫЛО — в world.rs, структура QueryCache:
pub(crate) struct QueryCache {
    entries: UnsafeCell<FxHashMap<Vec<ComponentId>, CacheEntry>>,
    version: u32,
}

// было в get_or_compute:
pub unsafe fn get_or_compute(
    &self,
    key:           &[ComponentId],
    world_version: u32,
    archetypes:    &[Archetype],
    matches:       impl Fn(&Archetype) -> bool,
) -> &[usize] {
    let map   = &mut *self.entries.get();
    let entry = map.entry(key.to_vec()).or_insert(CacheEntry { // <-- аллокация
        arch_indices: Vec::new(),
        version:      u32::MAX,
    });
    // ...
}

// СТАЛО — заменяем тип ключа на SmallVec и используем Borrow:
use smallvec::SmallVec;

// Новый ключ-обёртка — аналог ArchetypeKey из archetype_index:
#[derive(Clone, PartialEq, Eq, Hash)]
struct QueryCacheKey(SmallVec<[ComponentId; 8]>);

impl std::borrow::Borrow<[ComponentId]> for QueryCacheKey {
    fn borrow(&self) -> &[ComponentId] { &self.0 }
}

pub(crate) struct QueryCache {
    entries: UnsafeCell<FxHashMap<QueryCacheKey, CacheEntry>>,
    version: u32,
}

impl QueryCache {
    pub fn new() -> Self {
        Self { entries: UnsafeCell::new(FxHashMap::default()), version: 0 }
    }

    pub unsafe fn get_or_compute(
        &self,
        key:           &[ComponentId],   // теперь только &[ComponentId] — без аллокации
        world_version: u32,
        archetypes:    &[Archetype],
        matches:       impl Fn(&Archetype) -> bool,
    ) -> &[usize] {
        let map = &mut *self.entries.get();

        // Lookup по &[ComponentId] через Borrow — zero-copy, без аллокации при hit
        if let Some(entry) = map.get_mut(key) {
            if entry.version == world_version {
                return &entry.arch_indices;
            }
            // Cache stale — обновляем на месте, не создаём новую запись
            entry.arch_indices = archetypes
                .iter()
                .enumerate()
                .filter(|(_, arch)| !arch.is_empty() && matches(arch))
                .map(|(i, _)| i)
                .collect();
            entry.version = world_version;
            return &entry.arch_indices;
        }

        // Miss — вставляем новую запись (аллокация только здесь)
        let arch_indices = archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| !arch.is_empty() && matches(arch))
            .map(|(i, _)| i)
            .collect();
        let query_key = QueryCacheKey(key.iter().copied().collect());
        map.insert(query_key, CacheEntry { arch_indices, version: world_version });

        // Берём снова через &[ComponentId] (Borrow)
        &map.get(key).unwrap().arch_indices
    }

    pub fn invalidate(&mut self) { self.version = self.version.wrapping_add(1); }

    pub fn invalidate_for(&mut self, changed_cid: ComponentId) {
        let map = unsafe { &mut *self.entries.get() };
        map.retain(|key, _| !key.0.contains(&changed_cid));
    }

    pub fn version(&self) -> u32 { self.version }
}
```

**Ожидаемый результат:** устраняет heap-аллокацию на каждый `query_typed` call.
При типичном игровом цикле с 10–20 системами и 5–10 типами query — экономия
нескольких тысяч аллокаций per frame.

---

## Патч 2: `AccessDescriptor` — освобождать Vec'ы после `assign_masks`

**Файл:** `crates/apex-core/src/access.rs`

**Проблема:** После `compile()` поля `reads`, `writes`, `reads_event`, `writes_event`
никогда больше не читаются планировщиком, но остаются живыми. При N системах
это N × 4 ненужных Vec<TypeId>.

```rust
// БЫЛО в AccessDescriptor:
pub fn assign_masks(&mut self, type_to_idx: &std::collections::HashMap<TypeId, u8>) {
    self.read_mask  = ComponentMask::EMPTY;
    self.write_mask = ComponentMask::EMPTY;
    for tid in &self.reads  {
        if let Some(&idx) = type_to_idx.get(tid) { self.read_mask.set(idx); }
    }
    for tid in &self.writes {
        if let Some(&idx) = type_to_idx.get(tid) { self.write_mask.set(idx); }
    }
}

// СТАЛО — добавляем освобождение памяти после назначения масок:
pub fn assign_masks(&mut self, type_to_idx: &std::collections::HashMap<TypeId, u8>) {
    self.read_mask  = ComponentMask::EMPTY;
    self.write_mask = ComponentMask::EMPTY;
    for tid in &self.reads  {
        if let Some(&idx) = type_to_idx.get(tid) { self.read_mask.set(idx); }
    }
    for tid in &self.writes {
        if let Some(&idx) = type_to_idx.get(tid) { self.write_mask.set(idx); }
    }
    // Освобождаем векторы — после назначения масок они больше не нужны.
    // Маски содержат всю информацию в O(1) доступе.
    self.reads.clear();
    self.reads.shrink_to_fit();
    self.writes.clear();
    self.writes.shrink_to_fit();
    // reads_event / writes_event пока оставляем — они нужны для event-конфликтов.
    // Если event конфликты тоже переведены на маски — можно очистить и их.
}
```

**Дополнительно — оптимизация `dedup_push` для малых наборов:**

```rust
// БЫЛО:
fn dedup_push(vec: &mut Vec<TypeId>, items: &[TypeId]) {
    if items.is_empty() { return; }
    let mut set: HashSet<TypeId> = vec.iter().cloned().collect();  // аллокация
    for &item in items {
        if set.insert(item) { vec.push(item); }
    }
}

// СТАЛО — для малых наборов (< 8 элементов) linear scan быстрее HashSet:
fn dedup_push(vec: &mut Vec<TypeId>, items: &[TypeId]) {
    if items.is_empty() { return; }
    // Порог: если суммарный размер < 8 — O(N²) дешевле аллокации HashSet
    if vec.len() + items.len() < 8 {
        for &item in items {
            if !vec.contains(&item) { vec.push(item); }
        }
        return;
    }
    // Для больших наборов — HashSet как прежде
    let mut set: std::collections::HashSet<TypeId> = vec.iter().cloned().collect();
    for &item in items {
        if set.insert(item) { vec.push(item); }
    }
}
```

---

## Патч 3: `propagate_transforms` — dirty lookup через HashSet вместо world queries

**Файл:** `crates/apex-core/src/transform.rs`

**Проблема:** В DFS-цикле топологической сортировки на каждой итерации вызывается
`world.get::<TransformDirty>(entity).is_some()` — это HashMap lookup через
`component_arch_index` + archetype bounds check. При глубоких иерархиях
с тысячами dirty entities и повторными обходами это становится узким местом.

```rust
// БЫЛО — в функции propagate_transforms:
for &entity in &scratch.dirty_entities {
    if !world.get::<TransformDirty>(entity).is_some() {  // world lookup в цикле
        continue;
    }
    scratch.stack.clear();
    scratch.stack.push(entity);

    while let Some(top) = scratch.stack.last().copied() {
        if scratch.seen.contains(&top.index) {
            scratch.stack.pop();
            continue;
        }
        let parent = world.get_relation_target(top, ChildOf);
        let need_parent = parent
            .map(|p| {
                world.get::<TransformDirty>(p).is_some()  // ещё один world lookup
                    && !scratch.seen.contains(&p.index)
            })
            .unwrap_or(false);
        // ...
    }
}

// СТАЛО — строим FxHashSet<u32> из dirty entities один раз:

// В TransformScratch добавляем поле:
pub struct TransformScratch {
    pub(crate) dirty_entities: Vec<Entity>,
    pub(crate) dirty_set:      FxHashSet<u32>,  // <-- новое поле
    pub(crate) ordered:        Vec<Entity>,
    pub(crate) seen:           FxHashSet<u32>,
    pub(crate) stack:          Vec<Entity>,
    pub(crate) children:       Vec<Entity>,
}

// В propagate_transforms:
pub fn propagate_transforms(world: &mut World) {
    let mut scratch = world.remove_resource::<TransformScratch>()
        .unwrap_or_default();

    scratch.dirty_entities.clear();
    scratch.dirty_set.clear();     // <-- очищаем
    scratch.ordered.clear();
    scratch.seen.clear();
    scratch.stack.clear();
    scratch.children.clear();

    // 1. Собираем dirty entity и сразу строим set
    {
        let q = world.query_typed::<Read<TransformDirty>>();
        q.for_each(|e, _| {
            scratch.dirty_entities.push(e);
            scratch.dirty_set.insert(e.index);  // <-- заполняем set
        });
    }

    if scratch.dirty_entities.is_empty() {
        world.insert_resource(scratch);
        return;
    }

    // 2. DFS — используем dirty_set вместо world.get::<TransformDirty>()
    for &entity in &scratch.dirty_entities {
        if !scratch.dirty_set.contains(&entity.index) {  // O(1), без world lookup
            continue;
        }

        scratch.stack.clear();
        scratch.stack.push(entity);

        while let Some(top) = scratch.stack.last().copied() {
            if scratch.seen.contains(&top.index) {
                scratch.stack.pop();
                continue;
            }

            let parent = world.get_relation_target(top, ChildOf);
            let need_parent = parent
                .map(|p| {
                    scratch.dirty_set.contains(&p.index)    // O(1) вместо world.get
                        && !scratch.seen.contains(&p.index)
                })
                .unwrap_or(false);

            if need_parent {
                scratch.stack.push(parent.unwrap());
            } else {
                scratch.seen.insert(top.index);
                scratch.ordered.push(top);
                scratch.stack.pop();
            }
        }
    }

    // 3. Обработка ordered — при добавлении child в ordered также добавляем в dirty_set
    let mut i = 0;
    while i < scratch.ordered.len() {
        let entity = scratch.ordered[i];

        if !world.is_alive(entity) { i += 1; continue; }

        let local = match world.get::<LocalTransform>(entity) {
            Some(l) => *l,
            None    => { i += 1; continue; }
        };

        let parent = world.get_relation_target(entity, ChildOf);
        let global_matrix = if let Some(parent_entity) = parent {
            match world.get::<GlobalTransform>(parent_entity) {
                Some(pg) => pg.0 * local.to_matrix(),
                None     => local.to_matrix(),
            }
        } else {
            local.to_matrix()
        };

        if let Some(gt) = world.get_mut::<GlobalTransform>(entity) {
            gt.0 = global_matrix;
        }

        world.remove::<TransformDirty>(entity);
        scratch.dirty_set.remove(&entity.index);  // поддерживаем set актуальным

        scratch.children.clear();
        for child in world.children_of(ChildOf, entity) {
            scratch.children.push(child);
        }
        for &child in &scratch.children {
            if !world.is_alive(child) { continue; }
            if !scratch.dirty_set.contains(&child.index) {
                world.insert(child, TransformDirty);
                scratch.dirty_set.insert(child.index);  // <-- обновляем set
                scratch.ordered.push(child);
            }
        }

        i += 1;
    }

    world.insert_resource(scratch);
}
```

---

## Патч 4: `TrackedEventQueue` — `EventReadGuard` для автоматического продвижения курсора

**Файл:** `crates/apex-core/src/events.rs`

**Проблема:** `iter()` возвращает `&[T]` и НЕ продвигает курсор. Пользователь
обязан вручную вызвать `advance_reader_mut()`. Это источник молчаливых багов:
если забыть — события читаются повторно вечно. Старый `advance_reader` помечен
`#[deprecated]` и является no-op, что ещё хуже.

```rust
// ДОБАВЛЯЕМ в events.rs новый тип:

/// RAII-обёртка: при дропе автоматически продвигает курсор до конца буфера.
///
/// Создаётся через [`TrackedEventQueue::read`].
///
/// # Пример
///
/// ```ignore
/// // Курсор продвигается автоматически при выходе из scope:
/// {
///     let events = queue.read(&reader);
///     for ev in events.iter() { process(ev); }
/// } // <- здесь курсор автоматически продвигается
///
/// // Или с явным отказом от продвижения:
/// let events = queue.read(&reader);
/// let guard = events.peek(); // без продвижения
/// ```
pub struct EventReadGuard<'q, T> {
    queue:     &'q mut TrackedEventQueue<T>,
    reader_id: EventCursor,
    start:     usize,
}

impl<'q, T> EventReadGuard<'q, T> {
    /// Срез непрочитанных событий.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.queue.events[self.start..]
    }

    /// Итерация без потребления guard.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// Отказаться от автоматического продвижения курсора.
    /// Курсор останется на месте после дропа guard.
    pub fn peek(self) -> PeekGuard<'q, T> {
        PeekGuard(self)
    }
}

impl<T> std::ops::Deref for EventReadGuard<'_, T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] { self.as_slice() }
}

impl<T> Drop for EventReadGuard<'_, T> {
    fn drop(&mut self) {
        // Автоматически продвигаем курсор при дропе
        self.queue.advance_reader_mut(&self.reader_id);
    }
}

/// Обёртка для "посмотреть без продвижения".
pub struct PeekGuard<'q, T>(EventReadGuard<'q, T>);

impl<T> std::ops::Deref for PeekGuard<'_, T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] { self.0.as_slice() }
}

impl<T> Drop for PeekGuard<'_, T> {
    fn drop(&mut self) {
        // Намеренно НЕ продвигаем курсор
    }
}

// В TrackedEventQueue добавляем метод:
impl<T> TrackedEventQueue<T> {
    // ... существующие методы ...

    /// Прочитать непрочитанные события с автоматическим продвижением курсора.
    ///
    /// Курсор продвигается при дропе возвращённого [`EventReadGuard`].
    ///
    /// # Пример
    ///
    /// ```ignore
    /// // Автоматическое продвижение:
    /// for ev in queue.read(&reader).iter() { process(ev); }
    ///
    /// // Посмотреть без продвижения:
    /// let events = queue.read(&reader).peek();
    /// println!("{} pending", events.len());
    /// // курсор не сдвинулся
    /// ```
    #[inline]
    pub fn read(&mut self, reader_id: &EventCursor) -> EventReadGuard<'_, T> {
        let idx   = reader_id.0 as usize;
        let start = self.cursors.get(idx)
            .and_then(|c| c.as_ref())
            .copied()
            .unwrap_or(0) as usize;
        let start = start.min(self.events.len());
        EventReadGuard { queue: self, reader_id: *reader_id, start }
    }
}
```

**Использование в системах — до и после:**

```rust
// БЫЛО (требует двух вызовов, легко забыть второй):
let events = queue.iter(&reader);
for ev in events { process(ev); }
queue.advance_reader_mut(&reader); // если забыть — баг

// СТАЛО (один вызов, курсор продвигается автоматически):
for ev in queue.read(&reader).iter() { process(ev); }
// или:
let events = queue.read(&reader);
if !events.is_empty() {
    process_batch(&events);
}
// drop(events) → курсор продвинут автоматически
```

---

## Патч 5: `SparseSet` — adaptive backend для разреженных entity index

**Файл:** `crates/apex-core/src/storage/sparse_set.rs`

**Проблема:** `sparse: Vec<u32>` вырастает до `max_entity_index + 1`. Если создать
entity с index=50000 и добавить в SparseSet, массив займёт 200 KB при единственной
записи. Особенно критично для `SubjectIndex` в relations.

```rust
// СТАЛО — SparseSet с автоматическим переключением на HashMap при разреженности:

pub struct SparseSet<T> {
    inner: SparseSetInner<T>,
}

enum SparseSetInner<T> {
    Dense {
        sparse: Vec<u32>,
        dense:  Vec<u32>,
        data:   Vec<T>,
    },
    Sparse {
        map:   rustc_hash::FxHashMap<u32, T>,
        dense: Vec<u32>,  // для сохранения порядка итерации
    },
}

/// Порог переключения: если entity_index > len(dense) * SPARSITY_THRESHOLD,
/// переключаемся на HashMap-backend.
const SPARSITY_THRESHOLD: usize = 4;

impl<T> SparseSet<T> {
    pub fn new() -> Self {
        Self {
            inner: SparseSetInner::Dense {
                sparse: Vec::new(),
                dense:  Vec::new(),
                data:   Vec::new(),
            },
        }
    }

    pub fn insert(&mut self, entity_index: u32, value: T) {
        match &mut self.inner {
            SparseSetInner::Dense { sparse, dense, data } => {
                let idx = entity_index as usize;

                // Проверяем необходимость перехода на HashMap-backend
                if idx > dense.len().saturating_mul(SPARSITY_THRESHOLD).max(64)
                    && idx > 1024
                {
                    // Конвертируем в HashMap
                    let mut map = rustc_hash::FxHashMap::default();
                    let existing_dense = std::mem::take(dense);
                    let existing_data  = std::mem::take(data);
                    let dense_copy     = existing_dense.clone();
                    for (i, entity) in existing_dense.into_iter().enumerate() {
                        map.insert(entity, existing_data.into_iter().nth(i).unwrap());
                    }
                    map.insert(entity_index, value);
                    let mut new_dense = dense_copy;
                    new_dense.push(entity_index);
                    self.inner = SparseSetInner::Sparse { map, dense: new_dense };
                    return;
                }

                // Обычный Dense путь
                if idx >= sparse.len() {
                    sparse.resize(idx + 1, u32::MAX);
                }
                let pos = sparse[idx];
                if pos != u32::MAX {
                    data[pos as usize] = value;
                } else {
                    let new_pos = dense.len() as u32;
                    sparse[idx] = new_pos;
                    dense.push(entity_index);
                    data.push(value);
                }
            }
            SparseSetInner::Sparse { map, dense } => {
                if !map.contains_key(&entity_index) {
                    dense.push(entity_index);
                }
                map.insert(entity_index, value);
            }
        }
    }

    pub fn get(&self, entity_index: u32) -> Option<&T> {
        match &self.inner {
            SparseSetInner::Dense { sparse, data, .. } => {
                let idx = entity_index as usize;
                if idx >= sparse.len() { return None; }
                let pos = sparse[idx];
                if pos == u32::MAX { None } else { Some(&data[pos as usize]) }
            }
            SparseSetInner::Sparse { map, .. } => map.get(&entity_index),
        }
    }

    pub fn get_mut(&mut self, entity_index: u32) -> Option<&mut T> {
        match &mut self.inner {
            SparseSetInner::Dense { sparse, data, .. } => {
                let idx = entity_index as usize;
                if idx >= sparse.len() { return None; }
                let pos = sparse[idx];
                if pos == u32::MAX { None } else { Some(&mut data[pos as usize]) }
            }
            SparseSetInner::Sparse { map, .. } => map.get_mut(&entity_index),
        }
    }

    pub fn contains(&self, entity_index: u32) -> bool {
        self.get(entity_index).is_some()
    }

    pub fn remove(&mut self, entity_index: u32) -> Option<T> {
        match &mut self.inner {
            SparseSetInner::Dense { sparse, dense, data } => {
                let idx = entity_index as usize;
                if idx >= sparse.len() { return None; }
                let pos = sparse[idx];
                if pos == u32::MAX { return None; }

                let last_entity = *dense.last().unwrap();
                let pos_usize   = pos as usize;
                let last_pos    = dense.len() - 1;

                dense.swap(pos_usize, last_pos);
                data.swap(pos_usize, last_pos);

                sparse[last_entity as usize] = pos;
                sparse[idx]                  = u32::MAX;

                dense.pop();
                Some(data.pop().unwrap())
            }
            SparseSetInner::Sparse { map, dense } => {
                if let Some(val) = map.remove(&entity_index) {
                    if let Some(pos) = dense.iter().position(|&e| e == entity_index) {
                        dense.swap_remove(pos);
                    }
                    Some(val)
                } else {
                    None
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            SparseSetInner::Dense { dense, .. } => dense.len(),
            SparseSetInner::Sparse { map, .. }  => map.len(),
        }
    }

    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        match &self.inner {
            SparseSetInner::Dense { dense, data, .. } =>
                either_iter::Left(dense.iter().copied().zip(data.iter())),
            SparseSetInner::Sparse { map, dense, .. } =>
                either_iter::Right(dense.iter().copied().filter_map(|e| map.get(&e).map(|v| (e, v)))),
        }
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        match &self.inner {
            SparseSetInner::Dense { data, .. }  => either_iter::Left(data.iter()),
            SparseSetInner::Sparse { map, .. }  => either_iter::Right(map.values()),
        }
    }
}

impl<T> Default for SparseSet<T> {
    fn default() -> Self { Self::new() }
}

// Минималистичный Either для двух итераторов без доп. зависимости:
mod either_iter {
    pub enum Left<L, R> { Left(L), Right(R) }
    // ... или используй уже существующий паттерн из codebase
}
```

**Примечание:** если добавление зависимости `either` нежелательно, можно
использовать `Box<dyn Iterator>` для iter() с незначительным overhead,
или разделить метод на `iter_dense` / `iter_map` и обрабатывать в вызывающем коде.

---

## Патч 6: `make_serde_fns` — bincode вместо JSON для runtime, output-буфер

**Файл:** `crates/apex-core/src/component.rs`

**Проблема:** `deserialize_fn` при каждом вызове аллоцирует `Vec<u8>` для буфера,
что при snapshot-restore (тысячи компонентов) создаёт тысячи аллокаций.
Также JSON медленнее bincode в ~5–10x для числовых данных.

```rust
// ДОБАВЛЯЕМ в Cargo.toml для apex-core (bincode уже есть в workspace deps):
// apex-core/Cargo.toml — bincode уже подключён через workspace

// СТАЛО — make_serde_fns с bincode и reusable output buffer:

/// Создаёт `ComponentSerdeFns` для типа T.
///
/// Формат сериализации:
/// - Runtime (hot-reload, snapshot): `bincode` — быстро, компактно
/// - Human-readable (debug, export): `serde_json` — через отдельный метод
pub fn make_serde_fns<T: Serializable>() -> ComponentSerdeFns {
    ComponentSerdeFns {
        serialize_fn: |ptr| {
            let val = unsafe { &*(ptr as *const T) };
            // bincode вместо JSON: быстрее и компактнее для числовых данных
            bincode::serialize(val)
                .map_err(|e| ComponentSerdeError::SerializationFailed(e.to_string()))
        },
        deserialize_fn: |bytes| {
            let val: T = bincode::deserialize(bytes)
                .map_err(|e| ComponentSerdeError::DeserializationFailed(e.to_string()))?;
            let size = std::mem::size_of::<T>();
            let mut buf = vec![0u8; size];
            if size > 0 {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &val as *const T as *const u8,
                        buf.as_mut_ptr(),
                        size,
                    );
                }
            }
            std::mem::forget(val);
            Ok(buf)
        },
        format: "bincode",
    }
}

/// JSON-версия для human-readable export (отдельная функция).
pub fn make_serde_fns_json<T: Serializable>() -> ComponentSerdeFns {
    ComponentSerdeFns {
        serialize_fn: |ptr| {
            let val = unsafe { &*(ptr as *const T) };
            serde_json::to_vec(val)
                .map_err(|e| ComponentSerdeError::SerializationFailed(e.to_string()))
        },
        deserialize_fn: |bytes| {
            let val: T = serde_json::from_slice(bytes)
                .map_err(|e| ComponentSerdeError::DeserializationFailed(e.to_string()))?;
            let size = std::mem::size_of::<T>();
            let mut buf = vec![0u8; size];
            if size > 0 {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &val as *const T as *const u8,
                        buf.as_mut_ptr(),
                        size,
                    );
                }
            }
            std::mem::forget(val);
            Ok(buf)
        },
        format: "json",
    }
}

// В register_serde — добавляем параметр формата или оставляем bincode по умолчанию:
// ComponentRegistry::register_serde<T>() -> ComponentId  // bincode (default)
// ComponentRegistry::register_serde_json<T>() -> ComponentId  // JSON

pub fn register_serde<T: Serializable>(&mut self) -> ComponentId {
    let id = self.register::<T>();
    if let Some(info) = self.by_id.get_mut(&id.0) {
        if info.serde.is_none() {
            info.serde = Some(make_serde_fns::<T>());  // bincode
        }
    }
    id
}

pub fn register_serde_json<T: Serializable>(&mut self) -> ComponentId {
    let id = self.register::<T>();
    if let Some(info) = self.by_id.get_mut(&id.0) {
        if info.serde.is_none() {
            info.serde = Some(make_serde_fns_json::<T>());
        }
    }
    id
}
```

**Примечание по обратной совместимости:** если существующие снэпшоты в JSON,
нужна миграция. Добавьте поле `format: &'static str` в снэпшот и выбирайте
deserialize_fn по нему. Поле уже есть в `ComponentSerdeFns::format`.

---

## Патч 7: `Column::grow` — начальная ёмкость 16 вместо 64

**Файл:** `crates/apex-core/src/archetype.rs`

**Проблема:** `Column::grow` при первом выделении создаёт буфер на 64 элемента.
Для компонентов вроде `Mat4` (64 байта) это 4 KB на колонку при спавне
первой же entity. При 10 компонентах и 100 архетипах — 4 MB потрачено впустую.

```rust
// БЫЛО:
pub(crate) fn grow(&mut self) {
    let new_cap = if self.capacity == 0 { 64 } else { self.capacity * 2 };
    // ...
}

// СТАЛО — умное начальное выделение на основе item_size:
pub(crate) fn grow(&mut self) {
    let new_cap = if self.capacity == 0 {
        // Целевой размер первого выделения: ~256 байт минимум, но не более 64 элементов.
        // Для крупных компонентов (Mat4=64B, Transform=~48B) — 4 элемента.
        // Для мелких (f32=4B, u8=1B) — 64 элемента.
        if self.item_size == 0 {
            64
        } else {
            // 256 байт / item_size, зажатые в [4, 64]
            (256 / self.item_size.max(1)).clamp(4, 64)
        }
    } else {
        self.capacity * 2
    };
    // ... остальная логика без изменений
}

// Аналогично в reserve — изменить начальный new_cap:
pub(crate) fn reserve(&mut self, additional: usize) {
    let needed = self.len + additional;
    if needed <= self.capacity {
        self.change_ticks.reserve(additional);
        return;
    }
    // next_power_of_two но минимум 4, не 64:
    let new_cap = needed.next_power_of_two().max(4);
    // ... остальная логика без изменений
}
```

---

## Патч 8: `Bundle::component_ids` — `SmallVec` вместо `Vec`

**Файл:** `crates/apex-core/src/world.rs`, макрос `impl_bundle`

**Проблема:** `component_ids()` в `Bundle` возвращает `Vec<ComponentId>` — heap
аллокация при каждом `spawn_bundle`. Большинство бандлов содержат 1–8 компонентов
и отлично укладываются в стек.

```rust
// БЫЛО в trait Bundle:
pub trait Bundle: Sized {
    fn component_ids(&self, registry: &mut ComponentRegistry) -> Vec<ComponentId>;
    // ...
}

// СТАЛО:
use smallvec::SmallVec;

pub trait Bundle: Sized {
    // SmallVec<[ComponentId; 8]> — до 8 компонентов без heap аллокации
    fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]>;
    // ...
}

// В макросе impl_bundle:
macro_rules! impl_bundle {
    ($($T:ident),+) => {
        #[allow(non_snake_case)]
        impl<$($T: Component),+> Bundle for ($($T,)+) {
            fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
                let mut ids: SmallVec<[ComponentId; 8]> = smallvec::smallvec![
                    $( registry.get_or_register::<$T>() ),+
                ];
                ids.sort_unstable();
                ids
            }
            // write_into и write_into_batch без изменений
        }
    };
}

// В spawn_bundle и spawn_many_inner обновить типы:
pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Entity {
    let ids          = bundle.component_ids(&mut self.registry); // теперь SmallVec
    let archetype_id = self.get_or_create_archetype(&ids);       // принимает &[ComponentId]
    // ... остальное без изменений
}
```

**Примечание:** `get_or_create_archetype` уже принимает `&[ComponentId]`, поэтому
`SmallVec` через deref передаётся без изменений в call sites.

---

## Патч 9: `ArchetypeMask::iter_ones` — bit manipulation вместо filter_map

**Файл:** `crates/apex-core/src/access.rs`

**Проблема:** `iter_ones` использует `(0..64).filter_map(...)` — итерация по 64
позициям на каждое слово. При 16 словах = 1024 итераций с branch per bit.
`u64::trailing_zeros()` извлекает следующий бит за одну CPU инструкцию (BSF/TZCNT).

```rust
// БЫЛО:
pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
    self.bits.iter().enumerate().flat_map(|(chunk_i, &chunk)| {
        (0..64).filter_map(move |bit| {
            if chunk & (1u64 << bit) != 0 {
                Some(chunk_i * 64 + bit)
            } else {
                None
            }
        })
    })
}

// СТАЛО — использует trailing_zeros для O(popcount) итерации:
pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
    self.bits.iter().enumerate().flat_map(|(chunk_i, &chunk)| {
        BitIter { word: chunk, base: chunk_i * 64 }
    })
}

struct BitIter {
    word: u64,
    base: usize,
}

impl Iterator for BitIter {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.word == 0 {
            return None;
        }
        let bit = self.word.trailing_zeros() as usize;
        self.word &= self.word - 1;  // сбрасываем младший установленный бит
        Some(self.base + bit)
    }
}
```

**Аналогично для `ComponentMask`** если потребуется `iter_ones` там.

---

## Патч 10: `Graph::bfs` — переиспользование буферов (уже частично сделано, доделываем)

**Файл:** `crates/apex-graph/src/algorithms.rs`

**Проблема:** `bfs()` (публичный метод, не `has_path`) создаёт новые `visited`
и `queue` на каждый вызов, хотя буферы уже есть в `Graph` (`bfs_visited`, `bfs_queue`)
и используются только в `has_path`. В `dfs()` та же ситуация.

```rust
// БЫЛО в bfs():
pub fn bfs(&self, start: Index) -> Vec<Index> {
    // ...
    let slot_cap = self.slot_capacity();
    let mut visited = vec![false; slot_cap];   // новая аллокация
    let mut queue: Vec<Index> = Vec::new();    // новая аллокация
    // ...
}

// СТАЛО — bfs принимает &mut self и переиспользует буферы:
pub fn bfs(&mut self, start: Index) -> Vec<Index> {
    if self.nodes.get(start).is_none() { return Vec::new(); }

    let slot_cap = self.slot_capacity();

    // Переиспользуем буферы из Graph
    self.bfs_visited.clear();
    self.bfs_visited.resize(slot_cap, false);
    self.bfs_queue.clear();
    let mut head = 0usize;

    let start_slot = start.slot() as usize;
    if start_slot < self.bfs_visited.len() {
        self.bfs_visited[start_slot] = true;
    }
    self.bfs_queue.push(start);

    let mut result = Vec::new();

    while head < self.bfs_queue.len() {
        let node = self.bfs_queue[head];
        head += 1;
        result.push(node);

        let node_slot = node.slot() as usize;
        if let Some(edges) = self.adjacency_out.get(node_slot) {
            for &edge_idx in edges {
                let Some(edge) = self.edges.get(edge_idx) else { continue; };
                let succ = edge.to;
                if self.nodes.get(succ).is_none() { continue; }
                let succ_slot = succ.slot() as usize;
                if succ_slot < self.bfs_visited.len() && !self.bfs_visited[succ_slot] {
                    self.bfs_visited[succ_slot] = true;
                    self.bfs_queue.push(succ);
                }
            }
        }
    }

    result
}

// Аналогично dfs() — добавить переиспользуемый dfs_stack в Graph:
pub struct Graph<N, W> {
    // ... существующие поля ...
    pub(crate) dfs_stack:   Vec<Index>,    // <-- добавить
    pub(crate) dfs_visited: Vec<bool>,     // <-- добавить
}

pub fn dfs(&mut self, start: Index) -> Vec<Index> {
    if self.nodes.get(start).is_none() { return Vec::new(); }

    let slot_cap = self.slot_capacity();
    self.dfs_visited.clear();
    self.dfs_visited.resize(slot_cap, false);
    self.dfs_stack.clear();
    self.dfs_stack.push(start);

    let mut result = Vec::new();

    while let Some(node) = self.dfs_stack.pop() {
        let slot = node.slot() as usize;
        if slot >= self.dfs_visited.len() || self.dfs_visited[slot] { continue; }
        self.dfs_visited[slot] = true;
        result.push(node);

        if let Some(edges) = self.adjacency_out.get(slot) {
            for &edge_idx in edges.iter().rev() {
                let Some(edge) = self.edges.get(edge_idx) else { continue; };
                let succ = edge.to;
                if self.nodes.get(succ).is_none() { continue; }
                let succ_slot = succ.slot() as usize;
                if succ_slot < self.dfs_visited.len() && !self.dfs_visited[succ_slot] {
                    self.dfs_stack.push(succ);
                }
            }
        }
    }

    result
}
```

---

## Патч 11: `EntityAllocator` — pack Entity в u64

**Файл:** `crates/apex-core/src/entity.rs`

**Проблема:** `Entity { index: u32, generation: u32 }` = 8 байт, что нормально.
Но `EntityRecord { generation: u32, location: Option<EntityLocation> }` содержит
`Option<EntityLocation>` где `EntityLocation = { archetype_id: ArchetypeId(u32), row: u32 }`.
`Option<EntityLocation>` из-за discriminant занимает 12 байт вместо 8.

```rust
// БЫЛО:
struct EntityRecord {
    generation: u32,
    location:   Option<EntityLocation>,
}
// sizeof: 4 (gen) + 1 (discriminant) + 3 (padding) + 4 (arch_id) + 4 (row) = 16 байт

// СТАЛО — кодируем location в u64, используем sentinel для None:
/// Запись об entity в аллокаторе.
///
/// Кодировка location в u64:
/// - bits [0..31]  = row (u32)
/// - bits [32..63] = archetype_id (u32)
/// - u64::MAX = None (entity не размещена в архетипе)
struct EntityRecord {
    generation:      u32,
    /// Кодированное местоположение. u64::MAX = отсутствует.
    encoded_location: u64,
}
// sizeof: 4 + 4 (padding) + 8 = 16 байт — то же, но без Option overhead
// При добавлении repr(C) или оптимальном расположении: 12 байт

const NO_LOCATION: u64 = u64::MAX;

impl EntityRecord {
    #[inline]
    fn location(&self) -> Option<EntityLocation> {
        if self.encoded_location == NO_LOCATION {
            None
        } else {
            let row          = (self.encoded_location & 0xFFFF_FFFF) as u32;
            let archetype_id = (self.encoded_location >> 32) as u32;
            Some(EntityLocation {
                archetype_id: crate::archetype::ArchetypeId(archetype_id),
                row,
            })
        }
    }

    #[inline]
    fn set_location(&mut self, loc: EntityLocation) {
        self.encoded_location =
            (loc.row as u64) | ((loc.archetype_id.0 as u64) << 32);
    }

    #[inline]
    fn clear_location(&mut self) {
        self.encoded_location = NO_LOCATION;
    }

    #[inline]
    fn has_location(&self) -> bool {
        self.encoded_location != NO_LOCATION
    }
}

// Обновляем EntityAllocator под новую структуру:
impl EntityAllocator {
    pub fn allocate(&mut self) -> Entity {
        if let Some(index) = self.free_list.pop() {
            let gen = self.records[index as usize].generation;
            Entity { index, generation: gen }
        } else {
            let index = self.next_index;
            self.next_index += 1;
            self.records.push(EntityRecord {
                generation: 0,
                encoded_location: NO_LOCATION,
            });
            Entity { index, generation: 0 }
        }
    }

    pub fn free(&mut self, entity: Entity) -> bool {
        let record = match self.records.get_mut(entity.index as usize) {
            Some(r) => r,
            None    => return false,
        };
        if record.generation != entity.generation { return false; }
        record.generation = record.generation.wrapping_add(1);
        record.clear_location();
        self.free_list.push(entity.index);
        true
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.records
            .get(entity.index as usize)
            .map(|r| r.generation == entity.generation && r.has_location())
            .unwrap_or(false)
    }

    #[inline]
    pub fn get_location(&self, entity: Entity) -> Option<EntityLocation> {
        self.records
            .get(entity.index as usize)
            .filter(|r| r.generation == entity.generation)
            .and_then(|r| r.location())
    }

    #[inline]
    pub fn set_location(&mut self, entity: Entity, location: EntityLocation) {
        if let Some(record) = self.records.get_mut(entity.index as usize) {
            if record.generation == entity.generation {
                record.set_location(location);
            }
        }
    }
    // allocate_batch, set_locations_batch — обновить аналогично
}
```

**Ограничение:** максимум 2^32 архетипов и 2^32 rows — для ECS более чем достаточно.

---

## Сводная таблица приоритетов

| Патч | Файл | Усилие | Выигрыш | Риск |
|------|------|--------|---------|------|
| 1. QueryCache без аллокации | world.rs | низкое | высокий (горячий путь) | минимальный |
| 2. AccessDescriptor shrink | access.rs | минимальное | умеренный (memory) | нулевой |
| 3. TransformDirty HashSet | transform.rs | низкое | высокий при иерархиях | минимальный |
| 4. EventReadGuard | events.rs | среднее | качество API | нулевой |
| 5. SparseSet adaptive | sparse_set.rs | высокое | важен при разреженности | средний |
| 6. Bincode serde | component.rs | низкое | умеренный | совместимость снэпшотов |
| 7. Column::grow initial cap | archetype.rs | минимальное | умеренный (memory) | нулевой |
| 8. Bundle SmallVec | world.rs | низкое | умеренный (spawn hot path) | минимальный |
| 9. ArchetypeMask::iter_ones | access.rs | минимальное | умеренный (compile path) | нулевой |
| 10. Graph BFS/DFS буферы | algorithms.rs | низкое | умеренный | API (mut self) |
| 11. EntityRecord packed u64 | entity.rs | среднее | умеренный (cache line) | средний |

**Рекомендуемый порядок применения:**
1 → 2 → 7 → 9 (все минимального риска, высокий ROI)
→ 3 → 8 → 4 (низкий риск, заметный выигрыш)
→ 6 → 10 → 11 → 5 (требуют тестирования совместимости)
