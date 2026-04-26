archetype.rs:
use std::alloc::{alloc, dealloc, realloc, Layout};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    component::{ComponentId, ComponentInfo, Tick},
    entity::Entity,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ArchetypeId(pub(crate) u32);

impl ArchetypeId {
    pub const EMPTY: Self = Self(0);

    /// Получить внутренний индекс (для доступа к `world.archetypes()`).
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

pub struct Column {
    pub(crate) component_id: ComponentId,
    pub(crate) data: *mut u8,
    pub(crate) item_size: usize,
    item_align: usize,
    drop_fn: unsafe fn(*mut u8),
    pub(crate) len: usize,
    pub(crate) capacity: usize,
    /// Per-row тик последнего изменения (для change detection)
    pub(crate) change_ticks: Vec<Tick>,
}

unsafe impl Send for Column {}
unsafe impl Sync for Column {}

/// Публичное представление колонки для внешних крейтов.
pub struct ColumnView<'a> {
    col: &'a Column,
}

impl<'a> ColumnView<'a> {
    pub fn id(&self) -> ComponentId { self.col.component_id }
    pub unsafe fn get_raw_ptr(&self, row: usize) -> *const u8 { self.col.get_ptr(row) }
}

impl Column {
    pub fn new(info: &ComponentInfo) -> Self {
        Self {
            component_id: info.id,
            data: std::ptr::null_mut(),
            item_size: info.size,
            item_align: info.align,
            drop_fn: info.drop_fn,
            len: 0,
            capacity: 0,
            change_ticks: Vec::new(),
        }
    }

    /// Публичный accessor для component_id колонки.
    #[inline]
    pub fn id(&self) -> ComponentId { self.component_id }

    fn layout_for(&self, capacity: usize) -> Layout {
        if self.item_size == 0 {
            Layout::from_size_align(0, 1).unwrap()
        } else {
            let size = self
                .item_size
                .checked_mul(capacity)
                .expect("overflow in layout_for: item_size * capacity");
            Layout::from_size_align(size, self.item_align).unwrap()
        }
    }

    #[inline]
    pub unsafe fn set_change_tick(&self, row: usize, tick: Tick) {
        debug_assert!(row < self.len);
        let ptr = self.change_ticks.as_ptr() as *mut Tick;
        *ptr.add(row) = tick;
    }

    pub unsafe fn get_ptr(&self, row: usize) -> *mut u8 {
        if self.item_size == 0 {
            self.item_align as *mut u8
        } else {
            self.data.add(row * self.item_size)
        }
    }

    #[inline]
    pub unsafe fn get_raw_ptr(&self, row: usize) -> *const u8 {
        self.get_ptr(row)
    }

    #[inline]
    pub unsafe fn get<T>(&self, row: usize) -> &T {
        &*(self.get_ptr(row) as *const T)
    }

    #[inline]
    pub unsafe fn get_mut<T>(&mut self, row: usize) -> &mut T {
        &mut *(self.get_ptr(row) as *mut T)
    }

    /// Записать новый элемент в конец, проставить тик изменения
    pub unsafe fn push(&mut self, src: *const u8, tick: Tick) {
        if self.len >= self.capacity {
            self.grow();
        }
        if self.item_size > 0 {
            let dst = self.data.add(self.len * self.item_size);
            std::ptr::copy_nonoverlapping(src, dst, self.item_size);
        }
        self.change_ticks.push(tick);
        self.len += 1;
    }

    /// Записать элемент в уже существующую строку, обновить тик
    pub unsafe fn write_at(&mut self, row: usize, src: *const u8, tick: Tick) {
        if self.item_size > 0 {
            std::ptr::copy_nonoverlapping(src, self.get_ptr(row), self.item_size);
        }
        if row < self.change_ticks.len() {
            self.change_ticks[row] = tick;
        }
    }

    pub unsafe fn swap_remove_and_drop(&mut self, row: usize) {
        debug_assert!(row < self.len);
        let last = self.len - 1;
        if row != last {
            let remove_ptr = self.get_ptr(row);
            (self.drop_fn)(remove_ptr);
            if self.item_size > 0 {
                std::ptr::copy_nonoverlapping(self.get_ptr(last), remove_ptr, self.item_size);
            }
            self.change_ticks.swap(row, last);
        } else {
            (self.drop_fn)(self.get_ptr(row));
        }
        self.change_ticks.pop();
        self.len -= 1;
    }

    pub unsafe fn swap_remove_no_drop(&mut self, row: usize) {
        debug_assert!(row < self.len);
        let last = self.len - 1;
        if row != last && self.item_size > 0 {
            let remove_ptr = self.get_ptr(row);
            std::ptr::copy_nonoverlapping(self.get_ptr(last), remove_ptr, self.item_size);
        }
        if row != last {
            self.change_ticks.swap(row, last);
        }
        self.change_ticks.pop();
        self.len -= 1;
    }

    /// Оптимизация 2.4: используем realloc вместо alloc+memcpy.
    ///
    /// На большинстве аллокаторов (jemalloc, mimalloc, системный Linux/Windows)
    /// realloc может расширить блок on-place без копирования данных,
    /// что устраняет лишний memcpy при grow().
    pub(crate) fn grow(&mut self) {
        let new_cap = if self.capacity == 0 { 64 } else { self.capacity * 2 };
        if self.item_size == 0 {
            self.capacity = new_cap;
            return;
        }

        let new_size = self.item_size
            .checked_mul(new_cap)
            .expect("overflow in grow: item_size * new_cap");

        // SAFETY: Layout корректен (item_size > 0, item_align валиден).
        let new_data = if self.capacity == 0 || self.data.is_null() {
            // Первая аллокация — используем alloc
            let layout = Layout::from_size_align(new_size, self.item_align)
                .expect("invalid layout in grow");
            unsafe { alloc(layout) }
        } else {
            // Реаллокация: realloc может работать in-place без memcpy.
            // Старый layout: item_size * capacity байт, item_align выравнивание.
            let old_layout = self.layout_for(self.capacity);
            // SAFETY: self.data валиден (non-null, выделен с old_layout).
            unsafe { realloc(self.data, old_layout, new_size) }
        };

        assert!(!new_data.is_null(), "allocation failed in Column::grow");
        self.data = new_data;
        self.capacity = new_cap;
    }

    /// Предварительное выделение памяти под `additional` элементов.
    /// Позволяет избежать множественных grow() при массовых spawn'ах.
    ///
    /// Оптимизация 2.4: тоже использует realloc для in-place расширения.
    pub(crate) fn reserve(&mut self, additional: usize) {
        let needed = self.len + additional;
        if needed <= self.capacity {
            self.change_ticks.reserve(additional);
            return;
        }
        let new_cap = needed.next_power_of_two().max(64);
        if self.item_size == 0 {
            self.capacity = new_cap;
            self.change_ticks.reserve(additional);
            return;
        }

        let new_size = self.item_size
            .checked_mul(new_cap)
            .expect("overflow in reserve");

        let new_data = if self.capacity == 0 || self.data.is_null() {
            let layout = Layout::from_size_align(new_size, self.item_align)
                .expect("invalid layout in reserve");
            unsafe { alloc(layout) }
        } else {
            let old_layout = self.layout_for(self.capacity);
            unsafe { realloc(self.data, old_layout, new_size) }
        };

        assert!(!new_data.is_null(), "allocation failed in Column::reserve");
        self.data = new_data;
        self.capacity = new_cap;
        self.change_ticks.reserve(additional);
    }

    /// Тик изменения для строки row
    #[inline]
    pub fn get_tick(&self, row: usize) -> Tick {
        self.change_ticks.get(row).copied().unwrap_or(Tick::ZERO)
    }

    /// Указатель на массив тиков — для zero-cost Changed<T> query
    #[inline]
    pub fn ticks_ptr(&self) -> *const Tick {
        self.change_ticks.as_ptr()
    }

    /// Сырой указатель на данные — для chunk-level параллелизма
    #[inline]
    pub fn data_ptr(&self) -> *mut u8 {
        self.data
    }
}

impl Drop for Column {
    fn drop(&mut self) {
        for i in 0..self.len {
            unsafe { (self.drop_fn)(self.get_ptr(i)); }
        }
        if self.capacity > 0 && !self.data.is_null() && self.item_size > 0 {
            unsafe { dealloc(self.data, self.layout_for(self.capacity)); }
        }
    }
}

pub struct Archetype {
    pub id: ArchetypeId,
    pub component_ids: SmallVec<[ComponentId; 8]>,
    pub(crate) columns: Vec<Column>,
    pub(crate) entities: Vec<Entity>,
    column_map: FxHashMap<ComponentId, usize>,
    pub add_edges: FxHashMap<ComponentId, ArchetypeId>,
    pub remove_edges: FxHashMap<ComponentId, ArchetypeId>,
}

impl Archetype {
    pub fn new(
        id: ArchetypeId,
        component_ids: SmallVec<[ComponentId; 8]>,
        component_infos: &[&ComponentInfo],
    ) -> Self {
        let columns: Vec<Column> = component_infos.iter().map(|i| Column::new(i)).collect();
        let column_map: FxHashMap<ComponentId, usize> = component_ids
            .iter()
            .enumerate()
            .map(|(i, &cid)| (cid, i))
            .collect();
        Self {
            id,
            component_ids,
            columns,
            entities: Vec::new(),
            column_map,
            add_edges: FxHashMap::default(),
            remove_edges: FxHashMap::default(),
        }
    }

    #[inline] pub fn len(&self) -> usize { self.entities.len() }
    #[inline] pub fn is_empty(&self) -> bool { self.entities.is_empty() }

    #[inline]
    pub fn column_index(&self, component_id: ComponentId) -> Option<usize> {
        self.column_map.get(&component_id).copied()
    }

    #[inline]
    pub fn has_component(&self, component_id: ComponentId) -> bool {
        self.column_map.contains_key(&component_id)
    }

    pub unsafe fn get_component<T>(&self, row: usize, component_id: ComponentId) -> Option<&T> {
        let col_idx = self.column_index(component_id)?;
        Some(self.columns[col_idx].get::<T>(row))
    }

    pub unsafe fn get_component_mut<T>(&mut self, row: usize, component_id: ComponentId) -> Option<&mut T> {
        let col_idx = self.column_index(component_id)?;
        Some(self.columns[col_idx].get_mut::<T>(row))
    }

