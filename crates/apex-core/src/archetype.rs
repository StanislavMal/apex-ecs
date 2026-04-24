use std::alloc::{alloc, dealloc, Layout};
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

    pub(crate) fn grow(&mut self) {
        let new_cap = if self.capacity == 0 { 64 } else { self.capacity * 2 };
        if self.item_size == 0 {
            self.capacity = new_cap;
            return;
        }
        let new_layout = self.layout_for(new_cap);
        let new_data = unsafe { alloc(new_layout) };
        assert!(!new_data.is_null(), "allocation failed");
        if self.len > 0 && !self.data.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(self.data, new_data, self.len * self.item_size);
            }
        }
        if self.capacity > 0 && !self.data.is_null() {
            unsafe { dealloc(self.data, self.layout_for(self.capacity)); }
        }
        self.data = new_data;
        self.capacity = new_cap;
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

    pub fn entities(&self) -> &[Entity] { &self.entities }
}

/// Описание одного чанка архетипа для chunk-level параллелизма.
///
/// Содержит сырые указатели на срезы данных [start, start+len).
/// SAFETY: используется только внутри Rayon scope пока архетип жив
/// и не происходит structural changes.
pub struct ArchetypeChunk<'a> {
    pub entities: &'a [Entity],
    pub arch_id:  ArchetypeId,
    /// Индекс строки start внутри архетипа (для column_index lookup)
    pub start_row: usize,
    pub len:       usize,
}

/// Разбить архетип на чанки фиксированного размера.
///
/// Возвращает срезы `entities` длиной `chunk_size` (последний может быть меньше).
/// Используется `par_for_each_component` для параллельной итерации внутри одного архетипа.
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