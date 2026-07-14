use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::alloc::{alloc, dealloc, realloc, Layout};
use std::cell::UnsafeCell;

use crate::{
    component::{ComponentId, ComponentInfo, Tick},
    entity::Entity,
};

/// A change/added tick stored behind `UnsafeCell`.
///
/// Change detection writes ticks through a shared `&Column`: the query `Write`
/// / dense hot path (`Mut::deref_mut`, `stamp_range`) and the
/// `set_change_tick` interior-mutation entry point all stamp the current world
/// tick without holding `&mut`. Laundering a `*mut Tick` out of a shared
/// `&[Tick]` and writing through it is undefined behavior — Tree Borrows
/// forbids the write to non-interior-mutable memory. With the cell, the
/// pointer's provenance is the cell interior, so the write is legal (A2). The
/// scheduler serializes mutable access to any given row via `AccessDescriptor`,
/// so the interior mutation never actually races.
///
/// `#[repr(transparent)]` keeps the layout byte-identical to `Tick`, so
/// `ticks_ptr()` can still hand out a single `*const Tick` / `*mut Tick` over
/// the whole buffer for the zero-cost `Changed<T>` / `Added<T>` / `Mut<T>`
/// paths.
#[repr(transparent)]
pub(crate) struct TickCell(UnsafeCell<Tick>);

// SAFETY: `Tick` is `Send + Sync` (a plain `u32`); concurrent mutable access to
// a row's tick is excluded by the scheduler's `AccessDescriptor` discipline, so
// exposing `Sync` (needed because `Column` is `Sync`) never permits a real race.
unsafe impl Sync for TickCell {}

impl TickCell {
    #[inline]
    pub(crate) fn new(tick: Tick) -> Self {
        Self(UnsafeCell::new(tick))
    }

    /// Read the tick (shared). Concurrent writers to this row are excluded by
    /// scheduler access discipline.
    #[inline]
    pub(crate) fn get(&self) -> Tick {
        // SAFETY: `Tick: Copy`; a shared read of the cell interior.
        unsafe { *self.0.get() }
    }

    /// Exclusive mutable access — used where `&mut Column` is held.
    #[inline]
    pub(crate) fn get_mut(&mut self) -> &mut Tick {
        self.0.get_mut()
    }