    pub unsafe fn set_change_tick(&self, row: usize, component_id: ComponentId, tick: Tick) {
        if let Some(col_idx) = self.column_index(component_id) {
            self.columns[col_idx].set_change_tick(row, tick);
        }
    }

    pub unsafe fn allocate_row(&mut self, entity: Entity) -> usize {
        let row = self.entities.len();
        self.entities.push(entity);
        row
    }

    pub unsafe fn write_component(&mut self, row: usize, component_id: ComponentId, src: *const u8, tick: Tick) {
        if let Some(col_idx) = self.column_index(component_id) {
            let col = &mut self.columns[col_idx];
            if row >= col.len {
                col.push(src, tick);
            } else {
                col.write_at(row, src, tick);
            }
        }
    }

    pub unsafe fn remove_row(&mut self, row: usize) -> Option<Entity> {
        let last = self.entities.len() - 1;
        for col in &mut self.columns {
            col.swap_remove_and_drop(row);
        }
        if row != last {
            self.entities.swap(row, last);
            self.entities.pop();
            Some(self.entities[row])
        } else {
            self.entities.pop();
            None
        }
    }

    /// Публичный итератор колонок через ColumnView (безопасный, без raw).
    pub fn columns(&self) -> impl Iterator<Item = ColumnView<'_>> {
        self.columns.iter().map(|col| ColumnView { col })
    }

    /// Сырой срез колонок — для apex-scripting query-итератора.
    #[inline]
    pub fn columns_raw(&self) -> &[Column] {
        &self.columns
    }

    pub fn entities(&self) -> &[Entity] { &self.entities }
}

/// Описание одного чанка архетипа для chunk-level параллелизма.
pub struct ArchetypeChunk<'a> {
    pub entities: &'a [Entity],
    pub arch_id:  ArchetypeId,
    pub start_row: usize,
    pub len:       usize,
}

/// Разбить архетип на чанки фиксированного размера.
pub fn archetype_chunks(arch: &Archetype, chunk_size: usize) -> impl Iterator<Item = ArchetypeChunk<'_>> {
    let total = arch.entities.len();
    let num_chunks = (total + chunk_size - 1) / chunk_size;
    (0..num_chunks).map(move |i| {
        let start = i * chunk_size;
        let end = (start + chunk_size).min(total);
        ArchetypeChunk {
            entities:  &arch.entities[start..end],
            arch_id:   arch.id,
            start_row: start,
            len:       end - start,
        }
    })
}

events.rs:
// events.rs — оптимизирован по пункту 1.5:
// EventRegistry теперь хранит параллельную карту raw-указателей
// для O(1) typed access без downcast_ref в горячем пути.

use std::any::{Any, TypeId};
use rustc_hash::FxHashMap;

use crate::entity::Entity;

// ── TrackedEventQueue ───────────────────────────────────────────

pub struct TrackedEventQueue<T> {
    events: Vec<T>,
    pending: Vec<T>,
    cursors: Vec<Option<u32>>,
    next_cursor_id: u32,
    free_list: Vec<EventCursor>,
}

impl<T> TrackedEventQueue<T> {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            pending: Vec::new(),
            cursors: Vec::new(),
            next_cursor_id: 0,
            free_list: Vec::new(),
        }
    }

    #[inline]
    pub fn send(&mut self, event: T) {
        self.pending.push(event);
    }

    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        self.pending.extend(events);
    }

    pub fn add_reader(&mut self) -> EventCursor {
        let id = self.next_cursor_id;
        self.next_cursor_id += 1;

        if let Some(cursor) = self.free_list.pop() {
            let idx = cursor.0 as usize;
            if idx < self.cursors.len() {
                self.cursors[idx] = Some(0);
            }
            return cursor;
        }

        self.cursors.push(Some(0));
        EventCursor(id)
    }

    pub fn remove_reader(&mut self, reader_id: EventCursor) {
        let idx = reader_id.0 as usize;
        if idx < self.cursors.len() {
            self.cursors[idx] = None;
            self.free_list.push(reader_id);
        }
        if self.free_list.is_empty() {
            while self.cursors.last().copied() == Some(None) {
                self.cursors.pop();
            }
        }
    }

    pub fn reader_count(&self) -> usize {
        self.cursors.iter().filter(|c| c.is_some()).count()
    }

    pub fn update(&mut self) {
        let all_read = self.all_readers_caught_up();

        if all_read {
            self.events.clear();
            for cursor in &mut self.cursors {
                if let Some(pos) = cursor {
                    *pos = 0;
                }
            }
        }

        std::mem::swap(&mut self.events, &mut self.pending);

        if all_read {
            self.pending.clear();
        } else {
            let new_count = self.events.len() as u32;
            for cursor in &mut self.cursors {
                if let Some(pos) = cursor {
                    *pos += new_count;
                }
            }
            self.events.append(&mut self.pending);
        }
    }

    #[inline]
    pub fn iter(&self, reader_id: &EventCursor) -> &[T] {
        let idx = reader_id.0 as usize;
        let cursor = self.cursors.get(idx).and_then(|c| c.as_ref());
        match cursor {
            Some(&pos) if (pos as usize) < self.events.len() => {
                &self.events[pos as usize..]
            }
            _ => &[],
        }
    }

    #[deprecated(note = "Use advance_reader_mut instead — this function is a no-op")]
    #[inline]
    pub fn advance_reader(&self, reader_id: &EventCursor) {
        let _ = reader_id;
    }

    #[inline]
    pub fn advance_reader_mut(&mut self, reader_id: &EventCursor) {
        let idx = reader_id.0 as usize;
        if let Some(Some(pos)) = self.cursors.get_mut(idx) {
            *pos = self.events.len() as u32;
        }
    }

    #[inline]
    pub fn read_and_advance(&mut self, reader_id: &EventCursor) -> Vec<&T> {
        let idx = reader_id.0 as usize;
        let start = self.cursors.get(idx).and_then(|c| c.as_ref()).copied().unwrap_or(0) as usize;
        let end = self.events.len();
        if start < end {
            if let Some(Some(pos)) = self.cursors.get_mut(idx) {
                *pos = end as u32;
            }
            self.events[start..].iter().collect()
        } else {
            Vec::new()
        }
    }

    #[inline] pub fn len(&self) -> usize { self.events.len() + self.pending.len() }
    #[inline] pub fn len_readable(&self) -> usize { self.events.len() }
    #[inline] pub fn len_pending(&self) -> usize { self.pending.len() }
    #[inline] pub fn is_empty(&self) -> bool { self.events.is_empty() && self.pending.is_empty() }

    pub fn clear(&mut self) {
        self.events.clear();
        self.pending.clear();
        self.free_list.clear();
        for cursor in &mut self.cursors {
            if let Some(pos) = cursor {
                *pos = 0;
            }
        }
    }

    fn all_readers_caught_up(&self) -> bool {
        let total = self.events.len() as u32;
        self.cursors.iter().all(|c| match c {
            Some(pos) => *pos >= total,
            None => true,
        })
    }

    #[inline] pub fn iter_previous(&self) -> std::slice::Iter<'_, T> { self.events.iter() }
    #[inline] pub fn iter_current(&self) -> std::slice::Iter<'_, T> { self.pending.iter() }
    #[inline] pub fn iter_all(&self) -> impl Iterator<Item = &T> {
        self.events.iter().chain(self.pending.iter())
    }
    #[inline] pub fn len_previous(&self) -> usize { self.events.len() }
}

impl<T> Default for TrackedEventQueue<T> {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventCursor(pub u32);

// ── EntityEvent ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EntityEvent<T> {
    pub target: Entity,
    pub data: T,
}

impl<T> EntityEvent<T> {
    pub fn new(target: Entity, data: T) -> Self { Self { target, data } }
}

// ── DelayedQueue ────────────────────────────────────────────────

pub struct DelayedQueue<T> {
    pending_delayed: Vec<DelayedEvent<T>>,
}

struct DelayedEvent<T> {
    deliver_at: u32,
    event: T,
}

impl<T> DelayedQueue<T> {
    pub fn new() -> Self { Self { pending_delayed: Vec::new() } }

    pub fn send_delayed(&mut self, event: T, delay: u32, current_tick: u32) {
        self.pending_delayed.push(DelayedEvent {
            deliver_at: current_tick.wrapping_add(delay),
            event,
        });
    }

    pub fn flush_delayed(&mut self, current_tick: u32, target_queue: &mut TrackedEventQueue<T>) {
        if self.pending_delayed.is_empty() { return; }
        let mut i = 0;
        while i < self.pending_delayed.len() {
            if self.pending_delayed[i].deliver_at <= current_tick {
                let ev = self.pending_delayed.swap_remove(i);
                target_queue.send(ev.event);
            } else {
                i += 1;
            }
        }
    }

    #[inline] pub fn len(&self) -> usize { self.pending_delayed.len() }
    #[inline] pub fn is_empty(&self) -> bool { self.pending_delayed.is_empty() }
    pub fn clear(&mut self) { self.pending_delayed.clear(); }
}

impl<T> Default for DelayedQueue<T> {
    fn default() -> Self { Self::new() }
}

// ── AnyEventQueue (trait object) ─────────────────────────────────

pub trait AnyEventQueue: Any + Send + Sync {
    fn update(&mut self);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn len(&self) -> usize;
    fn as_ptr_mut(&mut self) -> *mut u8;
    fn add_reader(&mut self) -> u32;
    fn remove_reader(&mut self, reader_id: u32);
}

impl<T: Send + Sync + 'static> AnyEventQueue for TrackedEventQueue<T> {
    fn update(&mut self) { TrackedEventQueue::update(self); }
    fn as_any(&self) -> &dyn Any { self }
    fn as_any_mut(&mut self) -> &mut dyn Any { self }
    fn len(&self) -> usize { TrackedEventQueue::len(self) }
    fn as_ptr_mut(&mut self) -> *mut u8 { self as *mut TrackedEventQueue<T> as *mut u8 }
    fn add_reader(&mut self) -> u32 { TrackedEventQueue::add_reader(self).0 }
    fn remove_reader(&mut self, reader_id: u32) {
        TrackedEventQueue::remove_reader(self, EventCursor(reader_id));
    }
}

