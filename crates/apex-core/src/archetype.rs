use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::alloc::{alloc, dealloc, realloc, Layout};

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
    pub fn id(&self) -> ComponentId {
        self.col.component_id
    }
    pub unsafe fn get_raw_ptr(&self, row: usize) -> *const u8 {
        self.col.get_ptr(row)
    }
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
    pub fn id(&self) -> ComponentId {
        self.component_id
    }

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

    /// Записать типизированное значение в колонку по заданной строке и инкрементировать `len`.
    ///
    /// Используется derive-макросом [`#[derive(Bundle)]`](apex_macros::Bundle) для записи
    /// компонентов в архетип.
    ///
    /// # Safety
    /// Вызывающий гарантирует что `T` соответствует `self.component_id`,
    /// а `row == self.len` (запись всегда в конец колонки).
    #[inline]
    pub unsafe fn write_typed_at<T>(&mut self, value: T, row: usize, tick: Tick) {
        debug_assert!(
            row == self.len,
            "write_typed_at: row {} != len {}",
            row,
            self.len
        );
        self.push(&value as *const T as *const u8, tick);
        std::mem::forget(value);
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
        if self.item_size == 0 {
            self.capacity = new_cap;
            return;
        }
        if self.capacity == 0 {
            // Первое выделение — через alloc (realloc с null ptr — UB)
            let new_layout = self.layout_for(new_cap);
            self.data = unsafe {
                let ptr = alloc(new_layout);
                assert!(!ptr.is_null(), "allocation failed");
                ptr
            };
        } else {
            // Перевыделение — realloc: один syscall вместо alloc+copy+dealloc
            let old_layout = self.layout_for(self.capacity);
            let new_size = self.item_size * new_cap;
            self.data = unsafe {
                let ptr = realloc(self.data, old_layout, new_size);
                assert!(!ptr.is_null(), "reallocation failed");
                ptr
            };
        }
        self.capacity = new_cap;
    }

    /// Предварительное выделение памяти под `additional` элементов.
    /// Позволяет избежать множественных grow() при массовых spawn'ах.
    pub(crate) fn reserve(&mut self, additional: usize) {
        let needed = self.len + additional;
        if needed <= self.capacity {
            self.change_ticks.reserve(additional);
            return;
        }
        let new_cap = needed.next_power_of_two().max(4);
        if self.item_size == 0 {
            self.capacity = new_cap;
            self.change_ticks.reserve(additional);
            return;
        }
        if self.capacity == 0 {
            // Первое выделение — через alloc
            let new_layout = self.layout_for(new_cap);
            self.data = unsafe {
                let ptr = alloc(new_layout);
                assert!(!ptr.is_null(), "allocation failed");
                ptr
            };
        } else {
            // Перевыделение — realloc: один syscall вместо alloc+copy+dealloc
            let old_layout = self.layout_for(self.capacity);
            let new_size = self.item_size * new_cap;
            self.data = unsafe {
                let ptr = realloc(self.data, old_layout, new_size);
                assert!(!ptr.is_null(), "reallocation failed");
                ptr
            };
        }
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
            unsafe {
                (self.drop_fn)(self.get_ptr(i));
            }
        }
        if self.capacity > 0 && !self.data.is_null() && self.item_size > 0 {
            unsafe {
                dealloc(self.data, self.layout_for(self.capacity));
            }
        }
    }
}

pub struct Archetype {
    pub id: ArchetypeId,
    pub component_ids: SmallVec<[ComponentId; 8]>,
    pub columns: Vec<Column>,
    pub entities: Vec<Entity>,
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

    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

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

    pub unsafe fn get_component_mut<T>(
        &mut self,
        row: usize,
        component_id: ComponentId,
    ) -> Option<&mut T> {
        let col_idx = self.column_index(component_id)?;
        Some(self.columns[col_idx].get_mut::<T>(row))
    }