    /// Interior write through `&self` — the `ResMut::deref_mut` lazy stamp
    /// (RT-1). Concurrent writers are excluded by the same scheduler
    /// `AccessDescriptor` discipline that guards row ticks.
    #[inline]
    pub(crate) fn set(&self, tick: Tick) {
        // SAFETY: exclusive access to the resource (and thus its tick cell) is
        // guaranteed by the scheduler while a `ResMut` is live.
        unsafe { *self.0.get() = tick }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ArchetypeId(pub(crate) u32);

impl ArchetypeId {
    pub const EMPTY: Self = Self(0);

    /// Get the internal index (for access to `world.archetypes()`).
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
    /// Per-row tick of the last change (for change detection).
    /// `TickCell` so the change-detection hot path may write through a shared
    /// `&Column` soundly (A2).
    pub(crate) change_ticks: Vec<TickCell>,
    /// Per-row tick of the component's ADDITION to the entity (W3-1, `Added<T>`
    /// filter).
    ///
    /// Semantics: set when a component first appears on an entity (spawn/insert
    /// of a new one) and SURVIVES an archetype move (insert/remove of the
    /// entity's neighboring components does not "refresh" it). Replacing a value
    /// via `insert` over an existing component updates ONLY the change-tick (like
    /// Bevy: a re-insert does not restart `Added<T>`).
    pub(crate) added_ticks: Vec<TickCell>,
}

unsafe impl Send for Column {}
unsafe impl Sync for Column {}

/// Public view of a column for external crates.
pub struct ColumnView<'a> {
    col: &'a Column,
}

impl<'a> ColumnView<'a> {
    pub fn id(&self) -> ComponentId {
        self.col.component_id
    }
    /// # Safety
    /// `row` must be `< len` of the viewed column. The returned pointer aliases
    /// column storage and is invalidated by any structural change; the caller
    /// must interpret it as the column's component type.
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
            added_ticks: Vec::new(),
        }
    }

    /// Public accessor for the column's component_id.
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

    /// # Safety
    /// `row < self.len`, and no other reference to this row's change-tick may be
    /// live (interior mutation through `&self`).
    #[inline]
    pub unsafe fn set_change_tick(&self, row: usize, tick: Tick) {
        debug_assert!(row < self.len);
        // The elements are `TickCell` (interior-mutable), so a `*mut Tick` cast
        // from the buffer base carries cell provenance — writing is legal (A2).
        let ptr = self.change_ticks.as_ptr() as *mut Tick;
        *ptr.add(row) = tick;
    }

    /// Stamp the change-tick over a RANGE of rows `[start, end)` (W2-0.5).
    ///
    /// Used by dense iteration ([`DenseQuery`](crate::query::DenseQuery)): the
    /// contract is "a write slice = the whole range changed". The plain loop is
    /// vectorized by the compiler into a fill.
    ///
    /// # Safety
    /// `end <= self.len`; no other reference to these rows' ticks exists.
    #[inline]
    pub unsafe fn stamp_range(&self, start: usize, end: usize, tick: Tick) {
        debug_assert!(end <= self.len);
        // Cell-interior provenance (see `set_change_tick`); the plain loop still
        // lowers to a vectorized fill.
        let ptr = self.change_ticks.as_ptr() as *mut Tick;
        for row in start..end {
            *ptr.add(row) = tick;
        }
    }

    /// # Safety
    /// `row < self.len` (for ZST columns the returned pointer is a non-null
    /// dangling sentinel and must not be dereferenced). The pointer aliases
    /// column storage and is invalidated by `grow`/reallocation.
    pub unsafe fn get_ptr(&self, row: usize) -> *mut u8 {
        if self.item_size == 0 {
            self.item_align as *mut u8
        } else {
            self.data.add(row * self.item_size)
        }
    }

    /// # Safety
    /// Same contract as [`get_ptr`](Self::get_ptr).
    #[inline]
    pub unsafe fn get_raw_ptr(&self, row: usize) -> *const u8 {
        self.get_ptr(row)
    }

    /// # Safety
    /// `row < self.len` and `T` must be the column's component type. Returns a
    /// shared reference — no live exclusive reference to the row may exist.
    #[inline]
    pub unsafe fn get<T>(&self, row: usize) -> &T {
        &*(self.get_ptr(row) as *const T)
    }

    /// # Safety
    /// `row < self.len` and `T` must be the column's component type. Returns an
    /// exclusive reference — no other reference to the row may be live.
    #[inline]
    pub unsafe fn get_mut<T>(&mut self, row: usize) -> &mut T {
        &mut *(self.get_ptr(row) as *mut T)
    }

    /// Append a new element at the end, stamping the change and added ticks.
    ///
    /// `push` is the FRESH-value path (spawn / insert of a new component):
    /// added-tick == change-tick == `tick`. To move a row between archetypes
    /// while preserving ticks see [`push_moved`](Self::push_moved).
    ///
    /// # Safety
    /// `src` must point to an initialized value of the column's component type
    /// (`item_size` bytes); ownership is moved into the column (the caller must
    /// not drop the source). Ignored for ZST columns.
    pub unsafe fn push(&mut self, src: *const u8, tick: Tick) {
        if self.len >= self.capacity {
            self.grow();
        }
        if self.item_size > 0 {
            let dst = self.data.add(self.len * self.item_size);
            std::ptr::copy_nonoverlapping(src, dst, self.item_size);
        }
        self.change_ticks.push(TickCell::new(tick));
        self.added_ticks.push(TickCell::new(tick));
        self.len += 1;
    }

    /// Append the ticks of a MOVED row (archetype move): the data has already
    /// been copied by the caller, both ticks are preserved as-is — moving an
    /// entity between archetypes does not "refresh" either `Changed<T>` or
    /// `Added<T>`.
    ///
    /// # Safety
    /// The data for row `len` is already written; called exactly once per row.
    #[inline]
    pub(crate) unsafe fn push_moved_ticks(&mut self, changed: Tick, added: Tick) {
        self.change_ticks.push(TickCell::new(changed));
        self.added_ticks.push(TickCell::new(added));
        self.len += 1;
    }

    /// Write a typed value into the column at the given row and increment `len`.
    ///
    /// Used by the [`#[derive(Bundle)]`](apex_macros::Bundle) derive macro to
    /// write components into an archetype.
    ///
    /// # Safety
    /// The caller guarantees that `T` matches `self.component_id`, and that
    /// `row == self.len` (writes are always at the end of the column).
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

    /// Write an element into an already existing row and update the tick.
    ///
    /// Does NOT drop the previous value — for rows whose contents were already
    /// moved out (move) or are trivial. To overwrite a LIVE value use
    /// [`replace_at`](Self::replace_at), otherwise Drop types leak.
    ///
    /// # Safety
    /// `row < self.len`; `src` points to an initialized value of the column's
    /// component type (`item_size` bytes), whose ownership is moved in. The row's
    /// previous value is NOT dropped — the caller must ensure it was already
    /// moved out or is trivially droppable.
    pub unsafe fn write_at(&mut self, row: usize, src: *const u8, tick: Tick) {
        if self.item_size > 0 {
            std::ptr::copy_nonoverlapping(src, self.get_ptr(row), self.item_size);
        }
        if row < self.change_ticks.len() {
            self.change_ticks[row] = TickCell::new(tick);
        }
    }

    /// Replace a row's LIVE value: drop the old, write the new, stamp the tick.
    ///
    /// Closes the leak of `insert` over an existing component (W2-1): the old
    /// value of a Drop type (String, Vec, Arc…) was previously silently lost.
    ///
    /// # Safety
    /// `row < self.len`; the row currently holds a live, initialized value of
    /// the column's component type (it is dropped here); `src` points to an
    /// initialized replacement value of that type, moved in.
    pub unsafe fn replace_at(&mut self, row: usize, src: *const u8, tick: Tick) {
        debug_assert!(row < self.len);
        (self.drop_fn)(self.get_ptr(row));
        self.write_at(row, src, tick);
    }

    /// Clamp old change/added ticks to the `Tick::MAX_CHANGE_AGE` window (W2-3).
    pub(crate) fn check_change_ticks(&mut self, current: Tick) {
        for t in &mut self.change_ticks {
            t.get_mut().check_against(current);
        }
        for t in &mut self.added_ticks {
            t.get_mut().check_against(current);
        }
    }

    /// # Safety
    /// `row < self.len`. The removed value is dropped exactly once.
    pub unsafe fn swap_remove_and_drop(&mut self, row: usize) {
        debug_assert!(row < self.len);
        let last = self.len - 1;
        // Panic-safety (A8): move the value being removed into the LAST slot and
        // shrink `len`/ticks BEFORE dropping it, so the value lives outside the
        // live range `[0, len)` when its `Drop` runs. If that `Drop` panics
        // mid-unwind, `Drop for Column` (which only walks `[0, len)`) will not
        // drop the slot a second time — the old order (drop-then-shrink) left a
        // dropped value inside the live range → double-drop / double-panic-abort.
        if row != last && self.item_size > 0 {
            // Swap the two rows' bytes: the ex-last value fills the hole at
            // `row`, the removed value lands at `last`.
            std::ptr::swap_nonoverlapping(self.get_ptr(row), self.get_ptr(last), self.item_size);
        }
        self.change_ticks.swap(row, last);
        self.added_ticks.swap(row, last);
        self.change_ticks.pop();
        self.added_ticks.pop();
        self.len -= 1;
        // `last == old len - 1 == new len`, i.e. outside the live range now.
        (self.drop_fn)(self.get_ptr(last));
    }

    /// # Safety
    /// `row < self.len`. The removed value is NOT dropped — its ownership is
    /// considered moved out and the caller is responsible for it.
    pub unsafe fn swap_remove_no_drop(&mut self, row: usize) {
        debug_assert!(row < self.len);
        let last = self.len - 1;
        if row != last && self.item_size > 0 {
            let remove_ptr = self.get_ptr(row);
            std::ptr::copy_nonoverlapping(self.get_ptr(last), remove_ptr, self.item_size);
        }
        if row != last {
            self.change_ticks.swap(row, last);
            self.added_ticks.swap(row, last);
        }
        self.change_ticks.pop();
        self.added_ticks.pop();
        self.len -= 1;
    }

    pub(crate) fn grow(&mut self) {
        let new_cap = if self.capacity == 0 {
            // Target size of the first allocation: ~256 bytes minimum, but no more
            // than 64 elements. For large components (Mat4=64B, Transform=~48B) — 4
            // elements. For small ones (f32=4B, u8=1B) — 64 elements.
            if self.item_size == 0 {
                64
            } else {
                // 256 bytes / item_size, clamped to [4, 64]
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
            // First allocation — via alloc (realloc with a null ptr is UB)
            let new_layout = self.layout_for(new_cap);
            self.data = unsafe {
                let ptr = alloc(new_layout);
                assert!(!ptr.is_null(), "allocation failed");
                ptr
            };
        } else {
            // Reallocation — realloc: one syscall instead of alloc+copy+dealloc.
            // `new_size` via `layout_for` — the same checked_mul as in the alloc
            // branch (A11: the neighboring path previously multiplied without an
            // overflow check).
            let old_layout = self.layout_for(self.capacity);
            let new_size = self.layout_for(new_cap).size();
            self.data = unsafe {
                let ptr = realloc(self.data, old_layout, new_size);
                assert!(!ptr.is_null(), "reallocation failed");
                ptr
            };
        }
        self.capacity = new_cap;
    }

    /// Pre-allocate memory for `additional` elements.
    /// Avoids multiple grow() calls during bulk spawns.
    pub(crate) fn reserve(&mut self, additional: usize) {
        let needed = self.len + additional;
        if needed <= self.capacity {
            self.change_ticks.reserve(additional);
            self.added_ticks.reserve(additional);
            return;
        }
        let new_cap = needed.next_power_of_two().max(4);
        if self.item_size == 0 {
            self.capacity = new_cap;
            self.change_ticks.reserve(additional);
            self.added_ticks.reserve(additional);
            return;
        }
        if self.capacity == 0 {
            // First allocation — via alloc
            let new_layout = self.layout_for(new_cap);
            self.data = unsafe {
                let ptr = alloc(new_layout);
                assert!(!ptr.is_null(), "allocation failed");
                ptr
            };
        } else {
            // Reallocation — realloc: one syscall instead of alloc+copy+dealloc.
            // `new_size` via `layout_for` — the same checked_mul as in the alloc
            // branch (A11: the neighboring path previously multiplied without an
            // overflow check).
            let old_layout = self.layout_for(self.capacity);
            let new_size = self.layout_for(new_cap).size();
            self.data = unsafe {
                let ptr = realloc(self.data, old_layout, new_size);
                assert!(!ptr.is_null(), "reallocation failed");
                ptr
            };
        }
        self.capacity = new_cap;
        self.change_ticks.reserve(additional);
        self.added_ticks.reserve(additional);
    }

    /// Change tick for row `row`
    #[inline]
    pub fn get_tick(&self, row: usize) -> Tick {
        self.change_ticks
            .get(row)
            .map(|c| c.get())
            .unwrap_or(Tick::ZERO)
    }

    /// Component-added tick for row `row` (W3-1, `Added<T>`).
    #[inline]
    pub fn get_added_tick(&self, row: usize) -> Tick {
        self.added_ticks
            .get(row)
            .map(|c| c.get())
            .unwrap_or(Tick::ZERO)
    }

    /// Pointer to the tick array — for zero-cost Changed<T> query.
    /// `TickCell` is `#[repr(transparent)]` over `Tick`, so the base pointer is
    /// a valid `*const Tick` over the whole buffer.
    #[inline]
    pub fn ticks_ptr(&self) -> *const Tick {
        self.change_ticks.as_ptr() as *const Tick
    }

    /// Pointer to the added-ticks array — for zero-cost `Added<T>` query
    #[inline]
    pub fn added_ticks_ptr(&self) -> *const Tick {
        self.added_ticks.as_ptr() as *const Tick
    }

    /// Raw pointer to the data — for chunk-level parallelism
    #[inline]
    pub fn data_ptr(&self) -> *mut u8 {
        self.data
    }

    /// Allocated memory of the column: (data bytes, tick bytes) — for
    /// [`World::archetype_stats`](crate::World::archetype_stats) (W3-5).
    pub(crate) fn allocated_bytes(&self) -> (usize, usize) {
        let data = if self.item_size == 0 {
            0
        } else {
            self.capacity * self.item_size
        };
        let ticks = (self.change_ticks.capacity() + self.added_ticks.capacity())
            * std::mem::size_of::<Tick>();
        (data, ticks)
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

    /// Column linear-search threshold: at ≤8 components, scanning the inline
    /// SmallVec is faster than an FxHashMap lookup (CR-M3; benchmark — frag_world
    /// random get_mut).
    const COLUMN_LINEAR_MAX: usize = 8;

    #[inline]
    pub fn column_index(&self, component_id: ComponentId) -> Option<usize> {
        // Invariant: columns are built in the order of component_ids, so the
        // position in component_ids == the value in column_map.
        if self.component_ids.len() <= Self::COLUMN_LINEAR_MAX {
            self.component_ids.iter().position(|&c| c == component_id)
        } else {
            self.column_map.get(&component_id).copied()
        }
    }

    #[inline]
    pub fn has_component(&self, component_id: ComponentId) -> bool {
        if self.component_ids.len() <= Self::COLUMN_LINEAR_MAX {
            self.component_ids.contains(&component_id)
        } else {
            self.column_map.contains_key(&component_id)
        }
    }

    /// # Safety
    /// `row < self.len` and `T` must be the component type registered under
    /// `component_id`. Returns a shared reference; no exclusive reference to the
    /// row may be live. `None` if the archetype lacks the component.
    pub unsafe fn get_component<T>(&self, row: usize, component_id: ComponentId) -> Option<&T> {
        let col_idx = self.column_index(component_id)?;
        Some(self.columns[col_idx].get::<T>(row))
    }

    /// # Safety
    /// `row < self.len` and `T` must be the component type registered under
    /// `component_id`. Returns an exclusive reference; no other reference to the
    /// row may be live. `None` if the archetype lacks the component.
    pub unsafe fn get_component_mut<T>(
        &mut self,
        row: usize,
        component_id: ComponentId,
    ) -> Option<&mut T> {
        let col_idx = self.column_index(component_id)?;
        Some(self.columns[col_idx].get_mut::<T>(row))
    }

    /// The change tick of one component in the given row (`None` if the
    /// archetype lacks the component). Manual change detection for dynamic
    /// consumers (RT-1) — compare with `Tick::is_newer_than`.
    pub fn change_tick_of(&self, row: usize, component_id: ComponentId) -> Option<Tick> {
        let col_idx = self.column_index(component_id)?;
        Some(self.columns[col_idx].get_tick(row))
    }

    /// Update the change tick for a component in the given row.
    ///
    /// Uses raw pointers for interior mutation through `&self`, which allows
    /// updating the tick without `&mut self`.
    ///
    /// # Safety
    /// - `row` must be < the column's len
    /// - No other mutable reference to the column may exist
    pub unsafe fn set_change_tick(&self, row: usize, component_id: ComponentId, tick: Tick) {
        if let Some(col_idx) = self.column_index(component_id) {
            self.columns[col_idx].set_change_tick(row, tick);
        }
    }

    /// Reserve a fresh entity row (columns are filled separately). Returns the
    /// new row index.
    ///
    /// # Safety
    /// The caller must initialise every column at the returned row (via
    /// [`write_component`](Self::write_component)) before the row is read, so the
    /// archetype's parallel `entities`/columns stay length-consistent.
    pub unsafe fn allocate_row(&mut self, entity: Entity) -> usize {
        let row = self.entities.len();
        self.entities.push(entity);
        row
    }

    /// # Safety
    /// `src` points to an initialized value of the component type registered
    /// under `component_id`, moved into the column; `row` is either `col.len`
    /// (append) or `< col.len` (overwrite of an already moved-out/trivial slot —
    /// the old value is NOT dropped). No-op if the archetype lacks the component.
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

    /// Write a component: into a new row — push, over a live value — replace
    /// (dropping the old one). Used by the batched `insert_parts` (W2-1).
    ///
    /// # Safety
    /// `src` points to an initialized value of the component type registered
    /// under `component_id`, moved in. For `row < col.len` the existing live
    /// value is dropped and replaced; for `row == col.len` it is appended.
    /// No-op if the archetype lacks the component.
    pub unsafe fn write_or_replace_component(
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
                col.replace_at(row, src, tick);
            }
        }
    }

    /// Remove `row` from every column (dropping its values) and swap-remove the
    /// entity slot. Returns the entity that was relocated into `row` (the ex-last
    /// row), or `None` if `row` was already last.
    ///
    /// # Safety
    /// `row < self.len` and all columns share that length (row-consistent).
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

    /// Public column iterator via ColumnView (safe, no raw).
    pub fn columns(&self) -> impl Iterator<Item = ColumnView<'_>> {
        self.columns.iter().map(|col| ColumnView { col })
    }

    /// Raw column slice — for the apex-scripting query iterator.
    ///
    /// # Safety
    /// The caller must guarantee that:
    /// - There are no concurrent structural changes during iteration
    /// - Row indices do not exceed col.len
    ///
    /// Used only by `RhaiQueryIter` in a single-threaded context.
    #[inline]
    pub fn columns_raw(&self) -> &[Column] {
        &self.columns
    }

    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }
}