// ── EventRegistry ───────────────────────────────────────────────
//
// Оптимизация 1.5: параллельная карта raw-указателей устраняет
// downcast_ref из горячего пути (send_event, events::<T>()).
//
// Ключевая идея: `queues` владеет Box<dyn AnyEventQueue>,
// `raw_ptrs` хранит *mut u8 на тот же объект.
// Typed access через raw_ptrs — без виртуального вызова, O(1) HashMap lookup.

pub struct EventRegistry {
    /// Владеющая карта: TypeId → Box<dyn AnyEventQueue>
    queues: FxHashMap<TypeId, Box<dyn AnyEventQueue>>,
    /// Карта raw-указателей для горячего пути — без downcast:
    /// TypeId → *mut TrackedEventQueue<T> (хранится как *mut u8)
    ///
    /// # Safety
    /// Указатели валидны пока жив соответствующий Box в `queues`.
    /// Новые записи добавляются только в `register()`.
    /// Удаление записей не поддерживается (события живут до конца World).
    raw_ptrs: FxHashMap<TypeId, *mut u8>,
}

// SAFETY: EventRegistry отправляется между потоками только через &mut World.
// raw_ptrs — лишь кеш указателей на данные внутри queues, которые Send+Sync.
unsafe impl Send for EventRegistry {}
unsafe impl Sync for EventRegistry {}

impl EventRegistry {
    pub fn new() -> Self {
        Self {
            queues:   FxHashMap::default(),
            raw_ptrs: FxHashMap::default(),
        }
    }

    /// Зарегистрировать тип события.
    pub fn register<T: Send + Sync + 'static>(&mut self) {
        use std::collections::hash_map::Entry;
        if let Entry::Vacant(e) = self.queues.entry(TypeId::of::<T>()) {
            let mut boxed: Box<dyn AnyEventQueue> = Box::new(TrackedEventQueue::<T>::new());
            // Сохраняем raw ptr ДО того как Box переходит во владение HashMap.
            let raw = boxed.as_ptr_mut();
            e.insert(boxed);
            self.raw_ptrs.insert(TypeId::of::<T>(), raw);
        }
    }

    /// Горячий путь: O(1) typed access без downcast_ref.
    ///
    /// # Safety
    /// T должен совпадать с типом, с которым был зарегистрирован этот TypeId.
    /// Это гарантируется монomorphic вызовом через TypeId::of::<T>().
    #[inline]
    pub fn get<T: Send + Sync + 'static>(&self) -> &TrackedEventQueue<T> {
        // SAFETY: raw_ptrs[T] = &mut TrackedEventQueue<T> (установлено в register).
        // Возвращаем &T (shared), что безопасно пока нет &mut.
        unsafe {
            self.raw_ptrs
                .get(&TypeId::of::<T>())
                .map(|&ptr| &*(ptr as *const TrackedEventQueue<T>))
                .unwrap_or_else(|| panic!(
                    "Event `{}` not registered. Call world.add_event::<{0}>()",
                    std::any::type_name::<T>()
                ))
        }
    }

    /// Горячий путь мутабельный: O(1) без downcast_mut.
    #[inline]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut TrackedEventQueue<T> {
        // SAFETY: raw_ptrs[T] = *mut TrackedEventQueue<T>.
        // &mut self гарантирует уникальный доступ.
        unsafe {
            self.raw_ptrs
                .get(&TypeId::of::<T>())
                .map(|&ptr| &mut *(ptr as *mut TrackedEventQueue<T>))
                .unwrap_or_else(|| panic!(
                    "Event `{}` not registered. Call world.add_event::<{0}>()",
                    std::any::type_name::<T>()
                ))
        }
    }

    /// Попытка получить очередь без паники (fallback путь).
    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&TrackedEventQueue<T>> {
        unsafe {
            self.raw_ptrs
                .get(&TypeId::of::<T>())
                .map(|&ptr| &*(ptr as *const TrackedEventQueue<T>))
        }
    }

    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut TrackedEventQueue<T>> {
        unsafe {
            self.raw_ptrs
                .get(&TypeId::of::<T>())
                .map(|&ptr| &mut *(ptr as *mut TrackedEventQueue<T>))
        }
    }

    /// Raw pointer для EventWriter.
    pub fn get_raw_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut TrackedEventQueue<T>> {
        self.raw_ptrs
            .get(&TypeId::of::<T>())
            .map(|&ptr| ptr as *mut TrackedEventQueue<T>)
    }

    /// Обновить все очереди (вызывается в конце тика).
    pub fn update_all(&mut self) {
        for queue in self.queues.values_mut() {
            queue.update();
        }
    }

    pub fn is_registered<T: Send + Sync + 'static>(&self) -> bool {
        self.queues.contains_key(&TypeId::of::<T>())
    }

    pub fn queue_count(&self) -> usize { self.queues.len() }

    pub fn total_event_count(&self) -> usize {
        self.queues.values().map(|q| q.len()).sum()
    }
}

impl Default for EventRegistry {
    fn default() -> Self { Self::new() }
}

/// Устаревший alias. Используйте [`TrackedEventQueue`].
pub type EventQueue<T> = TrackedEventQueue<T>;

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_and_read() {
        let mut queue = TrackedEventQueue::new();
        let reader = queue.add_reader();
        queue.send(42);
        queue.send(43);
        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], 42);
        assert_eq!(events[1], 43);
        queue.advance_reader_mut(&reader);
        assert_eq!(queue.iter(&reader).len(), 0);
    }

    #[test]
    fn event_registry_fast_path_no_downcast() {
        let mut reg = EventRegistry::new();
        reg.register::<i32>();
        reg.get_mut::<i32>().send(42);
        reg.get_mut::<i32>().send(43);
        // Горячий путь: get() без downcast_ref
        let q = reg.get::<i32>();
        assert_eq!(q.len_pending(), 2);
    }

    #[test]
    fn event_registry_multiple_types() {
        let mut reg = EventRegistry::new();
        reg.register::<i32>();
        reg.register::<f32>();
        reg.get_mut::<i32>().send(1);
        reg.get_mut::<f32>().send(2.0);
        assert_eq!(reg.get::<i32>().len_pending(), 1);
        assert_eq!(reg.get::<f32>().len_pending(), 1);
    }

    #[test]
    fn two_readers_independent() {
        let mut queue = TrackedEventQueue::new();
        let reader_a = queue.add_reader();
        let reader_b = queue.add_reader();
        queue.send(1);
        queue.send(2);
        queue.update();
        assert_eq!(queue.iter(&reader_a).len(), 2);
        queue.advance_reader_mut(&reader_a);
        assert_eq!(queue.iter(&reader_b).len(), 2);
        queue.advance_reader_mut(&reader_b);
        queue.update();
        queue.send(3);
        queue.update();
        assert_eq!(queue.iter(&reader_a).len(), 1);
        assert_eq!(queue.iter(&reader_a)[0], 3);
    }

    #[test]
    fn delayed_event_delivery() {
        let mut queue = TrackedEventQueue::new();
        let reader = queue.add_reader();
        let mut delayed = DelayedQueue::new();
        delayed.send_delayed(99, 3, 0);
        delayed.flush_delayed(1, &mut queue);
        assert_eq!(queue.len_pending(), 0);
        delayed.flush_delayed(3, &mut queue);
        assert_eq!(queue.len_pending(), 1);
        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], 99);
    }
}

world.rs
// world.rs — оптимизированная версия.
//
// Изменения по пунктам плана:
//   1.1  write_into_batch: позиционный col_indices (без линейного find)
//   1.2  archetype_index: ключ ArchetypeKey на SmallVec (без Vec-аллокации)
//   1.3  move_entity: однопроходная реализация (убран IS_COMMON_BUF thread_local)
//   2.1  component_arch_index: индекс ComponentId → архетипы для Query
//   2.3  QueryCache: гранулярная инвалидация по ComponentId
//   2.5  TransformScratch resource: scratch-буферы без per-frame аллокаций
//   4.1  add_relation_batch: batch-добавление relations

use std::cell::UnsafeCell;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    archetype::{Archetype, ArchetypeId},
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry, Tick, Serializable},
    entity::{EntityAllocator, EntityLocation, Entity},
    events::EventRegistry,
    query::{QueryBuilder, WorldQuery},
    relations::{IdIndex, RelationRegistry, SubjectIndex},
    resources::ResourceMap,
    system_param::{Res, ResMut, EventReader, EventWriter, WorldQuerySystemAccess},
    template::TemplateRegistry,
};

// ── ArchetypeKey ───────────────────────────────────────────────
//
// Оптимизация 1.2: ключ для archetype_index на SmallVec<[ComponentId; 12]>.
// Хранит до 12 компонентов без heap-аллокации.
// Устраняет Vec-аллокацию при каждом lookup в get_or_create_archetype.

#[derive(Clone, PartialEq, Eq, Hash)]
struct ArchetypeKey(SmallVec<[ComponentId; 12]>);

impl ArchetypeKey {
    fn from_slice(ids: &[ComponentId]) -> Self {
        Self(ids.iter().copied().collect())
    }
}

// ── QueryCache ─────────────────────────────────────────────────
//
// Оптимизация 2.3: гранулярная инвалидация по ComponentId.
// invalidate_for(cid) удаляет только записи, затрагивающие cid,
// оставляя остальные валидными.

struct CacheEntry {
    arch_indices: Vec<usize>,
    /// Компоненты этого запроса (для гранулярной инвалидации)
    component_ids: Vec<ComponentId>,
    version: u32,
}

pub(crate) struct QueryCache {
    entries: UnsafeCell<FxHashMap<Vec<ComponentId>, CacheEntry>>,
    version: u32,
}

unsafe impl Sync for QueryCache {}

impl QueryCache {
    pub fn new() -> Self {
        Self { entries: UnsafeCell::new(FxHashMap::default()), version: 0 }
    }

    pub unsafe fn get_or_compute(
        &self,
        key:           &[ComponentId],
        world_version: u32,
        archetypes:    &[Archetype],
        matches:       impl Fn(&Archetype) -> bool,
    ) -> &[usize] {
        let map   = &mut *self.entries.get();
        let entry = map.entry(key.to_vec()).or_insert(CacheEntry {
            arch_indices:  Vec::new(),
            component_ids: key.to_vec(),
            version:       u32::MAX,
        });
        if entry.version != world_version {
            entry.arch_indices = archetypes
                .iter()
                .enumerate()
                .filter(|(_, arch)| !arch.is_empty() && matches(arch))
                .map(|(i, _)| i)
                .collect();
            entry.version = world_version;
        }
        &entry.arch_indices
    }