    /// Обновить change tick для компонента в указанной строке.
    ///
    /// Использует сырые указатели для interior mutation через `&self`,
    /// что позволяет обновлять tick без `&mut self`.
    ///
    /// # Safety
    /// - `row` должен быть < len колонки
    /// - Никакая другая mutable ссылка на колонку не должна существовать
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

    pub unsafe fn write_component(
        &mut self,
        row: usize,
        component_id: ComponentId,
        src: *const u8,
        tick: Tick,
    ) {
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
    ///
    /// # Safety
    /// Вызывающий должен гарантировать что:
    /// - Нет concurrent structural changes во время итерации
    /// - Индексы row не выходят за пределы col.len
    ///
    /// Используется только `RhaiQueryIter` в однопоточном контексте.
    #[inline]
    pub fn columns_raw(&self) -> &[Column] {
        &self.columns
    }

    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }
}

/// Описание одного чанка архетипа для chunk-level параллелизма.
///
/// Содержит сырые указатели на срезы данных [start, start+len).
/// SAFETY: используется только внутри Rayon scope пока архетип жив
/// и не происходит structural changes.
pub struct ArchetypeChunk<'a> {
    pub entities: &'a [Entity],
    pub arch_id: ArchetypeId,
    /// Индекс строки start внутри архетипа (для column_index lookup)
    pub start_row: usize,
    pub len: usize,
}