/// Description of a single archetype chunk for chunk-level parallelism.
///
/// Holds raw pointers to data slices [start, start+len).
/// SAFETY: used only inside a Rayon scope while the archetype is alive
/// and no structural changes occur.
pub struct ArchetypeChunk<'a> {
    pub entities: &'a [Entity],
    pub arch_id: ArchetypeId,
    /// Index of the start row within the archetype (for column_index lookup)
    pub start_row: usize,
    pub len: usize,
}

/// Split an archetype into fixed-size chunks.
///
/// Returns `entities` slices of length `chunk_size` (the last may be smaller).
/// Used by `par_for_each` for parallel iteration within a single archetype.
pub fn archetype_chunks(
    arch: &Archetype,
    chunk_size: usize,
) -> impl Iterator<Item = ArchetypeChunk<'_>> {
    let total = arch.entities.len();
    let num_chunks = total.div_ceil(chunk_size);
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
            map_entities: None,
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
                let stored: &f32 = col.get::<f32>(i);
                assert_eq!(*stored, i as f32);
            }
            assert_eq!(col.get_tick(i).0, i as u32);
        }
    }

    /// A8 regression: if a component's `Drop` panics during
    /// `swap_remove_and_drop`, the removed value must be dropped exactly once
    /// and no live value double-dropped. The fix shrinks `len` before running
    /// the drop, so the panicking slot sits outside the live range and
    /// `Drop for Column` skips it. Pre-fix this dropped the value while it was
    /// still inside `[0, len)`, so column teardown dropped it again (a second
    /// panic during unwind = process abort).
    #[test]
    fn swap_remove_and_drop_panic_no_double_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        static PANIC_ON: AtomicUsize = AtomicUsize::new(usize::MAX);

        unsafe fn counting_panic_drop(ptr: *mut u8) {
            let v = *(ptr as *const u32) as usize;
            DROPS.fetch_add(1, Ordering::SeqCst);
            if v == PANIC_ON.load(Ordering::SeqCst) {
                panic!("drop panicked on {v}");
            }
        }

        DROPS.store(0, Ordering::SeqCst);
        PANIC_ON.store(10, Ordering::SeqCst);

        let info = ComponentInfo {
            id: ComponentId(0),
            name: "panicky",
            type_id: std::any::TypeId::of::<u32>(),
            size: std::mem::size_of::<u32>(),
            align: std::mem::align_of::<u32>(),
            drop_fn: counting_panic_drop,
            serde: None,
            map_entities: None,
        };
        let mut col = Column::new(&info);
        for v in [10u32, 20, 30] {
            unsafe { col.push(&v as *const u32 as *const u8, Tick(1)) };
        }

        // Remove row 0 (value 10) — its Drop panics. Contained by catch_unwind.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            col.swap_remove_and_drop(0);
        }));
        assert!(result.is_err(), "the panicking Drop must propagate");

        // Exactly one drop so far: the removed value, dropped once. The hole was
        // filled by the ex-last value (30); nothing else has been dropped.
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
        assert_eq!(col.len, 2);
        unsafe {
            assert_eq!(*col.get::<u32>(0), 30, "last value filled the hole");
            assert_eq!(*col.get::<u32>(1), 20);
        }

        // Tearing down the column drops only the two live values (20, 30),
        // neither of which is PANIC_ON — so no second drop of value 10.
        PANIC_ON.store(usize::MAX, Ordering::SeqCst);
        drop(col);
        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            3,
            "removed value dropped once + two live values — no double-drop"
        );
    }

    /// A11: the column allocation size must go through `checked_mul`
    /// (`layout_for`) on every path — a huge `item_size` that overflows
    /// `item_size * capacity` must panic loudly, not silently wrap into a
    /// too-small allocation (heap corruption). The realloc path now reuses the
    /// same `layout_for` as the alloc path, so both are covered.
    #[test]
    #[should_panic(expected = "overflow in layout_for")]
    fn column_alloc_size_overflow_panics_loudly() {
        let info = ComponentInfo {
            id: ComponentId(0),
            name: "huge",
            type_id: std::any::TypeId::of::<u8>(),
            size: usize::MAX / 2,
            align: 1,
            drop_fn: noop_drop,
            serde: None,
            map_entities: None,
        };
        let mut col = Column::new(&info);
        // new_cap == 4 → item_size * 4 overflows usize → checked_mul panics.
        col.reserve(4);
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
        // The last element (4.0) is moved into the removed slot (1)
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
            map_entities: None,
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

        let val: f32 = 2.5;
        unsafe {
            arch.write_component(0, ComponentId(0), &val as *const f32 as *const u8, Tick(1));
        }

        unsafe {
            let stored: &f32 = arch.get_component::<f32>(0, ComponentId(0)).unwrap();
            assert_eq!(*stored, 2.5);
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
        assert_eq!(swapped, Some(e2)); // the last one moved into slot 0
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