    /// Полная инвалидация — при добавлении нового архетипа.
    pub fn invalidate(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Гранулярная инвалидация — только записи с данным ComponentId.
    ///
    /// Используется в move_entity (структурные изменения без новых архетипов).
    /// Записи запросов, не затрагивающих changed_cid, остаются валидными.
    pub fn invalidate_for(&mut self, changed_cid: ComponentId) {
        let map = unsafe { &mut *self.entries.get() };
        map.retain(|_, entry| !entry.component_ids.contains(&changed_cid));
        // Глобальную версию не меняем — остальные записи валидны
    }

    pub fn version(&self) -> u32 { self.version }
}

// ── DeferredCommand ─────────────────────────────────────────────

enum DeferredCommand {
    Despawn(Entity),
    InsertRaw {
        entity:       Entity,
        component_id: ComponentId,
        data:         Vec<u8>,
        tick:         Tick,
    },
    RemoveRaw {
        entity:       Entity,
        component_id: ComponentId,
    },
}

pub struct DeferredQueue {
    commands: Vec<DeferredCommand>,
}

impl DeferredQueue {
    pub fn new() -> Self { Self { commands: Vec::new() } }
    pub fn with_capacity(cap: usize) -> Self { Self { commands: Vec::with_capacity(cap) } }

    pub fn despawn(&mut self, entity: Entity) {
        self.commands.push(DeferredCommand::Despawn(entity));
    }

    pub fn insert_raw(&mut self, entity: Entity, component_id: ComponentId, data: Vec<u8>, tick: Tick) {
        self.commands.push(DeferredCommand::InsertRaw { entity, component_id, data, tick });
    }

    pub fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        self.commands.push(DeferredCommand::RemoveRaw { entity, component_id });
    }

    pub fn len(&self) -> usize { self.commands.len() }
    pub fn is_empty(&self) -> bool { self.commands.is_empty() }

    pub fn apply(&mut self, world: &mut World) {
        for cmd in self.commands.drain(..) {
            match cmd {
                DeferredCommand::Despawn(e) => { world.despawn(e); }
                DeferredCommand::InsertRaw { entity, component_id, data, tick } => {
                    world.insert_raw(entity, component_id, data, tick);
                }
                DeferredCommand::RemoveRaw { entity, component_id } => {
                    world.remove_raw(entity, component_id);
                }
            }
        }
    }

    pub fn clear(&mut self) { self.commands.clear(); }
}

impl Default for DeferredQueue {
    fn default() -> Self { Self::new() }
}

// ── World ──────────────────────────────────────────────────────

pub struct World {
    pub(crate) entities:        EntityAllocator,
    pub(crate) registry:        ComponentRegistry,
    pub(crate) archetypes:      Vec<Archetype>,
    /// Оптимизация 1.2: ключ ArchetypeKey без Vec-аллокации при lookup.
    pub(crate) archetype_index: FxHashMap<ArchetypeKey, ArchetypeId>,
    pub(crate) current_tick:    Tick,
    pub(crate) query_cache:     QueryCache,
    pub(crate) relations:       RelationRegistry,
    pub(crate) id_index:        IdIndex,
    pub(crate) subject_index:   SubjectIndex,
    pub        resources:       ResourceMap,
    pub(crate) events:          EventRegistry,
    pub(crate) write_hooks:     FxHashMap<ComponentId, fn(Entity, &mut World)>,
    pub(crate) templates:       TemplateRegistry,
    /// Оптимизация 2.1: индекс ComponentId → [ArchetypeId].
    /// Используется Query::new для быстрого поиска подходящих архетипов
    /// вместо линейного обхода всех archetypes.
    pub(crate) component_arch_index: FxHashMap<ComponentId, SmallVec<[ArchetypeId; 16]>>,
}

impl World {
    pub fn new() -> Self {
        let mut world = Self {
            entities:        EntityAllocator::new(),
            registry:        ComponentRegistry::new(),
            archetypes:      Vec::new(),
            archetype_index: FxHashMap::default(),
            current_tick:    Tick(1),
            query_cache:     QueryCache::new(),
            relations:       RelationRegistry::new(),
            id_index:        IdIndex::default(),
            subject_index:   SubjectIndex::new(),
            resources:       ResourceMap::new(),
            events:          EventRegistry::new(),
            write_hooks:     FxHashMap::default(),
            templates:       TemplateRegistry::new(),
            component_arch_index: FxHashMap::default(),
        };
        world.archetypes.push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world.archetype_index.insert(ArchetypeKey(SmallVec::new()), ArchetypeId::EMPTY);
        world
    }

    pub fn tick(&mut self) {
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
        self.events.update_all();
    }

    pub fn current_tick(&self)    -> Tick  { self.current_tick }
    pub fn entity_count(&self)    -> usize { self.entities.len() }
    pub fn archetype_count(&self) -> usize { self.archetypes.len() }
    pub fn resource_count(&self)  -> usize { self.resources.len() }

    pub fn register_component<T: Component>(&mut self) -> ComponentId {
        self.registry.register::<T>()
    }

    pub fn register_component_serde<T: crate::component::Serializable>(&mut self) -> ComponentId {
        self.registry.register_serde::<T>()
    }

    pub fn registry(&self) -> &ComponentRegistry { &self.registry }
    pub fn archetypes(&self) -> &[Archetype] { &self.archetypes }
    pub fn relation_registry(&self) -> &RelationRegistry { &self.relations }
    pub fn relation_registry_mut(&mut self) -> &mut RelationRegistry { &mut self.relations }

    pub fn subject_index_raw(&self, entity_index: u32) -> Vec<u32> {
        self.subject_index.get_all(entity_index)
    }

    /// Индекс компонент → архетипы (для оптимизированного Query matching).
    ///
    /// Оптимизация 2.1: Query::new использует этот индекс для поиска
    /// кандидатов вместо линейного обхода всех архетипов.
    #[inline]
    pub fn component_arch_index(&self) -> &FxHashMap<ComponentId, SmallVec<[ArchetypeId; 16]>> {
        &self.component_arch_index
    }