/// Разбить архетип на чанки фиксированного размера.
///
/// Возвращает срезы `entities` длиной `chunk_size` (последний может быть меньше).
/// Используется `par_for_each` для параллельной итерации внутри одного архетипа.
pub fn archetype_chunks(
    arch: &Archetype,
    chunk_size: usize,
) -> impl Iterator<Item = ArchetypeChunk<'_>> {
    let total = arch.entities.len();
    let num_chunks = (total + chunk_size - 1) / chunk_size;
    (0..num_chunks).map(move |i| {
        let start = i * chunk_size;
        let end = (start + chunk_size).min(total);
        ArchetypeChunk {
            entities: &arch.entities[start..end],
            arch_id: arch.id,
            start_row: start,
            len: end - start,
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{ComponentId, ComponentInfo, Tick};
    use smallvec::SmallVec;

    unsafe fn noop_drop(_ptr: *mut u8) {}

    fn make_info(id: u32) -> ComponentInfo {
        ComponentInfo {
            id: ComponentId(id),
            name: "test",
            type_id: std::any::TypeId::of::<f32>(),
            size: std::mem::size_of::<f32>(),
            align: std::mem::align_of::<f32>(),
            drop_fn: noop_drop,
            serde: None,
        }
    }

    // ── Column tests ──────────────────────────────────────────

    #[test]
    fn column_push_and_get() {
        let info = make_info(0);
        let mut col = Column::new(&info);
        assert_eq!(col.len, 0);
        assert_eq!(col.capacity, 0);

        let val: f32 = 42.0;
        unsafe {
            col.push(&val as *const f32 as *const u8, Tick(1));
        }
        assert_eq!(col.len, 1);
        assert!(col.capacity >= 1);
        assert_eq!(col.change_ticks.len(), 1);

        unsafe {
            let stored: &f32 = col.get::<f32>(0);
            assert_eq!(*stored, 42.0);
        }
    }

    #[test]
    fn column_push_many_grows_capacity() {
        let info = make_info(0);
        let mut col = Column::new(&info);
        let n = 200;

        for i in 0..n {
            let val: f32 = i as f32;
            unsafe {
                col.push(&val as *const f32 as *const u8, Tick(i as u32));
            }
        }
        assert_eq!(col.len, n);
        assert!(col.capacity >= n);
        assert_eq!(col.change_ticks.len(), n);

        for i in 0..n {
            unsafe {
                let stored: &f32 = col.get::<f32>(i as usize);
                assert_eq!(*stored, i as f32);
            }
            assert_eq!(col.get_tick(i as usize).0, i as u32);
        }
    }

    #[test]
    fn column_write_at_existing_row() {
        let info = make_info(0);
        let mut col = Column::new(&info);

        unsafe {
            col.push(&(1.0_f32) as *const f32 as *const u8, Tick(1));
            col.push(&(2.0_f32) as *const f32 as *const u8, Tick(1));
        }

        unsafe {
            col.write_at(0, &(99.0_f32) as *const f32 as *const u8, Tick(100));
        }

        unsafe {
            assert_eq!(*col.get::<f32>(0), 99.0);
            assert_eq!(*col.get::<f32>(1), 2.0);
        }
        assert_eq!(col.get_tick(0).0, 100);
    }

    #[test]
    fn column_swap_remove_no_drop() {
        let info = make_info(0);
        let mut col = Column::new(&info);

        for i in 0..5 {
            let val: f32 = i as f32;
            unsafe {
                col.push(&val as *const f32 as *const u8, Tick(i as u32));
            }
        }

        unsafe {
            col.swap_remove_no_drop(1);
        }
        assert_eq!(col.len, 4);
        // Последний элемент (4.0) перемещён на место удалённого (1)
        unsafe {
            assert_eq!(*col.get::<f32>(0), 0.0);
            assert_eq!(*col.get::<f32>(1), 4.0);
            assert_eq!(*col.get::<f32>(2), 2.0);
            assert_eq!(*col.get::<f32>(3), 3.0);
        }
    }

    #[test]
    fn column_swap_remove_last_noop() {
        let info = make_info(0);
        let mut col = Column::new(&info);

        unsafe {
            col.push(&(5.0_f32) as *const f32 as *const u8, Tick(1));
        }
        assert_eq!(col.len, 1);

        unsafe {
            col.swap_remove_no_drop(0);
        }
        assert_eq!(col.len, 0);
    }

    #[test]
    fn column_reserve_pre_allocation() {
        let info = make_info(0);
        let mut col = Column::new(&info);
        col.reserve(100);
        assert!(col.capacity >= 100);
        assert_eq!(col.len, 0);
    }

    #[test]
    fn column_change_tick_tracking() {
        let info = make_info(0);
        let mut col = Column::new(&info);

        unsafe {
            col.push(&(1.0_f32) as *const f32 as *const u8, Tick(10));
            col.push(&(2.0_f32) as *const f32 as *const u8, Tick(20));
        }

        assert_eq!(col.get_tick(0).0, 10);
        assert_eq!(col.get_tick(1).0, 20);

        unsafe {
            col.write_at(0, &(3.0_f32) as *const f32 as *const u8, Tick(30));
        }
        assert_eq!(col.get_tick(0).0, 30);
    }

    #[test]
    fn column_zero_sized_type() {
        let info = ComponentInfo {
            id: ComponentId(99),
            name: "zst",
            type_id: std::any::TypeId::of::<()>(),
            size: 0,
            align: 1,
            drop_fn: noop_drop,
            serde: None,
        };
        let mut col = Column::new(&info);
        unsafe {
            col.push(
                std::ptr::NonNull::<()>::dangling().as_ptr() as *const u8,
                Tick(1),
            );
        }
        assert_eq!(col.len, 1);
    }

    // ── Archetype tests ───────────────────────────────────────

    fn make_arch(id: u32, component_ids: &[u32]) -> Archetype {
        let ids: SmallVec<[ComponentId; 8]> =
            component_ids.iter().map(|&i| ComponentId(i)).collect();
        let infos: Vec<ComponentInfo> = component_ids.iter().map(|&i| make_info(i)).collect();
        let info_refs: Vec<&ComponentInfo> = infos.iter().collect();
        Archetype::new(ArchetypeId(id), ids, &info_refs)
    }

    fn make_entity(index: u32, generation: u32) -> Entity {
        Entity { index, generation }
    }

    #[test]
    fn archetype_allocate_row() {
        let mut arch = make_arch(0, &[0, 1]);
        let entity = make_entity(0, 100);
        let row = unsafe { arch.allocate_row(entity) };
        assert_eq!(row, 0);
        assert_eq!(arch.len(), 1);
    }

    #[test]
    fn archetype_write_and_read_component() {
        let mut arch = make_arch(0, &[0]);
        let entity = make_entity(0, 1);
        unsafe {
            arch.allocate_row(entity);
        }

        let val: f32 = 3.14;
        unsafe {
            arch.write_component(0, ComponentId(0), &val as *const f32 as *const u8, Tick(1));
        }

        unsafe {
            let stored: &f32 = arch.get_component::<f32>(0, ComponentId(0)).unwrap();
            assert_eq!(*stored, 3.14);
        }
    }

    #[test]
    fn archetype_remove_row() {
        let mut arch = make_arch(0, &[0]);
        let e1 = make_entity(0, 1);
        let e2 = make_entity(0, 2);

        unsafe {
            arch.allocate_row(e1);
            arch.write_component(
                0,
                ComponentId(0),
                &(1.0_f32) as *const f32 as *const u8,
                Tick(1),
            );
            arch.allocate_row(e2);
            arch.write_component(
                1,
                ComponentId(0),
                &(2.0_f32) as *const f32 as *const u8,
                Tick(1),
            );
        }
        assert_eq!(arch.len(), 2);

        let swapped = unsafe { arch.remove_row(0) };
        assert_eq!(arch.len(), 1);
        assert_eq!(swapped, Some(e2)); // последний переехал на место 0
    }

    #[test]
    fn archetype_has_component() {
        let arch = make_arch(0, &[0, 1]);
        assert!(arch.has_component(ComponentId(0)));
        assert!(arch.has_component(ComponentId(1)));
        assert!(!arch.has_component(ComponentId(2)));
    }

    #[test]
    fn archetype_column_index() {
        let arch = make_arch(0, &[0, 1, 2]);
        assert_eq!(arch.column_index(ComponentId(0)), Some(0));
        assert_eq!(arch.column_index(ComponentId(1)), Some(1));
        assert_eq!(arch.column_index(ComponentId(2)), Some(2));
        assert_eq!(arch.column_index(ComponentId(3)), None);
    }

    #[test]
    fn archetype_edges() {
        let mut arch = make_arch(0, &[0]);
        arch.add_edges.insert(ComponentId(1), ArchetypeId(3));
        arch.remove_edges.insert(ComponentId(0), ArchetypeId(7));

        assert_eq!(arch.add_edges.get(&ComponentId(1)), Some(&ArchetypeId(3)));
        assert_eq!(
            arch.remove_edges.get(&ComponentId(0)),
            Some(&ArchetypeId(7))
        );
    }

    // ── ArchetypeChunk tests ──────────────────────────────────

    #[test]
    fn chunk_exact_size() {
        let mut arch = make_arch(0, &[0]);
        for i in 0..10 {
            unsafe {
                arch.allocate_row(make_entity(0, i));
            }
        }
        let chunks: Vec<_> = archetype_chunks(&arch, 5).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].entities.len(), 5);
        assert_eq!(chunks[0].start_row, 0);
        assert_eq!(chunks[1].entities.len(), 5);
        assert_eq!(chunks[1].start_row, 5);
    }

    #[test]
    fn chunk_uneven() {
        let mut arch = make_arch(0, &[0]);
        for i in 0..7 {
            unsafe {
                arch.allocate_row(make_entity(0, i));
            }
        }
        let chunks: Vec<_> = archetype_chunks(&arch, 3).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].entities.len(), 3);
        assert_eq!(chunks[1].entities.len(), 3);
        assert_eq!(chunks[2].entities.len(), 1);
    }

    #[test]
    fn column_view_access() {
        let arch = make_arch(0, &[0, 1]);
        let views: Vec<_> = arch.columns().collect();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].id(), ComponentId(0));
        assert_eq!(views[1].id(), ComponentId(1));
    }
}