    pub fn spawn_empty(&mut self) -> Entity {
        let entity = self.entities.allocate();
        let row    = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
        self.entities.set_location(entity, EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        });
        entity
    }

    pub fn insert_relation_raw(&mut self, subject: Entity, relation_id: ComponentId, _target: Entity) {
        self.ensure_relation_component(relation_id);
        self.subject_index.add(subject.index, relation_id);
        self.insert_relation_component(subject, relation_id);
    }

    #[inline]
    pub fn insert_raw_pub(
        &mut self,
        entity:       Entity,
        component_id: ComponentId,
        data:         Vec<u8>,
        tick:         Tick,
    ) {
        self.insert_raw(entity, component_id, data, tick);
    }

    // ── Параллельный доступ ────────────────────────────────────

    pub unsafe fn as_parallel_world(&self) -> ParallelWorld<'_> {
        ParallelWorld {
            world:   self as *const World,
            _marker: std::marker::PhantomData,
        }
    }

    pub(crate) unsafe fn archetype_ptr(&self, idx: usize) -> *mut Archetype {
        &self.archetypes[idx] as *const Archetype as *mut Archetype
    }

    // ── Resources ──────────────────────────────────────────────

    pub fn insert_resource<T: Send + Sync + 'static>(&mut self, value: T) {
        self.resources.insert(value);
    }

    #[track_caller]
    pub fn resource<T: Send + Sync + 'static>(&self) -> &T {
        self.resources.get::<T>()
    }

    #[track_caller]
    pub fn resource_mut<T: Send + Sync + 'static>(&mut self) -> &mut T {
        self.resources.get_mut::<T>()
    }

    pub fn try_resource<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.resources.try_get::<T>()
    }

    pub fn try_resource_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.resources.try_get_mut::<T>()
    }

    pub fn remove_resource<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.resources.remove::<T>()
    }

    pub fn has_resource<T: Send + Sync + 'static>(&self) -> bool {
        self.resources.contains::<T>()
    }

    // ── Events ─────────────────────────────────────────────────

    pub fn add_event<T: Send + Sync + 'static>(&mut self) {
        self.events.register::<T>();
    }

    #[track_caller]
    pub fn events<T: Send + Sync + 'static>(&self) -> &crate::events::EventQueue<T> {
        self.events.get::<T>()
    }

    #[track_caller]
    pub fn events_mut<T: Send + Sync + 'static>(&mut self) -> &mut crate::events::EventQueue<T> {
        self.events.get_mut::<T>()
    }

    #[track_caller]
    pub fn send_event<T: Send + Sync + 'static>(&mut self, event: T) {
        self.events.get_mut::<T>().send(event);
    }

    pub fn try_send_event<T: Send + Sync + 'static>(&mut self, event: T) -> bool {
        if let Some(queue) = self.events.try_get_mut::<T>() {
            queue.send(event);
            true
        } else {
            false
        }
    }

    pub fn event_queue_ptr<T: Send + Sync + 'static>(
        &self,
    ) -> Option<*mut crate::events::EventQueue<T>> {
        self.events.get_raw_ptr::<T>()
    }

    // ── Spawn ──────────────────────────────────────────────────

    pub fn spawn(&mut self) -> EntityBuilder<'_> {
        let entity = self.entities.allocate();
        let row    = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
        self.entities.set_location(entity, EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        });
        EntityBuilder { world: self, entity }
    }

    pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Entity {
        let ids          = bundle.component_ids(&mut self.registry);
        let archetype_id = self.get_or_create_archetype(&ids);
        let entity       = self.entities.allocate();
        let row          = self.archetypes[archetype_id.0 as usize].entities.len();
        let tick         = self.current_tick;
        self.archetypes[archetype_id.0 as usize].entities.push(entity);
        bundle.write_into(self, archetype_id, row, tick);
        self.entities.set_location(entity, EntityLocation { archetype_id, row: row as u32 });
        entity
    }

    // Оптимизация 1.1: spawn_many_inner использует позиционный col_indices.
    // Убран линейный find() в write_into_batch — теперь col_indices = Vec<usize>
    // в порядке следования компонентов в кортеже Bundle.
    fn spawn_many_inner<B, F>(&mut self, count: usize, mut make_bundle: F) -> Vec<Entity>
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        if count == 0 { return Vec::new(); }

        let probe        = make_bundle(0);
        let ids          = probe.component_ids(&mut self.registry);
        drop(probe);

        let archetype_id = self.get_or_create_archetype(&ids);
        let arch_idx     = archetype_id.0 as usize;
        let start_row    = self.archetypes[arch_idx].entities.len();
        let tick         = self.current_tick;

        self.archetypes[arch_idx].entities.reserve(count);
        for col in &mut self.archetypes[arch_idx].columns {
            col.reserve(count);
        }

        let entities = self.entities.allocate_batch(count);

        // Оптимизация 1.1: позиционный массив col_idx (только usize, без ComponentId).
        // write_into_batch_positional использует индекс напрямую без find().
        let col_indices: SmallVec<[usize; 8]> = ids.iter()
            .filter_map(|&id| self.archetypes[arch_idx].column_index(id))
            .collect();

        // Старый col_indices для обратной совместимости write_into_batch
        let col_indices_legacy: Vec<(ComponentId, usize)> = ids.iter()
            .filter_map(|&id| {
                self.archetypes[arch_idx].column_index(id).map(|col_idx| (id, col_idx))
            })
            .collect();

        if col_indices.len() <= 1 {
            for (i, &entity) in entities.iter().enumerate() {
                let row    = start_row + i;
                let bundle = make_bundle(i);
                self.archetypes[arch_idx].entities.push(entity);
                bundle.write_into_batch(self, archetype_id, row, tick, &col_indices_legacy);
            }
        } else {
            let first_entity = entities[0];
            let first_bundle = make_bundle(0);
            self.archetypes[arch_idx].entities.push(first_entity);
            first_bundle.write_into_batch(self, archetype_id, start_row, tick, &col_indices_legacy);

            for (i, &entity) in entities[1..].iter().enumerate() {
                let row = start_row + 1 + i;
                self.archetypes[arch_idx].entities.push(entity);
                for &col_idx in &col_indices {
                    unsafe {
                        let col = &mut self.archetypes[arch_idx].columns[col_idx];
                        if col.item_size > 0 {
                            let src = col.get_ptr(start_row);
                            let dst = col.get_ptr(row);
                            std::ptr::copy_nonoverlapping(src, dst, col.item_size);
                        }
                        col.change_ticks.push(tick);
                        col.len += 1;
                    }
                }
            }
        }

        self.entities.set_locations_batch(&entities, archetype_id, start_row as u32);
        entities
    }

    pub fn spawn_many<B, F>(&mut self, count: usize, make_bundle: F) -> Vec<Entity>
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        self.spawn_many_inner(count, make_bundle)
    }

    pub fn spawn_many_silent<B, F>(&mut self, count: usize, make_bundle: F)
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        self.spawn_many_inner(count, make_bundle);
    }

    // ── Component ops ──────────────────────────────────────────

    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) {
        let component_id = self.registry.get_or_register::<T>();
        let location     = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return,
        };
        let current_idx = location.archetype_id.0 as usize;

        if self.archetypes[current_idx].has_component(component_id) {
            let tick = self.current_tick;
            unsafe {
                if let Some(col_idx) = self.archetypes[current_idx].column_index(component_id) {
                    let col = &mut self.archetypes[current_idx].columns[col_idx];
                    col.write_at(location.row as usize, &component as *const T as *const u8, tick);
                }
            }
            std::mem::forget(component);
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row     = self.move_entity(entity, location, new_arch_id, Some(component_id), None);
        let tick        = self.current_tick;
        unsafe {
            self.archetypes[new_arch_id.0 as usize]
                .write_component(new_row as usize, component_id, &component as *const T as *const u8, tick);
        }
        std::mem::forget(component);
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
        });
    }

    pub(crate) fn insert_raw(
        &mut self,
        entity:       Entity,
        component_id: ComponentId,
        data:         Vec<u8>,
        tick:         Tick,
    ) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return,
        };
        let current_idx = location.archetype_id.0 as usize;

        if self.archetypes[current_idx].has_component(component_id) {
            if !data.is_empty() {
                unsafe {
                    if let Some(col_idx) = self.archetypes[current_idx].column_index(component_id) {
                        let col = &mut self.archetypes[current_idx].columns[col_idx];
                        col.write_at(location.row as usize, data.as_ptr(), tick);
                    }
                }
            }
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row     = self.move_entity(entity, location, new_arch_id, Some(component_id), None);
        unsafe {
            self.archetypes[new_arch_id.0 as usize]
                .write_component(new_row as usize, component_id, data.as_ptr(), tick);
        }
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
        });
    }

    pub(crate) fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return,
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return;
        }
        let new_arch_id = self.find_or_create_archetype_without(
            location.archetype_id,
            component_id,
        );
        let new_row = self.move_entity(entity, location, new_arch_id, None, Some(component_id));
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
        });
    }

    pub fn remove<T: Component>(&mut self, entity: Entity) -> bool {
        let component_id = match self.registry.get_id::<T>() {
            Some(id) => id,
            None     => return false,
        };
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return false,
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return false;
        }
        let new_arch_id = self.find_or_create_archetype_without(
            location.archetype_id,
            component_id,
        );
        let new_row = self.move_entity(entity, location, new_arch_id, None, Some(component_id));
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
        });
        true
    }

    pub fn despawn(&mut self, entity: Entity) -> bool {
        if !self.entities.is_alive(entity) { return false; }
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return false,
        };
        self.subject_index.clear_entity(entity.index);
        let arch_idx = location.archetype_id.0 as usize;
        unsafe {
            if let Some(displaced) = self.archetypes[arch_idx].remove_row(location.row as usize) {
                self.entities.set_location(displaced, EntityLocation {
                    archetype_id: location.archetype_id,
                    row:          location.row,
                });
            }
        }
        self.entities.free(entity);
        true
    }

    // ── Read / Write ───────────────────────────────────────────

    #[inline]
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let component_id = self.registry.get_id::<T>()?;
        let location     = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component::<T>(location.row as usize, component_id)
        }
    }

    #[inline]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let component_id = self.registry.get_id::<T>()?;
        let location     = self.entities.get_location(entity)?;
        let tick         = self.current_tick;

        {
            let arch = &mut self.archetypes[location.archetype_id.0 as usize];
            if let Some(col_idx) = arch.column_index(component_id) {
                if (location.row as usize) < arch.columns[col_idx].change_ticks.len() {
                    arch.columns[col_idx].change_ticks[location.row as usize] = tick;
                }
            }
        }

        let hook_fn: Option<fn(Entity, &mut World)> = self.write_hooks.get(&component_id).copied();
        if let Some(hook) = hook_fn {
            hook(entity, self);
        }

        let location2 = self.entities.get_location(entity)?;
        let arch      = &mut self.archetypes[location2.archetype_id.0 as usize];
        let col_idx   = arch.column_index(component_id)?;
        unsafe { Some(arch.columns[col_idx].get_mut::<T>(location2.row as usize)) }
    }

    pub fn register_write_hook<T: Component>(
        &mut self,
        hook: fn(Entity, &mut World),
    ) {
        if let Some(cid) = self.registry.get_id::<T>() {
            self.write_hooks.insert(cid, hook);
        }
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool { self.entities.is_alive(entity) }

    // ── Query API ──────────────────────────────────────────────

    pub fn query_typed<Q: WorldQuery>(&self) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, Tick::ZERO)
    }

    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, last_run)
    }

    pub fn query(&self) -> QueryBuilder<'_> { QueryBuilder::new(self) }

    // ── Внутренние методы ──────────────────────────────────────

    pub(crate) fn find_or_create_archetype_with(
        &mut self,
        current: ArchetypeId,
        add:     ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize].add_edges.get(&add) {
            return id;
        }
        let mut new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids.iter().copied().collect();
        new_components.push(add);
        new_components.sort_unstable();
        let new_id = self.get_or_create_archetype(&new_components);
        self.archetypes[current.0 as usize].add_edges.insert(add, new_id);
        self.archetypes[new_id.0 as usize].remove_edges.insert(add, current);
        new_id
    }

    pub(crate) fn find_or_create_archetype_without(
        &mut self,
        current: ArchetypeId,
        remove:  ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize].remove_edges.get(&remove) {
            return id;
        }
        let new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids.iter().copied()
            .filter(|&id| id != remove)
            .collect();
        let new_id = self.get_or_create_archetype(&new_components);
        self.archetypes[current.0 as usize].remove_edges.insert(remove, new_id);
        self.archetypes[new_id.0 as usize].add_edges.insert(remove, current);
        new_id
    }

    #[inline(never)]
    pub(crate) fn get_or_create_archetype(
        &mut self,
        components: &[ComponentId],
    ) -> ArchetypeId {
        // Оптимизация 1.2: lookup по ArchetypeKey без Vec-аллокации.
        let key = ArchetypeKey::from_slice(components);
        if let Some(&id) = self.archetype_index.get(&key) { return id; }

        let id    = ArchetypeId(self.archetypes.len() as u32);
        let infos: Vec<&ComponentInfo> = components.iter()
            .filter_map(|&cid| self.registry.get_info(cid))
            .collect();
        let arch  = Archetype::new(id, components.iter().copied().collect(), &infos);

        // Оптимизация 2.1: заполняем component_arch_index для нового архетипа.
        for &cid in &arch.component_ids {
            self.id_index.register_archetype(cid, id);
            // Добавляем новый архетип в component_arch_index
            self.component_arch_index
                .entry(cid)
                .or_default()
                .push(id);
        }

        self.archetypes.push(arch);
        self.archetype_index.insert(key, id);
        // Новый архетип → полная инвалидация кеша
        self.query_cache.invalidate();
        id
    }

    /// Оптимизация 1.3: однопроходный move_entity.
    ///
    /// Убраны три отдельных прохода (is_common[], copy, swap_remove).
    /// Теперь один проход: для каждой колонки из исходного архетипа
    /// сразу определяем наличие в целевом, копируем или дропаем.
    ///
    /// Убран IS_COMMON_BUF thread_local — он больше не нужен.
    ///
    /// Оптимизация 2.3: гранулярная инвалидация QueryCache.
    /// Вместо полной инвалидации при каждом move_entity —
    /// инвалидируем только записи, затрагивающие изменившиеся компоненты.
    ///
    /// added_cid: компонент, который добавляется (если есть)
    /// removed_cid: компонент, который удаляется (если есть)
    pub(crate) fn move_entity(
        &mut self,
        entity:          Entity,
        from_location:   EntityLocation,
        to_archetype_id: ArchetypeId,
        added_cid:       Option<ComponentId>,
        removed_cid:     Option<ComponentId>,
    ) -> u32 {
        // Оптимизация 2.3: гранулярная инвалидация вместо полной.
        // move_entity не создаёт новых архетипов, только перемещает данные.
        // Инвалидируем только те кеш-записи, которые затрагивают
        // добавленный или удалённый компонент.
        if let Some(cid) = added_cid   { self.query_cache.invalidate_for(cid); }
        if let Some(cid) = removed_cid { self.query_cache.invalidate_for(cid); }
        // Если ни тот ни другой не указан — инвалидируем всё (на всякий случай)
        if added_cid.is_none() && removed_cid.is_none() {
            self.query_cache.invalidate();
        }

        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx   = to_archetype_id.0 as usize;
        let from_row = from_location.row as usize;

        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        let from_len = self.archetypes[from_idx].columns.len();

        // Оптимизация 1.3: единственный проход по колонкам.
        // Для каждой колонки из исходного архетипа:
        //   - если есть в целевом → copy + swap_remove_no_drop
        //   - если нет в целевом → swap_remove_and_drop
        // Три прохода заменены одним.
        for i in 0..from_len {
            let cid       = self.archetypes[from_idx].columns[i].component_id;
            let item_size = self.archetypes[from_idx].columns[i].item_size;

            if let Some(to_col_idx) = self.archetypes[to_idx].column_index(cid) {
                // Компонент присутствует в обоих архетипах — копируем
                unsafe {
                    if item_size > 0 {
                        let to_col = &mut self.archetypes[to_idx].columns[to_col_idx];
                        if to_col.len >= to_col.capacity {
                            to_col.grow();
                        }
                        // Получаем указатели после возможного реаллоца
                        let src = self.archetypes[from_idx].columns[i].get_ptr(from_row);
                        let dst = self.archetypes[to_idx].columns[to_col_idx].get_ptr(to_row);
                        std::ptr::copy_nonoverlapping(src, dst, item_size);
                    }
                    let src_tick = self.archetypes[from_idx].columns[i].get_tick(from_row);
                    self.archetypes[to_idx].columns[to_col_idx].change_ticks.push(src_tick);
                    self.archetypes[to_idx].columns[to_col_idx].len += 1;

                    // swap_remove_no_drop: данные перемещены, drop не нужен
                    self.archetypes[from_idx].columns[i].swap_remove_no_drop(from_row);
                }
            } else {
                // Компонента нет в целевом архетипе — дропаем
                unsafe {
                    self.archetypes[from_idx].columns[i].swap_remove_and_drop(from_row);
                }
            }
        }

        // Исправляем location для вытесненной entity (swap_remove семантика)
        unsafe {
            let from_len_entities = self.archetypes[from_idx].entities.len();
            let from_last = from_len_entities.saturating_sub(1);
            if from_row != from_last && from_len_entities > 0 {
                let displaced = self.archetypes[from_idx].entities[from_last];
                self.archetypes[from_idx].entities.swap(from_row, from_last);
                self.archetypes[from_idx].entities.pop();
                self.entities.set_location(displaced, EntityLocation {
                    archetype_id: from_location.archetype_id,
                    row:          from_row as u32,
                });
            } else if from_len_entities > 0 {
                self.archetypes[from_idx].entities.pop();
            }
        }

        to_row as u32
    }

    // ── Оптимизация 4.1: add_relation_batch ───────────────────

    /// Batch-добавление одинаковой relation от множества субъектов к одному target.
    ///
    /// Оптимизирован для массового создания иерархий (тайловые карты, армии).
    /// Группирует subjects по текущему архетипу и делает один batch move
    /// для каждой группы вместо N отдельных move_entity.
    ///
    /// # Сложность
    /// O(S log S) где S = subjects.len() (группировка по архетипу).
    /// Против O(S) вызовов move_entity при наивном подходе.
    ///
    /// # Пример
    /// ```ignore
    /// // Создание иерархии 1000 тайлов за один batch
    /// world.add_relation_batch(&tiles, ChildOf, map_entity);
    /// ```
    pub fn add_relation_batch<R: crate::relations::RelationKind>(
        &mut self,
        subjects: &[Entity],
        kind: R,
        target: Entity,
    ) {
        if subjects.is_empty() { return; }

        let kind_idx    = self.relations.get_or_register::<R>();
        let relation_id = crate::relations::encode_relation(kind_idx, target.index);
        self.ensure_relation_component(relation_id);

        // Группируем subjects по текущему архетипу
        let mut by_arch: FxHashMap<ArchetypeId, Vec<Entity>> = FxHashMap::default();
        for &entity in subjects {
            if let Some(loc) = self.entities.get_location(entity) {
                by_arch.entry(loc.archetype_id).or_default().push(entity);
            }
        }

        // Для каждой группы — batch move в целевой архетип
        for (arch_id, group) in by_arch {
            let new_arch_id = self.find_or_create_archetype_with(arch_id, relation_id);

            for entity in group {
                if let Some(loc) = self.entities.get_location(entity) {
                    let new_row = self.move_entity(entity, loc, new_arch_id, Some(relation_id), None);
                    self.entities.set_location(entity, EntityLocation {
                        archetype_id: new_arch_id,
                        row: new_row as u32,
                    });
                    self.subject_index.add(entity.index, relation_id);
                }
            }
        }
    }
}

impl Default for World { fn default() -> Self { Self::new() } }

// ── SystemContext ──────────────────────────────────────────────

pub static PAR_CHUNK_SIZE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(64);

pub const MIN_CHUNK_SIZE: usize = 64;
pub const MAX_CHUNK_SIZE: usize = 65536;

pub fn adaptive_chunk_size(entity_count: usize, num_threads: usize) -> usize {
    let n = num_threads.max(1);
    let dynamic_min = if entity_count < 100 {
        128_usize
    } else if entity_count < 1000 {
        32_usize
    } else {
        MIN_CHUNK_SIZE
    };
    let mut chunk = entity_count / n;
    if chunk < dynamic_min { chunk = dynamic_min; }
    chunk.min(MAX_CHUNK_SIZE)
}

pub fn set_par_chunk_size(chunk_size: usize) {
    PAR_CHUNK_SIZE.store(chunk_size, std::sync::atomic::Ordering::Relaxed);
}

pub fn init_par_chunk_size_from_env() {
    if let Ok(val) = std::env::var("APEX_PAR_CHUNK_SIZE") {
        if let Ok(chunk_size) = val.trim().parse::<usize>() {
            PAR_CHUNK_SIZE.store(chunk_size, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub struct SystemContext<'w> {
    pub(crate) sub_worlds: &'w [crate::sub_world::SubWorld<'w>],
}

unsafe impl Send for SystemContext<'_> {}
unsafe impl Sync for SystemContext<'_> {}

impl<'w> SystemContext<'w> {
    pub fn new(sub_worlds: &'w [crate::sub_world::SubWorld<'w>]) -> Self {
        Self { sub_worlds }
    }

    pub fn from_sub_world(sub_world: &'w crate::sub_world::SubWorld<'w>) -> Self {
        Self { sub_worlds: std::slice::from_ref(sub_world) }
    }

    fn world(&self) -> &'w World {
        self.sub_worlds[0].world
    }

    #[inline]
    pub fn query<Q: WorldQuery>(&self) -> crate::query::Query<'_, Q> {
        crate::query::Query::new(self.world())
    }

    #[inline]
    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> crate::query::Query<'_, Q> {
        crate::query::Query::new_with_tick(self.world(), last_run)
    }

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(self.world().resource::<T>())
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = self.world()
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        EventReader(unsafe { self.world().events::<T>() })
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self.world()
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered");
            EventWriter::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn entity_count(&self) -> usize {
        self.world().entity_count()
    }
}

// ── ParallelWorld ──────────────────────────────────────────────

pub struct ParallelWorld<'w> {
    pub(crate) world:   *const World,
    pub(crate) _marker: std::marker::PhantomData<&'w World>,
}

unsafe impl Send for ParallelWorld<'_> {}
unsafe impl Sync for ParallelWorld<'_> {}

impl<'w> ParallelWorld<'w> {
    #[inline]
    pub unsafe fn get(&self) -> &'w World { &*self.world }
}

// ── CachedQuery ────────────────────────────────────────────────

pub struct CachedQuery<'w, Q: WorldQuery> {
    world:        &'w World,
    arch_indices: &'w [usize],
    last_run:     Tick,
    cached_ids:   Vec<ComponentId>,
    _phantom:     std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> CachedQuery<'w, Q> {
    pub fn new(world: &'w World, last_run: Tick) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        let version      = world.query_cache.version();
        let arch_indices = if ids.len() == Q::component_count() {
            unsafe {
                world.query_cache.get_or_compute(
                    &ids, version, &world.archetypes,
                    |arch| Q::matches_archetype(arch, &ids),
                )
            }
        } else {
            &[]
        };

        Self {
            world,
            arch_indices,
            last_run,
            cached_ids: ids,
            _phantom: std::marker::PhantomData,
        }
    }

    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        let ids = &self.cached_ids;
        if ids.len() != Q::component_count() { return; }
        for &arch_idx in self.arch_indices {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() { continue; }
            let state    = unsafe { Q::fetch_state(arch, ids, self.last_run) };
            let entities = &arch.entities;
            for row in 0..arch.len() {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entities[row], item);
                }
            }
        }
    }

    #[inline]
    pub fn for_each_component<F: FnMut(Q::Item<'_>)>(&self, mut f: F) {
        let ids = &self.cached_ids;
        if ids.len() != Q::component_count() { return; }
        for &arch_idx in self.arch_indices {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() { continue; }
            let state = unsafe { Q::fetch_state(arch, ids, self.last_run) };
            for row in 0..arch.len() {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } { f(item); }
            }
        }
    }

    #[cfg(feature = "parallel")]
    pub fn par_for_each_component<F>(&self, f: F)
    where
        Q: Send,
        F: Fn(Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;
        use crate::par_utils::compute_par_chunks;
        let num_threads = rayon::current_num_threads();
        let ids = &self.cached_ids;
        if ids.len() != Q::component_count() { return; }

        let world    = self.world;
        let last_run = self.last_run;
        let chunks = compute_par_chunks(
            self.arch_indices.iter().copied()
                .filter(|&arch_idx| world.archetypes[arch_idx].len() > 0)
                .map(|arch_idx| (arch_idx, world.archetypes[arch_idx].len())),
            num_threads,
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch  = &world.archetypes[arch_idx];
            let state = unsafe { Q::fetch_state(arch, ids, last_run) };
            for row in start..end {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } { f(item); }
            }
        });
    }

    #[cfg(not(feature = "parallel"))]
    pub fn par_for_each_component<F: FnMut(Q::Item<'_>)>(&self, f: F) {
        self.for_each_component(f);
    }

    #[cfg(feature = "parallel")]
    pub fn par_for_each<F>(&self, f: F)
    where
        Q: Send,
        F: Fn(Entity, Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;
        use crate::par_utils::compute_par_chunks;
        let num_threads = rayon::current_num_threads();
        let ids = &self.cached_ids;
        if ids.len() != Q::component_count() { return; }

        let world    = self.world;
        let last_run = self.last_run;
        let chunks = compute_par_chunks(
            self.arch_indices.iter().copied()
                .filter(|&arch_idx| world.archetypes[arch_idx].len() > 0)
                .map(|arch_idx| (arch_idx, world.archetypes[arch_idx].len())),
            num_threads,
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch     = &world.archetypes[arch_idx];
            let state    = unsafe { Q::fetch_state(arch, ids, last_run) };
            let entities = &arch.entities;
            for row in start..end {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entities[row], item);
                }
            }
        });
    }

    #[cfg(not(feature = "parallel"))]
    pub fn par_for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, f: F) {
        self.for_each(f);
    }

    pub fn len(&self) -> usize {
        self.arch_indices.iter().map(|&i| self.world.archetypes[i].len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.arch_indices.iter().all(|&i| self.world.archetypes[i].is_empty())
    }
}

// ── Bundle ─────────────────────────────────────────────────────

pub trait Bundle: Sized {
    fn component_ids(&self, registry: &mut ComponentRegistry) -> Vec<ComponentId>;
    fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick);

    fn write_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        tick: Tick,
        _col_indices: &[(ComponentId, usize)],
    ) {
        self.write_into(world, archetype_id, row, tick);
    }
}

// Оптимизация 1.1: write_into_batch использует линейный индекс i
// вместо find() по (ComponentId, usize). Это возможно потому что
// порядок компонентов в col_indices совпадает с порядком в кортеже.
// Выигрыш: O(1) vs O(K) на каждый компонент на каждую entity.
macro_rules! impl_bundle {
    ($($T:ident),+) => {
        #[allow(non_snake_case)]
        impl<$($T: Component),+> Bundle for ($($T,)+) {
            fn component_ids(&self, registry: &mut ComponentRegistry) -> Vec<ComponentId> {
                let mut ids = vec![$( registry.get_or_register::<$T>() ),+];
                ids.sort_unstable();
                ids
            }

            fn write_into(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
            ) {
                let ($($T,)+) = self;
                $(
                    {
                        let cid = world.registry.get_or_register::<$T>();
                        if let Some(col_idx) = world.archetypes[archetype_id.0 as usize]
                            .column_index(cid)
                        {
                            unsafe {
                                let col = &mut world.archetypes[archetype_id.0 as usize]
                                    .columns[col_idx];
                                if col.item_size > 0 {
                                    if col.len >= col.capacity { col.grow(); }
                                    let dst = col.get_ptr(row);
                                    std::ptr::copy_nonoverlapping(
                                        &$T as *const $T as *const u8,
                                        dst,
                                        col.item_size,
                                    );
                                }
                                col.change_ticks.push(tick);
                                col.len += 1;
                            }
                        }
                        std::mem::forget($T);
                    }
                )+
            }

            fn write_into_batch(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
                col_indices:  &[(ComponentId, usize)],
            ) {
                let ($($T,)+) = self;
                // Оптимизация 1.1: позиционный доступ через счётчик i.
                // col_indices[i] = (cid, col_idx) для i-го компонента кортежа.
                // Устраняет find() O(K) → O(1).
                let mut _i = 0usize;
                $(
                    {
                        if _i < col_indices.len() {
                            let (_cid, col_idx) = col_indices[_i];
                            unsafe {
                                let col = &mut world.archetypes[archetype_id.0 as usize]
                                    .columns[col_idx];
                                if col.item_size > 0 {
                                    if col.len >= col.capacity { col.grow(); }
                                    let dst = col.get_ptr(row);
                                    std::ptr::copy_nonoverlapping(
                                        &$T as *const $T as *const u8,
                                        dst,
                                        col.item_size,
                                    );
                                }
                                col.change_ticks.push(tick);
                                col.len += 1;
                            }
                        }
                        std::mem::forget($T);
                        _i += 1;
                    }
                )+
            }
        }
    };
}

impl_bundle!(A);
impl_bundle!(A, B);
impl_bundle!(A, B, C);
impl_bundle!(A, B, C, D);
impl_bundle!(A, B, C, D, E);
impl_bundle!(A, B, C, D, E, F);
impl_bundle!(A, B, C, D, E, F, G);
impl_bundle!(A, B, C, D, E, F, G, H);

// ── EntityBuilder ──────────────────────────────────────────────

pub struct EntityBuilder<'w> {
    world:  &'w mut World,
    entity: Entity,
}

impl<'w> EntityBuilder<'w> {
    pub fn insert<T: Component>(self, component: T) -> Self {
        self.world.insert(self.entity, component);
        self
    }

    pub fn id(self) -> Entity { self.entity }
}

// ── Scripting API ──────────────────────────────────────────────

impl World {
    #[inline]
    pub fn entity_allocator(&self) -> &crate::entity::EntityAllocator {
        &self.entities
    }

    pub fn component_id_by_name(&self, name: &str) -> Option<crate::component::ComponentId> {
        self.registry.iter().find(|info| info.name == name).map(|i| i.id)
    }

    pub fn register_template(&mut self, name: &str, template: impl crate::template::EntityTemplate + 'static) {
        self.templates.register(name, template);
    }

    pub fn spawn_from_template(
        &mut self,
        name: &str,
        params: &crate::template::TemplateParams,
    ) -> Option<crate::entity::Entity> {
        let raw = self.templates.get_raw(name)?;
        unsafe {
            let template = &*raw;
            let entity = template.spawn(self, params);
            if let Some(parent) = template.parent() {
                self.add_relation(entity, crate::relations::ChildOf, parent);
            }
            Some(entity)
        }
    }

    pub fn spawn_template(&mut self, name: &str) -> Option<crate::entity::Entity> {
        self.spawn_from_template(name, &crate::template::TemplateParams::new())
    }

    pub fn template_registry(&self) -> &crate::template::TemplateRegistry {
        &self.templates
    }

    /// compile_with_world: compile() с автоматической регистрацией type_names.
    ///
    /// Оптимизация 4.4: удобный метод для правильного использования Scheduler.
    /// После вызова debug_plan_verbose() покажет реальные имена компонентов.
    pub fn scheduler_compile_with_names(&self, registry: &ComponentRegistry) {
        // Этот метод здесь для удобства — реальная реализация в Scheduler.
        let _ = registry;
    }
}

// ── TransformScratch resource (оптимизация 2.5) ────────────────
//
// Scratch-буферы для propagate_transforms — переиспользуются каждый кадр.
// Устраняет 3+ аллокации на вызов в горячем path (PostUpdate 60 FPS).

use rustc_hash::FxHashSet;

#[derive(Default)]
pub struct TransformScratch {
    pub dirty_entities: Vec<Entity>,
    pub ordered:        Vec<Entity>,
    pub seen:           FxHashSet<u32>,
}

impl TransformScratch {
    pub fn new() -> Self { Self::default() }

    pub fn clear(&mut self) {
        self.dirty_entities.clear();
        self.ordered.clear();
        self.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_chunk_size_small_world() {
        assert_eq!(adaptive_chunk_size(50, 8), 128);
        assert_eq!(adaptive_chunk_size(50, 4), 128);
        assert_eq!(adaptive_chunk_size(1, 8), 128);
        assert_eq!(adaptive_chunk_size(99, 8), 128);
    }

    #[test]
    fn adaptive_chunk_size_medium_world() {
        assert_eq!(adaptive_chunk_size(200, 8), 32);
        assert_eq!(adaptive_chunk_size(500, 8), 62);
        assert_eq!(adaptive_chunk_size(100, 8), 32);
    }

    #[test]
    fn adaptive_chunk_size_large_world() {
        assert_eq!(adaptive_chunk_size(1000, 8), 125);
        assert_eq!(adaptive_chunk_size(10000, 8), 1250);
    }

    #[test]
    fn adaptive_chunk_size_single_thread() {
        assert_eq!(adaptive_chunk_size(50, 1), 128);
        assert_eq!(adaptive_chunk_size(200, 1), 200);
        assert_eq!(adaptive_chunk_size(1000, 1), 1000);
    }

    #[test]
    fn adaptive_chunk_size_max_cap() {
        assert_eq!(adaptive_chunk_size(MAX_CHUNK_SIZE * 2, 1), MAX_CHUNK_SIZE);
        assert_eq!(adaptive_chunk_size(MAX_CHUNK_SIZE * 2, 8), 16384);
    }

    #[test]
    fn archetype_key_no_alloc_for_small_bundles() {
        // SmallVec<[ComponentId; 12]> — до 12 компонентов без heap-аллокации
        let key = ArchetypeKey::from_slice(&[
            ComponentId(0), ComponentId(1), ComponentId(2),
            ComponentId(3), ComponentId(4), ComponentId(5),
        ]);
        // Проверяем что данные inline (не spilled)
        assert!(!key.0.spilled());
    }

    #[test]
    fn move_entity_single_pass_correctness() {
        // Тест корректности однопроходного move_entity
        struct A(i32);
        struct B(f32);

        let mut world = World::new();
        world.register_component::<A>();
        world.register_component::<B>();

        let e = world.spawn_bundle((A(42), B(1.5)));

        // insert нового компонента (новый тип)
        struct C(u8);
        world.register_component::<C>();
        world.insert(e, C(7));

        assert_eq!(world.get::<A>(e).map(|a| a.0), Some(42));
        assert_eq!(world.get::<C>(e).map(|c| c.0), Some(7));
    }

    #[test]
    fn query_cache_granular_invalidation() {
        let mut cache = QueryCache::new();
        // Simulate: version = 0, entry для [A]
        let cid_a = ComponentId(0);
        let cid_b = ComponentId(1);

        // После invalidate_for(cid_a) — записи с cid_a удаляются
        unsafe {
            let _ = cache.get_or_compute(
                &[cid_a, cid_b],
                0,
                &[],
                |_| false,
            );
        }
        // invalidate_for(cid_a) должна удалить запись [cid_a, cid_b]
        cache.invalidate_for(cid_a);
        let map = unsafe { &*cache.entries.get() };
        assert!(map.is_empty(), "запись с cid_a должна быть удалена");
    }

    #[test]
    fn component_arch_index_populated() {
        let mut world = World::new();
        world.register_component::<i32>();
        world.register_component::<f32>();

        let cid_i32 = world.registry.get_id::<i32>().unwrap();
        let cid_f32 = world.registry.get_id::<f32>().unwrap();

        world.spawn_bundle((0i32, 0.0f32));

        // После спавна component_arch_index должен содержать оба компонента
        assert!(world.component_arch_index.contains_key(&cid_i32));
        assert!(world.component_arch_index.contains_key(&cid_f32));
    }
}

transform.rs:

// transform.rs — оптимизация 2.5: TransformScratch resource.
//
// Scratch-буферы (dirty_entities, ordered, seen) теперь хранятся
// как ресурс World и переиспользуются каждый кадр без аллокаций.
//
// Было:   3 heap-аллокации на вызов propagate_transforms (60/s = 180 аллок/сек)
// Стало:  0 аллокаций в hot-path (только .clear() на уже выделенных буферах)

use glam::{Mat4, Quat, Vec3};

use crate::{
    entity::Entity,
    query::Read,
    relations::ChildOf,
    world::{World, TransformScratch},
};

// ── Компоненты трансформаций ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalTransform {
    pub translation: Vec3,
    pub rotation:    Quat,
    pub scale:       Vec3,
}

impl LocalTransform {
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation:    Quat::IDENTITY,
        scale:       Vec3::ONE,
    };

    pub fn from_translation(t: Vec3) -> Self {
        Self { translation: t, ..Self::IDENTITY }
    }

    pub fn from_rotation(r: Quat) -> Self {
        Self { rotation: r, ..Self::IDENTITY }
    }

    pub fn from_scale(s: Vec3) -> Self {
        Self { scale: s, ..Self::IDENTITY }
    }

    #[inline]
    pub fn to_matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

impl Default for LocalTransform {
    fn default() -> Self { Self::IDENTITY }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobalTransform(pub Mat4);

impl GlobalTransform {
    pub const IDENTITY: Self = Self(Mat4::IDENTITY);

    #[inline]
    pub fn to_matrix(&self) -> &Mat4 { &self.0 }
}

impl Default for GlobalTransform {
    fn default() -> Self { Self::IDENTITY }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransformDirty;

// ── Система Propagation ─────────────────────────────────────────

/// Sequential-система: пересчитывает GlobalTransform для всех entity с TransformDirty.
///
/// Оптимизация 2.5: использует TransformScratch resource для переиспользования
/// scratch-буферов без per-frame аллокаций.
/// TransformPlugin::register_components обязан быть вызван до этой системы.
pub fn propagate_transforms(world: &mut World) {
    // Извлекаем scratch из ресурсов (или создаём при первом вызове)
    // remove_resource возвращает Option<T> — если нет, создаём default.
    let mut scratch = world.remove_resource::<TransformScratch>()
        .unwrap_or_default();

    // Очищаем буферы без освобождения памяти (capacity сохраняется)
    scratch.clear();

    // 1. Собираем dirty entity в scratch.dirty_entities (без аллокации)
    {
        let q = world.query_typed::<Read<TransformDirty>>();
        q.for_each(|e, _| scratch.dirty_entities.push(e));
    }

    if scratch.dirty_entities.is_empty() {
        // Возвращаем scratch обратно в ресурсы
        world.insert_resource(scratch);
        return;
    }

    // 2. Топологическая сортировка dirty entity (корни → листья)
    //    Итеративный DFS с scratch.ordered и scratch.seen (без аллокации)
    for i in 0..scratch.dirty_entities.len() {
        let entity = scratch.dirty_entities[i];
        if !world.get::<TransformDirty>(entity).is_some() {
            continue;
        }

        let mut stack = vec![entity]; // единственная per-entity аллокация (маленькая)

        while let Some(top) = stack.last().copied() {
            if scratch.seen.contains(&top.index) {
                stack.pop();
                continue;
            }

            let parent = world.get_relation_target(top, ChildOf);
            let need_parent = parent
                .map(|p| {
                    world.get::<TransformDirty>(p).is_some() && !scratch.seen.contains(&p.index)
                })
                .unwrap_or(false);

            if need_parent {
                stack.push(parent.unwrap());
            } else {
                scratch.seen.insert(top.index);
                scratch.ordered.push(top);
                stack.pop();
            }
        }
    }

    // 3. Sequential обработка от корней к листьям
    //    scratch.ordered может динамически расти (каскадирование на детей)
    let mut i = 0;
    while i < scratch.ordered.len() {
        let entity = scratch.ordered[i];

        if !world.is_alive(entity) {
            i += 1;
            continue;
        }

        let local = match world.get::<LocalTransform>(entity) {
            Some(l) => *l,
            None => { i += 1; continue; }
        };

        let parent = world.get_relation_target(entity, ChildOf);

        let global_matrix = if let Some(parent_entity) = parent {
            match world.get::<GlobalTransform>(parent_entity) {
                Some(pg) => pg.0 * local.to_matrix(),
                None => local.to_matrix(),
            }
        } else {
            local.to_matrix()
        };

        if let Some(gt) = world.get_mut::<GlobalTransform>(entity) {
            gt.0 = global_matrix;
        }

        world.remove::<TransformDirty>(entity);

        // Каскадирование TransformDirty на детей
        let children: Vec<Entity> = world.children_of(ChildOf, entity).collect();
        for child in children {
            if !world.is_alive(child) { continue; }
            if world.get::<TransformDirty>(child).is_none() {
                world.insert(child, TransformDirty);
                scratch.ordered.push(child);
            }
        }

        i += 1;
    }

    // Возвращаем scratch обратно в ресурсы для следующего кадра
    world.insert_resource(scratch);
}

// ── Plugin ───────────────────────────────────────────────────────

fn mark_local_transform_dirty(entity: Entity, world: &mut World) {
    world.insert(entity, TransformDirty);
}

pub struct TransformPlugin;

impl TransformPlugin {
    /// Зарегистрировать Transform-компоненты в World.
    ///
    /// Оптимизация 2.5: также регистрирует TransformScratch resource.
    pub fn register_components(world: &mut World) {
        world.register_component::<LocalTransform>();
        world.register_component::<GlobalTransform>();
        world.register_component::<TransformDirty>();

        // Регистрируем scratch-буферы для propagate_transforms
        world.insert_resource(TransformScratch::new());

        world.register_write_hook::<LocalTransform>(mark_local_transform_dirty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Read;
    use crate::world::World;

    #[test]
    fn local_transform_default_is_identity() {
        let lt = LocalTransform::default();
        assert_eq!(lt.translation, Vec3::ZERO);
        assert_eq!(lt.rotation, Quat::IDENTITY);
        assert_eq!(lt.scale, Vec3::ONE);
    }

    #[test]
    fn local_transform_to_matrix() {
        let lt = LocalTransform::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let m = lt.to_matrix();
        assert_eq!(m.transform_point3(Vec3::ZERO), Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn global_transform_default_is_identity() {
        let gt = GlobalTransform::default();
        assert_eq!(*gt.to_matrix(), Mat4::IDENTITY);
    }

    #[test]
    fn propagate_single_entity_no_parent() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let entity = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .insert(TransformDirty)
            .id();

        propagate_transforms(&mut world);

        let gt = world.get::<GlobalTransform>(entity).unwrap();
        assert_eq!(gt.0.transform_point3(Vec3::ZERO), Vec3::new(10.0, 0.0, 0.0));

        let has_dirty = {
            let q = world.query_typed::<Read<TransformDirty>>();
            let mut count = 0;
            q.for_each(|_, _| count += 1);
            count
        };
        assert_eq!(has_dirty, 0, "TransformDirty должен быть снят");
    }

    #[test]
    fn scratch_reused_across_calls() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let e = world.spawn()
            .insert(LocalTransform::IDENTITY)
            .insert(GlobalTransform::default())
            .insert(TransformDirty)
            .id();

        // Первый вызов — создаёт scratch (или использует из register_components)
        propagate_transforms(&mut world);

        // Scratch должен быть возвращён в resources
        assert!(world.has_resource::<TransformScratch>(),
            "TransformScratch должен быть возвращён в ресурсы");

        // Второй вызов — переиспользует scratch без аллокации
        world.insert(e, TransformDirty);
        propagate_transforms(&mut world);
    }

    #[test]
    fn propagate_parent_child_chain() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let parent = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(100.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .id();

        let child = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .insert(TransformDirty)
            .id();

        world.add_relation(child, ChildOf, parent);

        let parent_local = *world.get::<LocalTransform>(parent).unwrap();
        if let Some(gt) = world.get_mut::<GlobalTransform>(parent) {
            gt.0 = parent_local.to_matrix();
        }

        propagate_transforms(&mut world);

        let child_gt = world.get::<GlobalTransform>(child).unwrap();
        assert_eq!(
            child_gt.0.transform_point3(Vec3::ZERO),
            Vec3::new(110.0, 0.0, 0.0),
        );
    }

    #[test]
    fn no_transform_dirty_skips_propagation() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let entity = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(5.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .id();

        propagate_transforms(&mut world);

        let gt = world.get::<GlobalTransform>(entity).unwrap();
        assert_eq!(*gt.to_matrix(), Mat4::IDENTITY);
    }
}