use crate::par_utils::compute_par_chunks;
use crate::{
    access::AccessDescriptor,
    archetype::Archetype,
    component::{Component, ComponentId, Tick},
    entity::Entity,
    sub_world::SubWorld,
    system_param::WorldQuerySystemAccess,
    unsafe_world_cell::UnsafeWorldCell,
    world::World,
};

// ── WorldQuery ─────────────────────────────────────────────────

/// Inline buffer of ComponentId for a query shape: up to 8 components with NO
/// heap allocation (W2-0 — `fill_ids` on the hot path `ctx.query`, every call).
pub type IdBuf = smallvec::SmallVec<[ComponentId; 8]>;

/// Component roles in the query cache key (high bits above ComponentId).
pub const KEY_ROLE_WITHOUT: u64 = 1 << 32;
pub const KEY_ROLE_OPTIONAL: u64 = 2 << 32;
/// Structural markers for an `Or<>` group in the cache key (W2-5). They carry no
/// ComponentId and are SKIPPED when reconstructing ids from the key (see
/// [`KEY_MARKER_BIT`]): `(Or<(With<A>,)>, With<B>)` and `Or<(With<A>, With<B>)>`
/// must produce different cache entries — they have different match semantics
/// for the same ids.
pub const KEY_OR_OPEN: u64 = 4 << 32;
pub const KEY_OR_CLOSE: u64 = 5 << 32;
/// Bit "the key entry is a structural marker, not a component".
pub const KEY_MARKER_BIT: u64 = 4 << 32;

/// # Safety
///
/// The implementation is part of the unsafe iteration contract (safe code in
/// `for_each`/`iter` relies on it without checks):
/// - `fetch_state`/`fetch_item` are called only for an archetype that passed
///   `matches_archetype`, with `row < arch.len()`; the returned `Item`s must not
///   alias other rows.
/// - If `has_row_filter()` returns `false`, then `fetch_item` MUST return `Some`
///   for EVERY row of a matched non-empty archetype — otherwise
///   `fetch_item_unchecked` (its `unwrap_unchecked`) is UB. A shape that may
///   filter out a row must return `has_row_filter() == true`.
/// - `component_count()` = the number of entries that `fill_ids`/`fill_cache_key`
///   push, and equals the number of segments `fetch_state` expects.
pub unsafe trait WorldQuery: Sized {
    type Item<'w>;
    type State: Copy;

    /// Read-only projection of this shape: every `&mut T` / `Mut<T>` becomes
    /// `&T`. `Read<T>::ReadOnly = Read<T>`, `Write<T>::ReadOnly = Read<T>`,
    /// filters map to themselves, tuples element-wise. Lets a shared-borrow
    /// (`&self`) iterator hand out read-only items even for a write-shaped query
    /// (Bevy `QueryData::ReadOnly`): the bound guarantees the projection yields
    /// no mutable component access.
    type ReadOnly: ReadOnlyWorldQuery;

    fn component_count() -> usize;

    /// Fills the ComponentId of the shape. INVARIANT (W2): every shape pushes
    /// EXACTLY `component_count()` entries — an unregistered component is encoded
    /// with the [`ComponentId::INVALID`] sentinel. This guarantees the alignment
    /// of id segments in tuples/`Or` for any combination of registrations; an
    /// "empty query for an unregistered required component" falls out naturally:
    /// `has_component(INVALID)` matches nowhere.
    fn fill_ids(world: &World, ids: &mut IdBuf);

    /// Fills only the "positive" (non-Without) component IDs.
    /// By default — the same as fill_ids.
    fn fill_positive_ids(world: &World, ids: &mut IdBuf) {
        Self::fill_ids(world, ids);
    }

    /// Fills only the REQUIRED component IDs — those without which an archetype
    /// definitely cannot match (Read/Write/Ref/With/Changed). Unlike
    /// `fill_positive_ids` it does NOT include optionals (`Maybe`/`MaybeWrite`).
    /// Used by `Query::new` to pick candidate archetypes from
    /// `component_arch_index` (candidates = archetypes of the rarest required
    /// component).
    fn fill_required_ids(world: &World, ids: &mut IdBuf) {
        Self::fill_positive_ids(world, ids);
    }

    /// Query cache key (CR-M2b): one `u64` per component — ComponentId in the low
    /// 32 bits, the role in the high bits ([`KEY_ROLE_WITHOUT`]/[`KEY_ROLE_OPTIONAL`];
    /// required — 0). Unambiguously encodes the match semantics of the query shape
    /// ((Read<A>, Read<B>) ≠ (Read<A>, Without<B>) ≠ (Read<A>, Maybe<B>)).
    ///
    /// IMPORTANT: the sequence of low 32 bits must match `fill_ids` (CachedQuery
    /// reconstructs the ids from the key without a second registry pass). A single
    /// pass, no heap allocations up to 8 components.
    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>);

    /// Reports this form's DATA borrows for the runtime self-alias check (C2):
    /// pushes `(component_id, exclusive)` for every component whose data this
    /// form hands out as a reference. Pure filters (`With`/`Without`/`Changed`/
    /// `Added`/`Or`), `Entity` and `()` borrow no component data, so the default
    /// is a no-op; data-yielding forms (`Read`/`Write`/`&T`/`&mut T`/`Maybe`/
    /// `MaybeWrite`) and tuples override / union. Used by
    /// [`Query`](crate::query::Query) constructors to reject shapes that would
    /// alias one row's data (e.g. `Query<(&mut T, &mut T)>`).
    fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
        let _ = (world, out);
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool;

    /// Captures per-archetype state for iteration.
    ///
    /// `last_run` — the tick of the previous run (for `Changed<T>` filtering).
    /// `this_run` — the current world tick; `Write<T>`/`MaybeWrite<T>` stamp it
    /// into the row's change-tick on `DerefMut` through the returned `Mut<T>`.
    ///
    /// # Safety
    /// `arch` must have matched this query (`matches_archetype`), and `ids` must
    /// be this query's component-id list. The returned state holds raw pointers
    /// into `arch` valid only until the next structural change.
    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        last_run: Tick,
        this_run: Tick,
    ) -> Self::State;

    /// # Safety
    /// `state` must come from [`fetch_state`](Self::fetch_state) on the same
    /// archetype and `row` must be a valid row of it. For write-forms the row
    /// must be accessed exclusively (no aliasing across parallel chunks).
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>>;

    /// `true` if `fetch_item` may return `None` on SOME rows of an
    /// already-matched archetype — i.e. the shape carries a **per-row** filter
    /// (`Changed`/`Added`/`Or`/a tuple with such). `false` ⇒ for a matched
    /// archetype `fetch_item` is ALWAYS `Some`, so iteration may skip the per-row
    /// Option check and run a dense loop through
    /// [`fetch_item_unchecked`](Self::fetch_item_unchecked) (Bevy's
    /// "archetype-level filter" fast-path; perf campaign §3.1A). This is a **pure
    /// perf flag**: the semantics do not change (`Mut<T>` still stamps the
    /// change-tick on `DerefMut`, and lazy entity loading for row filters is
    /// preserved).
    ///
    /// Default `false` — most shapes (`Read`/`Write`/`With`/`Without`/
    /// `Maybe`/`Entity`/`()`) are infallible. Only per-row filters and the
    /// combinators over them override it.
    #[inline(always)]
    fn has_row_filter() -> bool {
        false
    }

    /// Infallible variant of [`fetch_item`](Self::fetch_item) for
    /// archetype-level shapes. Call ONLY when [`has_row_filter`](Self::has_row_filter)
    /// is false — otherwise UB (`unwrap_unchecked` on a per-row-filtered row).
    /// The default delegates to `fetch_item().unwrap_unchecked()`: for infallible
    /// shapes the optimizer inlines the `Some(..)` construction and eliminates
    /// the Option entirely.
    ///
    /// # Safety
    /// Same contract as [`fetch_item`](Self::fetch_item), AND
    /// [`has_row_filter`](Self::has_row_filter) must be `false` for this shape —
    /// otherwise the internal `unwrap_unchecked` hits a `None` (UB).
    #[inline(always)]
    unsafe fn fetch_item_unchecked<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        Self::fetch_item(state, row).unwrap_unchecked()
    }

    fn is_filter() -> bool {
        false
    }
    /// Returns true for components that MUST be present.
    /// For Without<T> returns false.
    fn is_positive() -> bool {
        true
    }
}

// ── Mut<T> — smart-pointer for Write<T> (change detection) ──────

/// Mutable access to a component with automatic change-detection.
///
/// Returned from `Query<Write<T>>`. On `DerefMut` it stamps the current world
/// tick into the row's change-tick → `Changed<T>` reliably fires on ALL mutation
/// paths (not only `World::get_mut`). Semantics as in Bevy `Mut<T>`: any mutable
/// borrow marks the component changed, even if it was in fact only read — an
/// acceptable and standard trade-off.
pub struct Mut<'w, T: 'static> {
    pub(crate) value: &'w mut T,
    /// Pointer to this row's change-tick (inside `Column::change_ticks`).
    pub(crate) change_tick: *mut Tick,
    /// Current world tick — stamped on `DerefMut`.
    pub(crate) this_run: Tick,
}

impl<T: 'static> Mut<'_, T> {
    /// Explicitly mark the component changed without mutating the value.
    #[inline]
    pub fn set_changed(&mut self) {
        unsafe { *self.change_tick = self.this_run };
    }

    /// Get `&mut T` without marking a change (escape-hatch).
    #[inline]
    pub fn bypass_change_detection(&mut self) -> &mut T {
        self.value
    }
}

impl<T: 'static> std::ops::Deref for Mut<'_, T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: 'static> std::ops::DerefMut for Mut<'_, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { *self.change_tick = self.this_run };
        self.value
    }
}

impl<T: std::fmt::Debug + 'static> std::fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

/// Per-archetype state of `Write<T>`: base data pointer + pointer to the
/// change-ticks array + current world tick.
pub struct WriteState<T> {
    data: *mut T,
    ticks: *mut Tick,
    this_run: Tick,
}

impl<T> Clone for WriteState<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for WriteState<T> {}

// ── Read<T> ────────────────────────────────────────────────────

pub struct Read<T: Component>(std::marker::PhantomData<T>);

unsafe impl<T: Component> WorldQuery for Read<T> {
    type Item<'w> = &'w T;
    type State = *const T;
    type ReadOnly = Read<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // role REQUIRED = 0
    }

    fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
        out.push((
            world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID),
            false,
        ));
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], _: Tick, _: Tick) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        arch.columns[col_idx].get_ptr(0) as *const T
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        Some(&*state.add(row))
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Read<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── Write<T> ───────────────────────────────────────────────────

pub struct Write<T: Component>(std::marker::PhantomData<T>);

unsafe impl<T: Component> WorldQuery for Write<T> {
    type Item<'w> = Mut<'w, T>;
    type State = WriteState<T>;
    type ReadOnly = Read<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // role REQUIRED = 0
    }

    fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
        out.push((
            world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID),
            true,
        ));
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        this_run: Tick,
    ) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        WriteState {
            data: col.get_ptr(0) as *mut T,
            ticks: col.ticks_ptr() as *mut Tick,
            this_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        Some(Mut {
            value: &mut *state.data.add(row),
            change_tick: state.ticks.add(row),
            this_run: state.this_run,
        })
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Write<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().write::<T>()
    }
}

// ── ReadOnlyWorldQuery ─────────────────────────────────────────

/// Marker for query shapes that can never hand out mutable component access.
///
/// This is the type-level knob of the borrow model: `Query::new(&World)` (and
/// every other shared-`&World` constructor) is only available for shapes that
/// are read-only end to end, so safe code cannot obtain two aliasing `&mut T`
/// through shared world borrows. Write shapes construct from `&mut World`
/// ([`Query::new_mut`]) or through the scheduler's validated unsafe escape.
///
/// # Safety
/// Implement only for shapes whose `Item` (and the items of every nested
/// element) provides no mutable access to component data. `Write<T>`,
/// `&mut T` and `MaybeWrite<T>` must NOT implement this.
pub unsafe trait ReadOnlyWorldQuery: WorldQuery {}

// SAFETY: items are shared references / entity ids / filter unit types.
unsafe impl<T: Component> ReadOnlyWorldQuery for Read<T> {}
unsafe impl<T: Component> ReadOnlyWorldQuery for &T {}
unsafe impl ReadOnlyWorldQuery for Entity {}
unsafe impl<T: Component> ReadOnlyWorldQuery for With<T> {}
unsafe impl<T: Component> ReadOnlyWorldQuery for Without<T> {}
unsafe impl<T: Component> ReadOnlyWorldQuery for Maybe<T> {}
unsafe impl<T: Component> ReadOnlyWorldQuery for Changed<T> {}
unsafe impl<T: Component> ReadOnlyWorldQuery for Added<T> {}
unsafe impl ReadOnlyWorldQuery for () {}

/// `&T` as a query specifier (1:1 port from Bevy). Delegates to [`Read<T>`],
/// yields `&T`.
unsafe impl<'a, T: Component> WorldQuery for &'a T {
    type Item<'w> = &'w T;
    type State = <Read<T> as WorldQuery>::State;
    type ReadOnly = &'a T;

    #[inline]
    fn component_count() -> usize {
        <Read<T> as WorldQuery>::component_count()
    }
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        <Read<T> as WorldQuery>::fill_ids(world, ids)
    }
    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        <Read<T> as WorldQuery>::fill_cache_key(world, key)
    }
    fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
        <Read<T> as WorldQuery>::fill_data_access(world, out)
    }
    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        <Read<T> as WorldQuery>::matches_archetype(arch, ids)
    }
    #[inline]
    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], lr: Tick, tr: Tick) -> Self::State {
        <Read<T> as WorldQuery>::fetch_state(arch, ids, lr, tr)
    }
    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        <Read<T> as WorldQuery>::fetch_item(state, row)
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for &T {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

/// `&mut T` as a query specifier (1:1 port from Bevy). Delegates to
/// [`Write<T>`], yields [`Mut<T>`] (stamping the change-tick on `DerefMut`).
unsafe impl<'a, T: Component> WorldQuery for &'a mut T {
    type Item<'w> = Mut<'w, T>;
    type State = <Write<T> as WorldQuery>::State;
    type ReadOnly = &'a T;

    #[inline]
    fn component_count() -> usize {
        <Write<T> as WorldQuery>::component_count()
    }
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        <Write<T> as WorldQuery>::fill_ids(world, ids)
    }
    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        <Write<T> as WorldQuery>::fill_cache_key(world, key)
    }
    fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
        <Write<T> as WorldQuery>::fill_data_access(world, out)
    }
    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        <Write<T> as WorldQuery>::matches_archetype(arch, ids)
    }
    #[inline]
    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], lr: Tick, tr: Tick) -> Self::State {
        <Write<T> as WorldQuery>::fetch_state(arch, ids, lr, tr)
    }
    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        <Write<T> as WorldQuery>::fetch_item(state, row)
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for &mut T {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().write::<T>()
    }
}

// ── Entity as a query shape (P1/TD-8, Bevy parity) ─────────────

/// `Entity` — an ordinary query shape: `Query<(Entity, &Pos)>` yields the
/// entity id as part of the item. After P1, `iter()`/the for-loop yield ONLY the
/// item — entity is no longer forced; if you need it, request it explicitly (as
/// in Bevy).
unsafe impl WorldQuery for Entity {
    type Item<'w> = Entity;
    /// Pointer to the `Archetype::entities` array (valid as long as the world is
    /// alive and there is no structural change — the standard iteration
    /// invariant).
    type State = *const Entity;
    type ReadOnly = Entity;

    #[inline]
    fn component_count() -> usize {
        0
    }

    fn fill_ids(_world: &World, _ids: &mut IdBuf) {}

    fn fill_cache_key(_world: &World, _key: &mut smallvec::SmallVec<[u64; 8]>) {}

    fn matches_archetype(_arch: &Archetype, _ids: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(arch: &Archetype, _: &[ComponentId], _: Tick, _: Tick) -> Self::State {
        arch.entities().as_ptr()
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        Some(*state.add(row))
    }
}

impl WorldQuerySystemAccess for Entity {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new()
    }
}

// ── With<T> ────────────────────────────────────────────────────

pub struct With<T: Component>(std::marker::PhantomData<T>);

unsafe impl<T: Component> WorldQuery for With<T> {
    type Item<'w> = ();
    type State = ();
    type ReadOnly = With<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // role REQUIRED = 0
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(_: &Archetype, _: &[ComponentId], _: Tick, _: Tick) -> Self::State {}

    #[inline(always)]
    unsafe fn fetch_item<'w>(_: Self::State, _: usize) -> Option<Self::Item<'w>> {
        Some(())
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for With<T> {
    fn system_access() -> AccessDescriptor {
        // With<T> only checks presence — read semantics
        AccessDescriptor::new().read::<T>()
    }
}

// ── Without<T> ─────────────────────────────────────────────────

pub struct Without<T: Component>(std::marker::PhantomData<T>);

unsafe impl<T: Component> WorldQuery for Without<T> {
    type Item<'w> = ();
    type State = ();
    type ReadOnly = Without<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }
    #[inline]
    fn is_positive() -> bool {
        false
    }

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_positive_ids(_: &World, _: &mut IdBuf) {}

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64 | KEY_ROLE_WITHOUT);
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        ids.is_empty() || !arch.has_component(ids[0])
    }

    unsafe fn fetch_state(_: &Archetype, _: &[ComponentId], _: Tick, _: Tick) -> Self::State {}

    #[inline(always)]
    unsafe fn fetch_item<'w>(_: Self::State, _: usize) -> Option<Self::Item<'w>> {
        Some(())
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Without<T> {
    fn system_access() -> AccessDescriptor {
        // Without does not read T's data — no access to T at all
        AccessDescriptor::new()
    }
}

// ── Maybe<T> — optional read (Optional <T>) ────────────────────

/// Optional component — the analogue of `Option<&T>`.
///
/// Unlike `Read<T>`, it does not require the component to be present.
/// Always iterates every entity: if the component is absent — returns `None`.
///
/// # Example
///
/// ```ignore
/// Query::<(Read<A>, Maybe<B>)>::new(&world)
///     .for_each(|entity, (a, b)| {
///         // a: &A — always present
///         // b: Option<&B> — may be None
///     });
/// ```
pub struct Maybe<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct MaybeState {
    data: *const u8,
    item_size: usize,
    present: bool,
}

unsafe impl Send for MaybeState {}
unsafe impl Sync for MaybeState {}

impl MaybeState {
    fn absent() -> Self {
        MaybeState {
            data: std::ptr::null(),
            item_size: 0,
            present: false,
        }
    }
}

unsafe impl<T: Component> WorldQuery for Maybe<T> {
    type Item<'w> = Option<&'w T>;
    type State = MaybeState;
    type ReadOnly = Maybe<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }

    /// Optional ALWAYS pushes an entry (the [`ComponentId::INVALID`] sentinel for
    /// an unregistered T) — otherwise components AFTER it in the tuple would read
    /// the wrong ids (alignment by `component_count`).
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    /// Optional component: presence is NOT required — does not narrow candidates.
    fn fill_required_ids(_: &World, _: &mut IdBuf) {}

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64 | KEY_ROLE_OPTIONAL);
    }

    fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
        // Optional shared read: still aliases if paired with a write of the same T.
        out.push((
            world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID),
            false,
        ));
    }

    fn matches_archetype(_: &Archetype, _: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        _: Tick,
    ) -> Self::State {
        // The INVALID sentinel is not matched by has_component in any archetype.
        if ids.is_empty() || !arch.has_component(ids[0]) {
            return MaybeState::absent();
        }
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        MaybeState {
            data: col.data,
            item_size: col.item_size,
            present: true,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        if state.present {
            if state.item_size == 0 {
                // ZST — the column allocates no memory, use a dangling pointer
                Some(Some(&*(std::ptr::NonNull::<T>::dangling().as_ptr())))
            } else {
                Some(Some(&*(state.data.add(row * state.item_size) as *const T)))
            }
        } else {
            Some(None)
        }
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Maybe<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── MaybeWrite<T> — optional write (Optional <&mut T>) ─────────

/// Optional mutable component — the analogue of `Option<&mut T>`.
///
/// Always iterates every entity: if the component is absent — returns `None`.
pub struct MaybeWrite<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct MaybeMutState {
    data: *mut u8,
    ticks: *mut Tick,
    item_size: usize,
    present: bool,
    this_run: Tick,
}

unsafe impl Send for MaybeMutState {}
unsafe impl Sync for MaybeMutState {}

impl MaybeMutState {
    fn absent() -> Self {
        MaybeMutState {
            data: std::ptr::null_mut(),
            ticks: std::ptr::null_mut(),
            item_size: 0,
            present: false,
            this_run: Tick::ZERO,
        }
    }
}

unsafe impl<T: Component> WorldQuery for MaybeWrite<T> {
    type Item<'w> = Option<Mut<'w, T>>;
    type State = MaybeMutState;
    type ReadOnly = Maybe<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }

    /// Optional ALWAYS pushes an entry (the [`ComponentId::INVALID`] sentinel for
    /// an unregistered T) — id alignment by `component_count`.
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    /// Optional component: presence is NOT required — does not narrow candidates.
    fn fill_required_ids(_: &World, _: &mut IdBuf) {}

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64 | KEY_ROLE_OPTIONAL);
    }

    fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
        out.push((
            world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID),
            true,
        ));
    }

    fn matches_archetype(_: &Archetype, _: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        this_run: Tick,
    ) -> Self::State {
        // The INVALID sentinel is not matched by has_component in any archetype.
        if ids.is_empty() || !arch.has_component(ids[0]) {
            return MaybeMutState::absent();
        }
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        MaybeMutState {
            data: col.data,
            ticks: col.ticks_ptr() as *mut Tick,
            item_size: col.item_size,
            present: true,
            this_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        if state.present {
            let value = if state.item_size == 0 {
                &mut *std::ptr::NonNull::<T>::dangling().as_ptr()
            } else {
                &mut *(state.data.add(row * state.item_size) as *mut T)
            };
            Some(Some(Mut {
                value,
                change_tick: state.ticks.add(row),
                this_run: state.this_run,
            }))
        } else {
            Some(None)
        }
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for MaybeWrite<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().write::<T>()
    }
}

// ── Changed<T> ─────────────────────────────────────────────────

pub struct Changed<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct ChangedState {
    ticks: *const Tick,
    last_run: Tick,
}

unsafe impl Send for ChangedState {}
unsafe impl Sync for ChangedState {}

unsafe impl<T: Component> WorldQuery for Changed<T> {
    type Item<'w> = ();
    type State = ChangedState;
    type ReadOnly = Changed<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }
    #[inline(always)]
    fn has_row_filter() -> bool {
        true
    }

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // role REQUIRED = 0
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        last_run: Tick,
        _: Tick,
    ) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        ChangedState {
            ticks: col.ticks_ptr(),
            last_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        let tick = *state.ticks.add(row);
        if tick.is_newer_than(state.last_run) {
            Some(())
        } else {
            None
        }
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Changed<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── Added<T> ───────────────────────────────────────────────────

/// Filter "component `T` was ADDED to the entity after `last_run`" (W3-1, parity
/// with Bevy `Added<T>`).
///
/// Added-tick semantics: set when the component APPEARS on the entity
/// (spawn / insert of a new one), survives an archetype move (insert/remove of
/// neighboring components), and is NOT updated by mutation (`Changed`) nor by an
/// `insert` over an existing component (replace = Changed, not Added — as in
/// Bevy). A per-row filter: like `Changed<T>`, it does not compile with dense
/// iteration ([`DenseQuery`](crate::dense::DenseQuery)).
pub struct Added<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct AddedState {
    added: *const Tick,
    last_run: Tick,
}

unsafe impl Send for AddedState {}
unsafe impl Sync for AddedState {}

unsafe impl<T: Component> WorldQuery for Added<T> {
    type Item<'w> = ();
    type State = AddedState;
    type ReadOnly = Added<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }
    #[inline(always)]
    fn has_row_filter() -> bool {
        true
    }

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // role REQUIRED = 0
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        last_run: Tick,
        _: Tick,
    ) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        AddedState {
            added: col.added_ticks_ptr(),
            last_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        let tick = *state.added.add(row);
        if tick.is_newer_than(state.last_run) {
            Some(())
        } else {
            None
        }
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Added<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── Or<> — disjunction of filters (W2-5) ───────────────────────

/// Bevy-level disjunction of filters: a row passes if AT LEAST ONE branch
/// passes. The main consumer is `Or<(Changed<A>, Changed<B>)>` instead of two
/// queries + a dedup-set (the pattern of the engine's extract systems).
///
/// ```ignore
/// Query::<(Read<A>, Read<B>, Or<(Changed<A>, Changed<B>)>)>::new(&world)
///     .for_each(|e, (a, b, _)| { /* A OR B changed */ });
/// ```
///
/// Semantics:
/// - an archetype matches if at least one branch matches;
/// - a branch with an unregistered component simply does not match
///   (the others still work);
/// - branches are filters (`With`/`Without`/`Changed`/a nested `Or`/conjunction
///   tuples of them); a branch's item is ignored, so data shapes (`Read` etc.)
///   inside `Or` are allowed but pointless;
/// - `Or` does not narrow the query's candidates (`fill_required_ids` is empty):
///   a row may pass via any branch.
pub struct Or<T>(std::marker::PhantomData<T>);

macro_rules! impl_or_query {
    ( $( ($F:ident, $idx:tt) ),+ ) => {
        unsafe impl< $($F: WorldQuery),+ > WorldQuery for Or<( $($F,)+ )> {
            type Item<'w> = ();
            /// Per-arch state of a branch: `Some(state)` — the branch matches
            /// this archetype, `None` — the branch is dead (state is NOT fetched,
            /// otherwise UB on a missing column).
            type State = ( $(Option<$F::State>,)+ );
            type ReadOnly = Or<( $(<$F as WorldQuery>::ReadOnly,)+ )>;

            #[inline]
            fn component_count() -> usize { 0 $( + $F::component_count() )+ }
            #[inline]
            fn is_filter() -> bool { true }
            /// The disjunction is per-row if AT LEAST ONE branch is per-row: a
            /// row-filter branch (`Changed`/`Added`) may null out a row even in a
            /// matched archetype (when only it matched). Conservatively correct:
            /// at `false` every branch is infallible ⇒ `Or` is infallible.
            #[inline(always)]
            fn has_row_filter() -> bool { false $( || $F::has_row_filter() )+ }

            /// A branch with an unregistered component carries the INVALID
            /// sentinel (the `fill_ids` invariant) — a dead branch does not empty
            /// the query.
            fn fill_ids(world: &World, ids: &mut IdBuf) {
                $( $F::fill_ids(world, ids); )+
            }

            fn fill_positive_ids(world: &World, ids: &mut IdBuf) {
                Self::fill_ids(world, ids);
            }

            /// The disjunction does not narrow candidates: a row may pass via any
            /// branch, so NO component of Or is "required".
            fn fill_required_ids(_: &World, _: &mut IdBuf) {}

            fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
                key.push(KEY_OR_OPEN);
                $( $F::fill_cache_key(world, key); )+
                key.push(KEY_OR_CLOSE);
            }

            fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
                let mut offset = 0;
                $(
                    let n = $F::component_count();
                    let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                    if $F::matches_archetype(arch, slice) { return true; }
                    #[allow(unused_assignments)] { offset += n; }
                )+
                false
            }

            unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick, this_run: Tick) -> Self::State {
                let mut offset = 0;
                ($(
                    {
                        let n = $F::component_count();
                        let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                        let s = if $F::matches_archetype(arch, slice) {
                            Some($F::fetch_state(arch, slice, last_run, this_run))
                        } else {
                            None
                        };
                        #[allow(unused_assignments)] { offset += n; }
                        s
                    },
                )+)
            }

            #[inline(always)]
            unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
                $(
                    if let Some(s) = state.$idx {
                        if $F::fetch_item(s, row).is_some() {
                            return Some(());
                        }
                    }
                )+
                None
            }
        }

        impl< $($F: WorldQuery + WorldQuerySystemAccess + 'static),+ >
            WorldQuerySystemAccess for Or<( $($F,)+ )>
        {
            fn system_access() -> AccessDescriptor {
                AccessDescriptor::new()
                    $( .merge(&$F::system_access()) )+
            }
        }

        // SAFETY: `Or` yields `()`; it is read-only iff every branch is
        // read-only (branch states are still fetched, so a write branch
        // would create transient mutable state).
        unsafe impl< $($F: ReadOnlyWorldQuery),+ > ReadOnlyWorldQuery for Or<( $($F,)+ )> {}
    };
}

impl_or_query!((A, 0));
impl_or_query!((A, 0), (B, 1));
impl_or_query!((A, 0), (B, 1), (C, 2));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_or_query!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7)
);

// ── Tuple impls ────────────────────────────────────────────────

macro_rules! impl_world_query_tuple {
    ( $( ($Q:ident, $idx:tt) ),+ ) => {
        unsafe impl< $($Q: WorldQuery),+ > WorldQuery for ( $($Q,)+ ) {
            type Item<'w> = ( $($Q::Item<'w>,)+ );
            type State    = ( $($Q::State,)+ );
            type ReadOnly = ( $(<$Q as WorldQuery>::ReadOnly,)+ );

            #[inline]
            fn component_count() -> usize { 0 $( + $Q::component_count() )+ }
            /// A tuple is per-row if AT LEAST ONE element is per-row: the tuple's
            /// `fetch_item` returns `None` as soon as any element yields `None`.
            #[inline(always)]
            fn has_row_filter() -> bool { false $( || $Q::has_row_filter() )+ }

            fn fill_ids(world: &World, ids: &mut IdBuf) {
                $( $Q::fill_ids(world, ids); )+
            }

            fn fill_positive_ids(world: &World, ids: &mut IdBuf) {
                $( $Q::fill_positive_ids(world, ids); )+
            }

            fn fill_required_ids(world: &World, ids: &mut IdBuf) {
                $( $Q::fill_required_ids(world, ids); )+
            }

            fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
                $( $Q::fill_cache_key(world, key); )+
            }

            fn fill_data_access(world: &World, out: &mut smallvec::SmallVec<[(ComponentId, bool); 8]>) {
                $( $Q::fill_data_access(world, out); )+
            }

            fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
                let mut offset = 0;
                $(
                    let n = $Q::component_count();
                    let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                    if !$Q::matches_archetype(arch, slice) { return false; }
                    #[allow(unused_assignments)] { offset += n; }
                )+
                true
            }

            unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick, this_run: Tick) -> Self::State {
                let mut offset = 0;
                ($(
                    {
                        let n = $Q::component_count();
                        let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                        let s = $Q::fetch_state(arch, slice, last_run, this_run);
                        #[allow(unused_assignments)] { offset += n; }
                        s
                    },
                )+)
            }


            #[inline(always)]
            unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
                Some(( $( {
                    let item = $Q::fetch_item(state.$idx, row);
                    if item.is_none() { return None; }
                    item.unwrap_unchecked()
                }, )+ ))
            }

            /// Dense fetch without Option: every element is infallible (call only
            /// when `has_row_filter() == false`), so build the tuple directly via
            /// the elements' `fetch_item_unchecked` — not a single per-element
            /// Option check.
            #[inline(always)]
            unsafe fn fetch_item_unchecked<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
                ( $( $Q::fetch_item_unchecked(state.$idx, row), )+ )
            }
        }

        // WorldQuerySystemAccess for tuples
        impl< $($Q: WorldQuery + WorldQuerySystemAccess + 'static),+ >
            WorldQuerySystemAccess for ( $($Q,)+ )
        {
            fn system_access() -> AccessDescriptor {
                AccessDescriptor::new()
                    $( .merge(&$Q::system_access()) )+
            }
        }

        // SAFETY: a tuple is read-only iff every element is read-only.
        unsafe impl< $($Q: ReadOnlyWorldQuery),+ > ReadOnlyWorldQuery for ( $($Q,)+ ) {}
    };
}

// ── () — empty query (for AutoSystem without component access) ─

unsafe impl WorldQuery for () {
    type Item<'w> = ();
    type State = ();
    type ReadOnly = ();

    fn component_count() -> usize {
        0
    }

    fn fill_ids(_world: &World, _ids: &mut IdBuf) {}

    fn fill_cache_key(_world: &World, _key: &mut smallvec::SmallVec<[u64; 8]>) {}

    fn matches_archetype(_arch: &Archetype, _ids: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        _arch: &Archetype,
        _ids: &[ComponentId],
        _last_run: Tick,
        _this_run: Tick,
    ) -> Self::State {
    }

    unsafe fn fetch_item<'w>(_state: Self::State, _row: usize) -> Option<Self::Item<'w>> {
        Some(())
    }
}

impl WorldQuerySystemAccess for () {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new()
    }
}

impl_world_query_tuple!((A, 0));
impl_world_query_tuple!((A, 0), (B, 1));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_world_query_tuple!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7)
);

// ── ArchetypeFilter (D2-2) ─────────────────────────────────────

/// Marker "the filter is entirely archetype-level" — it does not look at rows.
/// Implemented for `()`, `With<T>`, `Without<T>` and their tuples. Required by
/// dense iteration (`for_each_chunk`): per-row filters (`Changed`/`Added`/`Or`
/// with them) are incompatible with slice-based yielding.
pub trait ArchetypeFilter: WorldQuery {}

impl ArchetypeFilter for () {}
impl<T: Component> ArchetypeFilter for With<T> {}
impl<T: Component> ArchetypeFilter for Without<T> {}

macro_rules! impl_archetype_filter_tuple {
    ( $($F:ident),+ ) => {
        impl< $($F: ArchetypeFilter),+ > ArchetypeFilter for ( $($F,)+ ) {}
    };
}
impl_archetype_filter_tuple!(A);
impl_archetype_filter_tuple!(A, B);
impl_archetype_filter_tuple!(A, B, C);
impl_archetype_filter_tuple!(A, B, C, D);
impl_archetype_filter_tuple!(A, B, C, D, E);
impl_archetype_filter_tuple!(A, B, C, D, E, F);
impl_archetype_filter_tuple!(A, B, C, D, E, F, G);
impl_archetype_filter_tuple!(A, B, C, D, E, F, G, H);

// ── Query<'w, 's, D, F = ()> — the single view (C6) ────────────
//
// One query type over a lazy per-archetype fetch machine: collapses the former
// `Query` (inline-owned), `CachedQuery` (Arc-cache) and the view half of
// `QueryState` (borrowed) into ONE type. The matched-archetype list comes from
// [`StateSrc`] ({Owned | Shared | Borrowed}); component data always lives in the
// archetype columns (their own allocations), so a single method set serves all
// three sources. The public signature carries TWO lifetime parameters (`'w` —
// the world borrow, `'s` — the state borrow), like Bevy `Query<'w, 's>`: today
// they coincide on every construction path, but they are split up front so the
// signature is stable before crates.io publication and forward-compatible with a
// future borrowed-`QueryState` fast path (`'s` outliving `'w`) without a major
// version bump.
//
// Read/write split (S1): the `&self` accessors (`iter`/`for_each`/`par_*`/
// `*_chunk`/`get`/`single`) require a read-only projection of the shape
// ([`ReadOnlyWorldQuery`]) — otherwise two scoped threads sharing `&q` (the type
// is `Sync`) could each obtain `&mut` to the same row (a data race from safe
// code). Write shapes go through the `&mut self` `*_mut` variants: the exclusive
// borrow proves the query is not shared.

/// Source of the matched-archetype list for [`Query`]:
/// - `Owned` — computed in place (ad-hoc `Query::new`), inline up to 8, no heap;
/// - `Shared` — from the global `QueryCache` (`World::query`), a shared `Arc`;
/// - `Borrowed` — borrowed from [`QueryState`]/`SubWorld` (per-system, zero-copy).
enum StateSrc<'s> {
    Owned(smallvec::SmallVec<[usize; 8]>),
    Shared(std::sync::Arc<[usize]>),
    Borrowed(&'s [usize]),
}

impl<'s> StateSrc<'s> {
    #[inline]
    fn as_slice(&self) -> &[usize] {
        match self {
            StateSrc::Owned(v) => v,
            StateSrc::Shared(a) => a,
            StateSrc::Borrowed(s) => s,
        }
    }
}

/// C2: reject a query shape that borrows the same component's data mutably
/// more than once (`(&mut T, &mut T)`) or both mutably and immutably
/// (`(&T, &mut T)`, `(Read<T>, Write<T>)`). Such a shape would hand out
/// aliasing references to one row on every iteration — undefined behavior
/// reachable from entirely safe code. Mirrors Bevy, which panics on the same
/// shapes. Runs once at construction over the (typically ≤ 8) declared
/// accesses, so the cost is negligible. Shared by every query construction path.
pub(crate) fn assert_no_self_alias<S: WorldQuery>(world: &World) {
    let mut access: smallvec::SmallVec<[(ComponentId, bool); 8]> = smallvec::SmallVec::new();
    S::fill_data_access(world, &mut access);
    for i in 0..access.len() {
        let (id_i, excl_i) = access[i];
        // Unregistered components collapse to INVALID and match no archetype,
        // so a duplicate INVALID can never actually alias — skip it.
        if id_i == ComponentId::INVALID {
            continue;
        }
        for &(id_j, excl_j) in &access[i + 1..] {
            if id_j == id_i && (excl_i || excl_j) {
                let name = world
                    .registry
                    .get_info(id_i)
                    .map(|info| info.name)
                    .unwrap_or("<component>");
                panic!(
                    "Query aliases component `{name}`: it is accessed mutably more than \
                     once (or both mutably and immutably) within a single query \
                     (e.g. `Query<(&mut {name}, &mut {name})>` or `(Read, Write)` of the \
                     same component). This would create aliasing references to one row. \
                     Access each component at most once per query, or split into separate \
                     queries."
                );
            }
        }
    }
}

/// A component query. The second parameter is the FILTER (Bevy form):
/// `Query<(&A, &mut B), (With<C>, Changed<A>)>` — data and filtering are split,
/// the filter's item is not yielded. `F` defaults to `()`; the single-tuple form
/// stays valid too (`Query<(Read<A>, With<C>)>` is equivalent).
///
/// The shape `(D, F)` runs in a single pass: `matches_archetype` is the AND of
/// both, and the filter's `fetch_item` is dropped on output (leaving `D::Item`).
pub struct Query<'w, 's, D: WorldQuery, F: WorldQuery = ()> {
    world: &'w World,
    arch: StateSrc<'s>,
    /// Component ids of the shape `(D, F)` in `fill_ids` order — inline up to 8,
    /// no heap.
    ids: smallvec::SmallVec<[ComponentId; 8]>,
    last_run: Tick,
    /// Row restrictions (SubWorld row-level splits): `(arch_idx, start, end)`.
    row_ranges: &'s [(usize, usize, usize)],
    /// `true` if every index in `arch` has ALREADY passed `matches_archetype`
    /// (`Owned`/`Shared`/`QueryState`: the list is an exact match). `from_sub_world`
    /// sets `false`: the scheduler's indices are a superset, so filter while
    /// iterating.
    match_verified: bool,
    _phantom: std::marker::PhantomData<fn() -> (D, F)>,
}

impl<'w, 's, D: WorldQuery, F: WorldQuery> Query<'w, 's, D, F> {
    // ── Constructors (ad-hoc owned; see also World::query / QueryState) ──

    /// Read-only ad-hoc query over a shared world borrow.
    ///
    /// Write shapes (`Write<T>`, `&mut T`, `MaybeWrite<T>`) do not satisfy
    /// [`ReadOnlyWorldQuery`]: construct those with [`Query::new_mut`] (the
    /// exclusive borrow proves no aliasing), or receive the query from the
    /// scheduler as a system parameter — cross-system exclusivity is validated
    /// there from the declared access.
    pub fn new(world: &'w World) -> Self
    where
        D: ReadOnlyWorldQuery,
        F: ReadOnlyWorldQuery,
    {
        Self::build_owned(world, Tick::ZERO)
    }

    /// [`Query::new`] with an explicit change-detection base tick.
    pub fn new_with_tick(world: &'w World, last_run: Tick) -> Self
    where
        D: ReadOnlyWorldQuery,
        F: ReadOnlyWorldQuery,
    {
        Self::build_owned(world, last_run)
    }

    /// Any-shape query (including writes) over an exclusive world borrow.
    /// The `&mut World` receiver proves no other world view is live, so the
    /// yielded `Mut<T>` items cannot alias anything.
    pub fn new_mut(world: &'w mut World) -> Self {
        Self::build_owned(world, Tick::ZERO)
    }

    /// [`Query::new_mut`] with an explicit change-detection base tick.
    pub fn new_mut_with_tick(world: &'w mut World, last_run: Tick) -> Self {
        Self::build_owned(world, last_run)
    }

    /// Any-shape query through the unsafe world escape.
    ///
    /// # Safety
    /// For the lifetime of the query the declared component access must not alias
    /// any other live access to the same world: components this shape writes are
    /// accessed by no other view, components it reads — by no mutable view. The
    /// scheduler upholds this for systems by validating the declared accesses of
    /// everything that runs concurrently.
    pub unsafe fn new_unchecked(world: UnsafeWorldCell<'w>) -> Self {
        Self::build_owned(world.world(), Tick::ZERO)
    }

    /// [`Query::new_unchecked`] with an explicit change-detection base tick.
    ///
    /// # Safety
    /// Same contract as [`Query::new_unchecked`].
    pub unsafe fn new_unchecked_with_tick(world: UnsafeWorldCell<'w>, last_run: Tick) -> Self {
        Self::build_owned(world.world(), last_run)
    }

    /// C2: see [`assert_no_self_alias`] — checks the whole `(D, F)` shape.
    #[inline]
    fn assert_no_self_alias(world: &World) {
        assert_no_self_alias::<(D, F)>(world);
    }

    /// Shared inline path (ad-hoc `Query::new*`): computes matching archetypes in
    /// place, without the global cache (no lock, no shared state — suitable for a
    /// read-only `&World`). Candidates are narrowed through `component_arch_index`
    /// only on a large world (a linear scan is cheaper on a small one).
    fn build_owned(world: &'w World, last_run: Tick) -> Self {
        Self::assert_no_self_alias(world);
        let mut ids = IdBuf::new();
        <(D, F)>::fill_ids(world, &mut ids);
        debug_assert_eq!(
            ids.len(),
            <(D, F)>::component_count(),
            "fill_ids invariant violated"
        );

        // Without semantics live entirely in matches_archetype (Without checks the
        // absence itself), so no separate exclude mask is needed (CR-M4).
        let arch_filter = |arch_idx: usize| -> bool {
            let arch = &world.archetypes[arch_idx];
            !arch.is_empty() && <(D, F)>::matches_archetype(arch, &ids)
        };

        // Linear-scan threshold: on a small world scanning every archetype is
        // cheaper than a component_arch_index hash-lookup per component.
        const LINEAR_SCAN_MAX_ARCHETYPES: usize = 128;

        let indices: smallvec::SmallVec<[usize; 8]> = {
            let mut required_ids = IdBuf::new();
            if world.archetypes.len() > LINEAR_SCAN_MAX_ARCHETYPES {
                <(D, F)>::fill_required_ids(world, &mut required_ids);
            }

            if required_ids.is_empty() {
                // Linear scan: the world is small OR the query has no required
                // components (Without-only / Maybe-only / empty).
                (0..world.archetypes.len())
                    .filter(|&arch_idx| arch_filter(arch_idx))
                    .collect()
            } else {
                // Candidates = archetypes of the rarest required component.
                let mut candidates: &[crate::archetype::ArchetypeId] = &[];
                let mut best = usize::MAX;
                for id in &required_ids {
                    let list = world
                        .component_arch_index
                        .get(id)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    if list.len() < best {
                        best = list.len();
                        candidates = list;
                    }
                }
                candidates
                    .iter()
                    .map(|id| id.0 as usize)
                    .filter(|&arch_idx| arch_filter(arch_idx))
                    .collect()
            }
        };

        Self {
            world,
            arch: StateSrc::Owned(indices),
            ids,
            last_run,
            row_ranges: &[],
            match_verified: true,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Cached path (`World::query`/`query_mut`): the archetype list comes from the
    /// incremental global [`QueryCache`] (a shared `Arc`). Single registry pass —
    /// the cache key carries both the ids and the shape roles.
    pub(crate) fn from_world_cached(world: &'w World, last_run: Tick) -> Self {
        Self::assert_no_self_alias(world);
        let mut key: smallvec::SmallVec<[u64; 8]> = smallvec::SmallVec::new();
        <(D, F)>::fill_cache_key(world, &mut key);
        let ids: smallvec::SmallVec<[ComponentId; 8]> = key
            .iter()
            .filter(|&&e| e & KEY_MARKER_BIT == 0)
            .map(|&e| ComponentId(e as u32))
            .collect();
        debug_assert_eq!(
            ids.len(),
            <(D, F)>::component_count(),
            "fill_cache_key invariant violated"
        );

        let arch_indices = world
            .query_cache
            .get_or_compute(&key, &world.archetypes, |arch| {
                <(D, F)>::matches_archetype(arch, &ids)
            });

        Self {
            world,
            arch: StateSrc::Shared(arch_indices),
            ids,
            last_run,
            row_ranges: &[],
            match_verified: true, // get_or_compute filtered by matches_archetype
            _phantom: std::marker::PhantomData,
        }
    }

    /// Query over the archetypes/rows of a SubWorld (the scheduler's transport).
    ///
    /// Zero-alloc: the archetype indices are BORROWED from the SubWorld
    /// (precomputed per system by the scheduler). The indices are a superset;
    /// filtering by `matches_archetype` happens while iterating
    /// (`match_verified == false`).
    ///
    /// # Safety
    /// The `SubWorld` must have been vended by the scheduler for a system whose
    /// declared access covers this query's shape, and no conflicting access may
    /// run concurrently (row ranges keep same-system splits disjoint).
    pub unsafe fn from_sub_world(sub: &'s SubWorld<'w>, last_run: Tick) -> Self {
        Self::assert_no_self_alias(sub.world());
        let mut ids = IdBuf::new();
        <(D, F)>::fill_ids(sub.world(), &mut ids);
        Self {
            world: sub.world(),
            arch: StateSrc::Borrowed(sub.archetype_indices()),
            ids,
            last_run,
            row_ranges: sub.row_ranges(),
            match_verified: false, // scheduler indices are a superset
            _phantom: std::marker::PhantomData,
        }
    }

    /// Internal constructor for [`QueryState`]: borrows the state's ready-made
    /// indices and ids — zero allocations, zero locks.
    pub(crate) fn from_state_parts(
        world: &'w World,
        arch_indices: &'s [usize],
        ids: &[ComponentId],
        last_run: Tick,
    ) -> Self {
        Self {
            world,
            arch: StateSrc::Borrowed(arch_indices),
            ids: ids.iter().copied().collect(),
            last_run,
            row_ranges: &[],
            match_verified: true, // QueryState.update filtered by matches_archetype
            _phantom: std::marker::PhantomData,
        }
    }

    #[inline]
    fn row_range(&self, arch_idx: usize) -> (usize, usize) {
        self.row_ranges
            .iter()
            .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
            .unwrap_or((0, usize::MAX))
    }

    // ── Iteration ─────────────────────────────────────────────────

    /// Lazy iterator (no read-only bound) over the matched archetypes, tied to the
    /// `&self` borrow. Private: the public [`iter`](Self::iter) (`&self`, read-only
    /// bound) / [`iter_mut`](Self::iter_mut) (`&mut self`, exclusive) add the
    /// aliasing discipline. Internal counters (`len`/`is_empty`) go through here.
    #[inline]
    fn iter_raw(&self) -> QueryIter<'_, D, F> {
        QueryIter {
            world: self.world,
            arch_indices: self.arch.as_slice(),
            ids: &self.ids,
            last_run: self.last_run,
            row_ranges: self.row_ranges,
            match_verified: self.match_verified,
            arch_pos: 0,
            row: 0,
            row_end: 0,
            state: None,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Read-only iterator over the query items (entity — via the shape
    /// `Query<(Entity, …)>`, as in Bevy).
    ///
    /// Requires a read-only shape: a shared `&self` iterator over a write shape
    /// would hand out `Mut<T>`, and because a query is `Sync` two scoped threads
    /// sharing `&q` could each obtain `&mut` to the same row — a data race from
    /// safe code (S1). Write shapes iterate via [`iter_mut`](Self::iter_mut)
    /// (`&mut self` proves the query is not shared).
    #[inline]
    pub fn iter(&self) -> QueryIter<'_, D, F>
    where
        D: ReadOnlyWorldQuery,
        F: ReadOnlyWorldQuery,
    {
        self.iter_raw()
    }

    /// Iterator yielding `Mut<T>` for write shapes. `&mut self` proves the query
    /// is not shared, so the yielded mutable items cannot alias across threads.
    #[inline]
    pub fn iter_mut(&mut self) -> QueryIter<'_, D, F> {
        self.iter_raw()
    }

    #[inline]
    fn get_impl(&self, entity: Entity) -> Option<D::Item<'_>> {
        let loc = self.world.entities.get_location(entity)?;
        let arch_idx = loc.archetype_id.as_usize();
        if !self.arch.as_slice().contains(&arch_idx) {
            return None;
        }
        let arch = &self.world.archetypes[arch_idx];
        if !self.match_verified && !<(D, F)>::matches_archetype(arch, &self.ids) {
            return None;
        }
        let row = loc.row as usize;
        let (r_start, r_end) = self.row_range(arch_idx);
        if row < r_start || row >= r_end.min(arch.len()) {
            return None;
        }
        let state =
            unsafe { <(D, F)>::fetch_state(arch, &self.ids, self.last_run, self.world.current_tick()) };
        unsafe { <(D, F)>::fetch_item(state, row) }.map(|(item, _)| item)
    }

    /// Item of a specific entity, if it matches the query (Bevy parity
    /// `Query::get`). Read-only shapes: item on `&self`. Write shapes —
    /// [`get_mut`](Self::get_mut): calling `q.get(e)` twice on a write shape would
    /// yield TWO `Mut<T>` to the same row (aliasing from safe code, S1).
    #[inline]
    pub fn get(&self, entity: Entity) -> Option<D::Item<'_>>
    where
        D: ReadOnlyWorldQuery,
    {
        self.get_impl(entity)
    }

    /// Item of a specific entity for mutable shapes. `&mut self` proves
    /// exclusivity — there cannot be a second live item on the same row.
    #[inline]
    pub fn get_mut(&mut self, entity: Entity) -> Option<D::Item<'_>> {
        self.get_impl(entity)
    }

    #[inline]
    fn single_impl(&self) -> Result<D::Item<'_>, QuerySingleError> {
        let mut it = self.iter_raw();
        let first = it.next().ok_or(QuerySingleError::NoEntities)?;
        if it.next().is_some() {
            return Err(QuerySingleError::MultipleEntities);
        }
        Ok(first)
    }

    /// Exactly one matching entity (Bevy parity). `Err(QuerySingleError)` on zero
    /// or several. Yields `D::Item`; if you need the entity, include it in the
    /// query: `Query<(Entity, &Hp)>`. Read-only shape (`&self`); write —
    /// [`single_mut`](Self::single_mut).
    #[inline]
    pub fn single(&self) -> Result<D::Item<'_>, QuerySingleError>
    where
        D: ReadOnlyWorldQuery,
    {
        self.single_impl()
    }

    /// [`single`](Self::single) for mutable shapes (`&mut self` is exclusive — no
    /// second `Mut` to the same row).
    #[inline]
    pub fn single_mut(&mut self) -> Result<D::Item<'_>, QuerySingleError> {
        self.single_impl()
    }

    /// Consuming form of [`single`](Self::single): the item lives for `'w` (the
    /// world borrow), not the `&self` borrow — needed by the `Single<D>` system
    /// parameter, which stores the extracted item in a field that outlives the
    /// local query.
    pub fn single_inner(self) -> Result<(Entity, D::Item<'w>), QuerySingleError> {
        let ids = &self.ids;
        let this_run = self.world.current_tick();
        let mut found: Option<(Entity, D::Item<'w>)> = None;
        for &arch_idx in self.arch.as_slice() {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() || (!self.match_verified && !<(D, F)>::matches_archetype(arch, ids)) {
                continue;
            }
            let (row_start, row_end) = self.row_range(arch_idx);
            let end = row_end.min(arch.len());
            if end <= row_start {
                continue;
            }
            // SAFETY: the state holds column pointers valid for the whole world
            // borrow 'w; `self` is consumed, so no further access through this
            // Query is possible (the same aliasing discipline as iter()).
            let state = unsafe { <(D, F)>::fetch_state(arch, ids, self.last_run, this_run) };
            let entities = &arch.entities[row_start..end];
            for (offset, &entity) in entities.iter().enumerate() {
                let row = row_start + offset;
                if let Some((item, _)) = unsafe { <(D, F)>::fetch_item(state, row) } {
                    if found.is_some() {
                        return Err(QuerySingleError::MultipleEntities);
                    }
                    found = Some((entity, item));
                }
            }
        }
        found.ok_or(QuerySingleError::NoEntities)
    }

    #[inline]
    fn for_each_raw<Func: FnMut(Entity, D::Item<'_>)>(&self, mut f: Func) {
        let ids = &self.ids;
        debug_assert_eq!(ids.len(), <(D, F)>::component_count(), "fill_ids invariant violated");
        let this_run = self.world.current_tick();
        for &arch_idx in self.arch.as_slice() {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() {
                continue;
            }
            if !self.match_verified && !<(D, F)>::matches_archetype(arch, ids) {
                continue;
            }
            let state = unsafe { <(D, F)>::fetch_state(arch, ids, self.last_run, this_run) };
            let (row_start, row_end) = self.row_range(arch_idx);
            let end = row_end.min(arch.len());
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let entities = &arch.entities[row_start..end];
            if <(D, F)>::has_row_filter() {
                // Row-level filter: load the entity LAZILY — only for passing rows.
                for offset in 0..len {
                    let row = row_start + offset;
                    if let Some((item, _)) = unsafe { <(D, F)>::fetch_item(state, row) } {
                        f(entities[offset], item);
                    }
                }
            } else {
                // Archetype-level shape: dense loop without the Option branch (§3.1A).
                for offset in 0..len {
                    let (item, _) =
                        unsafe { <(D, F)>::fetch_item_unchecked(state, row_start + offset) };
                    f(entities[offset], item);
                }
            }
        }
    }

    /// Read-only `for_each` — see [`iter`](Self::iter) for the read-only bound.
    /// Write shapes: [`for_each_mut`](Self::for_each_mut).
    #[inline]
    pub fn for_each<Func: FnMut(Entity, D::Item<'_>)>(&self, f: Func)
    where
        D: ReadOnlyWorldQuery,
        F: ReadOnlyWorldQuery,
    {
        self.for_each_raw(f);
    }

    /// `for_each` yielding `Mut<T>` for write shapes; `&mut self` proves
    /// exclusivity.
    #[inline]
    pub fn for_each_mut<Func: FnMut(Entity, D::Item<'_>)>(&mut self, f: Func) {
        self.for_each_raw(f);
    }

    fn par_for_each_raw<Func>(&self, f: Func)
    where
        D: Send,
        F: Send,
        Func: Fn(Entity, D::Item<'_>) + Send + Sync,
    {
        let ids = self.ids.clone();
        let world = self.world;
        let last_run = self.last_run;
        let this_run = world.current_tick();
        let num_threads = rayon::current_num_threads();
        let row_ranges = self.row_ranges;
        let match_verified = self.match_verified;
        let rr = |arch_idx: usize| -> (usize, usize) {
            row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX))
        };
        // Flat disjoint (arch, start, end) row ranges — one per matched archetype.
        let items: smallvec::SmallVec<[(usize, usize, usize); 32]> = self
            .arch
            .as_slice()
            .iter()
            .copied()
            .filter(|&a| !world.archetypes[a].is_empty())
            .filter(|&a| match_verified || <(D, F)>::matches_archetype(&world.archetypes[a], &ids))
            .filter_map(|a| {
                let (s, e) = rr(a);
                let start = s;
                let end = e.min(world.archetypes[a].len());
                if start >= end {
                    None
                } else {
                    Some((a, start, end))
                }
            })
            .collect();
        let total: usize = items.iter().map(|&(_, s, e)| e - s).sum();
        let threshold = crate::world::adaptive_chunk_size(total, num_threads, world.chunk_config());
        // SAFETY: each leaf gets a disjoint row range of a matched archetype, so
        // `&mut` access never aliases across the parallel `join`; `ids` is the
        // query id list.
        let process = |arch_idx: usize, start: usize, end: usize| {
            let arch = &world.archetypes[arch_idx];
            let state = unsafe { <(D, F)>::fetch_state(arch, &ids, last_run, this_run) };
            let entities = &arch.entities[start..end];
            if <(D, F)>::has_row_filter() {
                for (offset, &entity) in entities.iter().enumerate() {
                    if let Some((item, _)) = unsafe { <(D, F)>::fetch_item(state, start + offset) } {
                        f(entity, item);
                    }
                }
            } else {
                for (offset, &entity) in entities.iter().enumerate() {
                    let (item, _) =
                        unsafe { <(D, F)>::fetch_item_unchecked(state, start + offset) };
                    f(entity, item);
                }
            }
        };
        crate::par_utils::par_split_run_ranges(&items, threshold, &process);
    }

    /// Parallel iteration via an adaptive split (recursive `rayon::join` across
    /// archetypes). Order is nondeterministic. Read-only shape (see
    /// [`iter`](Self::iter)); write shapes — [`par_for_each_mut`](Self::par_for_each_mut).
    pub fn par_for_each<Func>(&self, f: Func)
    where
        D: ReadOnlyWorldQuery + Send,
        F: ReadOnlyWorldQuery + Send,
        Func: Fn(Entity, D::Item<'_>) + Send + Sync,
    {
        self.par_for_each_raw(f);
    }

    /// Parallel iteration with `Mut<T>` for write shapes; `&mut self` proves
    /// exclusivity (each leaf gets a disjoint row range).
    pub fn par_for_each_mut<Func>(&mut self, f: Func)
    where
        D: Send,
        F: Send,
        Func: Fn(Entity, D::Item<'_>) + Send + Sync,
    {
        self.par_for_each_raw(f);
    }

    /// Number of query matches.
    ///
    /// Fast path (match_verified, no row_ranges, no row-level filter) is exact by
    /// summing lengths; otherwise count the actual matches (`iter().count()`).
    pub fn len(&self) -> usize {
        if self.match_verified && self.row_ranges.is_empty() && !<(D, F)>::has_row_filter() {
            return self
                .arch
                .as_slice()
                .iter()
                .map(|&i| self.world.archetypes[i].len())
                .sum();
        }
        self.iter_raw().count()
    }

    pub fn is_empty(&self) -> bool {
        if self.match_verified && self.row_ranges.is_empty() && !<(D, F)>::has_row_filter() {
            return self
                .arch
                .as_slice()
                .iter()
                .all(|&i| self.world.archetypes[i].is_empty());
        }
        self.iter_raw().next().is_none()
    }

    fn for_each_chunk_raw<Func>(&self, mut f: Func)
    where
        D: crate::dense::DenseQuery,
        F: ArchetypeFilter,
        Func: FnMut(&[Entity], <D as crate::dense::DenseQuery>::Slices<'_>),
    {
        // The dense path relies on the ids of the DATA shape `D` (filters are
        // `ArchetypeFilter` and carry no data); `matches_archetype` needs the full
        // `(D, F)` ids, so compute both.
        let mut ids = IdBuf::new();
        D::fill_ids(self.world, &mut ids);
        let mut match_ids = IdBuf::new();
        <(D, F)>::fill_ids(self.world, &mut match_ids);
        let this_run = self.world.current_tick();
        for &arch_idx in self.arch.as_slice() {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() || (!self.match_verified && !<(D, F)>::matches_archetype(arch, &match_ids)) {
                continue;
            }
            let (row_start, row_end) = self.row_range(arch_idx);
            let end = row_end.min(arch.len());
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let slices = unsafe { D::fetch_slices(arch, &ids, row_start, len, this_run) };
            f(&arch.entities[row_start..end], slices);
        }
    }

    /// Dense (chunk) iteration: the callback receives the entities slice and whole
    /// archetype column SLICES — no per-row `fetch_item`. Available only to
    /// non-filtering shapes ([`DenseQuery`]); `Changed<T>` does not compile.
    /// Read-only shape; write — [`for_each_chunk_mut`](Self::for_each_chunk_mut).
    pub fn for_each_chunk<Func>(&self, f: Func)
    where
        D: crate::dense::DenseQuery + ReadOnlyWorldQuery,
        F: ArchetypeFilter,
        Func: FnMut(&[Entity], <D as crate::dense::DenseQuery>::Slices<'_>),
    {
        self.for_each_chunk_raw(f);
    }

    /// Dense iteration with mutable slices (`&mut [T]`) for write shapes;
    /// `&mut self` proves exclusivity.
    pub fn for_each_chunk_mut<Func>(&mut self, f: Func)
    where
        D: crate::dense::DenseQuery,
        F: ArchetypeFilter,
        Func: FnMut(&[Entity], <D as crate::dense::DenseQuery>::Slices<'_>),
    {
        self.for_each_chunk_raw(f);
    }

    fn par_for_each_chunk_raw<Func>(&self, f: Func)
    where
        D: crate::dense::DenseQuery + Send,
        F: ArchetypeFilter,
        Func: Fn(&[Entity], <D as crate::dense::DenseQuery>::Slices<'_>) + Send + Sync,
    {
        use rayon::prelude::*;
        let num_threads = rayon::current_num_threads();
        let mut ids = IdBuf::new();
        D::fill_ids(self.world, &mut ids);
        let mut match_ids = IdBuf::new();
        <(D, F)>::fill_ids(self.world, &mut match_ids);

        let world = self.world;
        let row_ranges = self.row_ranges;
        let match_verified = self.match_verified;
        let rr = |arch_idx: usize| -> (usize, usize) {
            row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX))
        };
        let chunks = compute_par_chunks(
            self.arch
                .as_slice()
                .iter()
                .copied()
                .filter(|&arch_idx| !world.archetypes[arch_idx].is_empty())
                .filter(|&arch_idx| {
                    match_verified || <(D, F)>::matches_archetype(&world.archetypes[arch_idx], &match_ids)
                })
                .map(|arch_idx| {
                    let s = rr(arch_idx);
                    let effective_len =
                        s.1.min(world.archetypes[arch_idx].len()).saturating_sub(s.0);
                    (arch_idx, effective_len)
                }),
            num_threads,
            world.chunk_config(),
        );

        let this_run = world.current_tick();
        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let (r_start, r_end) = rr(arch_idx);
            let clamped_start = r_start + start;
            let clamped_end = (r_start + end).min(r_end);
            if clamped_start >= clamped_end {
                return;
            }
            let arch = &world.archetypes[arch_idx];
            let len = clamped_end - clamped_start;
            let slices = unsafe { D::fetch_slices(arch, &ids, clamped_start, len, this_run) };
            f(&arch.entities[clamped_start..clamped_end], slices);
        });
    }

    /// Parallel dense iteration: the same chunk ranges as
    /// [`par_for_each`](Self::par_for_each), but the callback receives slices.
    pub fn par_for_each_chunk<Func>(&self, f: Func)
    where
        D: crate::dense::DenseQuery + ReadOnlyWorldQuery + Send,
        F: ArchetypeFilter,
        Func: Fn(&[Entity], <D as crate::dense::DenseQuery>::Slices<'_>) + Send + Sync,
    {
        self.par_for_each_chunk_raw(f);
    }

    /// Parallel dense iteration with mutable slices for write shapes; `&mut self`
    /// proves exclusivity.
    pub fn par_for_each_chunk_mut<Func>(&mut self, f: Func)
    where
        D: crate::dense::DenseQuery + Send,
        F: ArchetypeFilter,
        Func: Fn(&[Entity], <D as crate::dense::DenseQuery>::Slices<'_>) + Send + Sync,
    {
        self.par_for_each_chunk_raw(f);
    }
}

// ── Iterators ──────────────────────────────────────────────────

/// Lazy iterator for [`Query`]. `fetch_state` runs lazily — only when moving to a
/// new archetype. Tied to the `&self`/`&mut self` borrow (lifetime `'q`):
/// read-only on `&self`, write on `&mut self`.
pub struct QueryIter<'q, D: WorldQuery, F: WorldQuery = ()> {
    world: &'q World,
    arch_indices: &'q [usize],
    ids: &'q [ComponentId],
    last_run: Tick,
    row_ranges: &'q [(usize, usize, usize)],
    match_verified: bool,

    arch_pos: usize,
    row: usize,
    row_end: usize,
    state: Option<<(D, F) as WorldQuery>::State>,
    _phantom: std::marker::PhantomData<fn() -> (D, F)>,
}

impl<'q, D: WorldQuery, F: WorldQuery> Iterator for QueryIter<'q, D, F> {
    /// Iteration yields ONLY `D::Item` (Bevy 1:1); entity — via the query
    /// shape (`Query<(Entity, &A)>`).
    type Item = D::Item<'q>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            while self.row >= self.row_end {
                if !self.advance_archetype() {
                    return None;
                }
            }
            let row = self.row;
            self.row += 1;

            let state = *self.state.as_ref().unwrap();
            if <(D, F)>::has_row_filter() {
                if let Some((item, _)) = unsafe { <(D, F)>::fetch_item(state, row) } {
                    return Some(item);
                }
            } else {
                let (item, _) = unsafe { <(D, F)>::fetch_item_unchecked(state, row) };
                return Some(item);
            }
        }
    }
}

impl<'q, D: WorldQuery, F: WorldQuery> QueryIter<'q, D, F> {
    /// Set up the next non-empty matching archetype (from `arch_pos`).
    fn advance_archetype(&mut self) -> bool {
        while self.arch_pos < self.arch_indices.len() {
            let arch_idx = self.arch_indices[self.arch_pos];
            self.arch_pos += 1;

            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() {
                continue;
            }
            if !self.match_verified && !<(D, F)>::matches_archetype(arch, self.ids) {
                continue;
            }

            let (r_start, r_end) = self
                .row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX));
            let end = r_end.min(arch.len());
            if end <= r_start {
                continue;
            }

            self.state = Some(unsafe {
                <(D, F)>::fetch_state(arch, self.ids, self.last_run, self.world.current_tick())
            });
            self.row = r_start;
            self.row_end = end;
            return true;
        }
        false
    }
}

/// Exactly one query match (1:1 Bevy `Single`): a plain-fn system parameter;
/// the scheduler SKIPS the system in frames with 0 or >1 matches (skip semantics,
/// not a panic). `Option<Single<D, F>>` is `None` on zero matches, skipped only on
/// >1.
///
/// ```ignore
/// fn update(camera: Single<(&mut DistanceFog, &mut LocalTransform), With<Camera>>) {
///     let (mut fog, mut tf) = camera.into_inner();
/// }
/// ```
pub struct Single<'w, D: WorldQuery, F: WorldQuery = ()> {
    pub(crate) entity: Entity,
    pub(crate) item: D::Item<'w>,
    pub(crate) _filter: std::marker::PhantomData<F>,
}

impl<'w, D: WorldQuery, F: WorldQuery> Single<'w, D, F> {
    /// Entity of the single match.
    pub fn entity(&self) -> Entity {
        self.entity
    }

    /// Take the item (for tuple destructuring: `let (a, b) = s.into_inner()`).
    pub fn into_inner(self) -> D::Item<'w> {
        self.item
    }
}

impl<'w, D: WorldQuery, F: WorldQuery> std::ops::Deref for Single<'w, D, F> {
    type Target = D::Item<'w>;
    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl<'w, D: WorldQuery, F: WorldQuery> std::ops::DerefMut for Single<'w, D, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}

/// Error from [`Query::single`] (Bevy parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuerySingleError {
    NoEntities,
    MultipleEntities,
}

impl std::fmt::Display for QuerySingleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEntities => write!(f, "Query::single: no matching entity"),
            Self::MultipleEntities => write!(f, "Query::single: more than one matching entity"),
        }
    }
}

impl std::error::Error for QuerySingleError {}

/// `for item in &query` — Bevy 1:1: yields `D::Item`; entity — via the query
/// shape. Read-only shapes (like [`Query::iter`]); write — `&mut query`.
impl<'q, 'w, 's, D, F> IntoIterator for &'q Query<'w, 's, D, F>
where
    D: ReadOnlyWorldQuery,
    F: ReadOnlyWorldQuery,
{
    type Item = D::Item<'q>;
    type IntoIter = QueryIter<'q, D, F>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// `for mut item in &mut query` — write shapes yield `Mut<T>`; the exclusive
/// `&mut` borrow proves the query is not shared (S1).
impl<'q, 'w, 's, D: WorldQuery, F: WorldQuery> IntoIterator for &'q mut Query<'w, 's, D, F> {
    type Item = D::Item<'q>;
    type IntoIter = QueryIter<'q, D, F>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}


// ── Dynamic query (QueryBuilder / DynQuery / DynItem) ──────────
//
// Runtime-composed queries for consumers that don't know component types at
// compile time: the editor inspector, scripting bindings and agent IPC.
//
// The READ path ([`QueryBuilder`] → [`DynQuery`]) is safe over a shared
// `&World` — structural changes cannot happen while the borrow is live. The
// WRITE path ([`QueryBuilderMut`] → [`DynQueryMut`]) mirrors the typed
// borrow model (B1(v)): it is constructed from `&mut World`, so the exclusive
// borrow proves the yielded `&mut T` cannot alias anything.

/// Error produced by [`QueryBuilder::build`] / [`QueryBuilderMut::build`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynQueryError {
    /// A component name passed to `*_name` is not registered in the world's
    /// component registry. Names are matched against the full
    /// `std::any::type_name` of the component (the same resolution as
    /// [`World::component_id_by_name`]).
    UnknownComponent(String),
    /// The (read) builder contains `write`/`write_id`/`write_name` terms.
    /// Dynamic write access needs exclusive world access to be sound — use
    /// [`World::query_builder_mut`] instead of [`World::query_builder`].
    WriteNotSupported,
    /// A write builder requests mutable access to the same component id more
    /// than once (`(&mut T, &mut T)`), which would alias one row's data.
    /// Mirrors the typed query's self-alias rejection (C2).
    AliasedWrite(ComponentId),
}

impl std::fmt::Display for DynQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownComponent(name) => {
                write!(f, "dynamic query: unknown component name '{name}'")
            }
            Self::WriteNotSupported => write!(
                f,
                "dynamic query: write access requires exclusive world access; \
                 use World::query_builder_mut (not query_builder)"
            ),
            Self::AliasedWrite(id) => write!(
                f,
                "dynamic query: component id {id:?} is written more than once — \
                 that would alias one row's data (write each component at most once)"
            ),
        }
    }
}

impl std::error::Error for DynQueryError {}

/// One relation term of a dynamic query (S8). Relations are not
/// archetype-structural, so these are applied as a per-entity post-filter over
/// the archetype-matched set (see [`DynTerms::relations`]).
#[derive(Clone, Copy)]
struct RelationFilter {
    kind_idx: u32,
    /// `Some(target)` = relation to that exact target; `None` = any target.
    target: Option<Entity>,
    /// `true` = the entity must NOT have the relation (absence filter).
    negate: bool,
}

/// Resolved terms of a dynamic query — shared by the read and write builders.
#[derive(Default)]
struct DynTerms {
    reads: Vec<ComponentId>,
    writes: Vec<ComponentId>,
    withs: Vec<ComponentId>,
    excludes: Vec<ComponentId>,
    /// First unresolved component name — surfaced as an error at build time.
    unknown: Option<String>,
    /// Relation terms (S8) — a per-entity post-filter (relations live in the
    /// subject index, not the archetype).
    relations: Vec<RelationFilter>,
    /// A REQUIRED relation term whose kind was never registered can never match
    /// any entity ⇒ the whole query is empty. Set at builder time; makes
    /// [`matching_archetype_ids`](DynTerms::matching_archetype_ids) return none.
    relation_impossible: bool,
}

impl DynTerms {
    #[inline]
    fn id_of<T: Component>(world: &World) -> ComponentId {
        world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID)
    }

    #[inline]
    fn resolve_name(&mut self, world: &World, name: &str) -> Option<ComponentId> {
        let id = world.component_id_by_name(name);
        if id.is_none() && self.unknown.is_none() {
            self.unknown = Some(name.to_string());
        }
        id
    }

    /// An archetype matches iff it has every read/write/with component and
    /// none of the excludes.
    #[inline]
    fn matches_arch(&self, arch: &Archetype) -> bool {
        self.reads.iter().all(|id| arch.has_component(*id))
            && self.writes.iter().all(|id| arch.has_component(*id))
            && self.withs.iter().all(|id| arch.has_component(*id))
            && self.excludes.iter().all(|id| !arch.has_component(*id))
    }

    fn matching_archetype_ids(&self, world: &World) -> Vec<usize> {
        if self.unknown.is_some() || self.relation_impossible {
            return Vec::new();
        }
        world
            .archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| self.matches_arch(arch))
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether `entity` satisfies every relation term (per-entity post-filter,
    /// S8). Empty term list ⇒ trivially true (no per-entity cost).
    #[inline]
    fn entity_passes_relations(relations: &[RelationFilter], world: &World, entity: Entity) -> bool {
        relations.iter().all(|t| {
            world.subject_has_relation_idx(entity, t.kind_idx, t.target) != t.negate
        })
    }

    /// Record a relation term, resolving the kind read-only. An unregistered
    /// kind on a REQUIRED term makes the query empty; on an ABSENCE term it is
    /// trivially satisfied (dropped).
    fn push_relation<R: crate::relations::RelationKind>(
        &mut self,
        world: &World,
        target: Option<Entity>,
        negate: bool,
    ) {
        match world.relation_kind_idx::<R>() {
            Some(kind_idx) => self.relations.push(RelationFilter { kind_idx, target, negate }),
            None if !negate => self.relation_impossible = true,
            None => {}
        }
    }

    /// C2 for the dynamic write path: a write id must not repeat. Unregistered
    /// ids collapse to `INVALID` and match nothing, so a duplicate `INVALID`
    /// can never actually alias — skip it.
    fn check_write_alias(&self) -> Result<(), DynQueryError> {
        for i in 0..self.writes.len() {
            let id = self.writes[i];
            if id == ComponentId::INVALID {
                continue;
            }
            if self.writes[i + 1..].contains(&id) {
                return Err(DynQueryError::AliasedWrite(id));
            }
        }
        Ok(())
    }
}

/// Builder for a runtime-composed (dynamic) READ query.
///
/// Components can be selected statically (`read::<T>()`), by
/// [`ComponentId`] (`read_id`) or by full type name (`read_name`). Unknown
/// names are reported loudly by [`build`](Self::build); a *typed* term whose
/// component was never registered simply matches nothing (no entity can have
/// it) — this is encoded with the [`ComponentId::INVALID`] sentinel, the same
/// convention the typed query path uses.
///
/// For mutation use [`QueryBuilderMut`] ([`World::query_builder_mut`]).
pub struct QueryBuilder<'w> {
    world: &'w World,
    terms: DynTerms,
}

impl<'w> QueryBuilder<'w> {
    pub fn new(world: &'w World) -> Self {
        Self {
            world,
            terms: DynTerms::default(),
        }
    }

    /// Read access to `T`. An unregistered `T` makes the query match nothing
    /// (nothing can have a never-registered component).
    pub fn read<T: Component>(mut self) -> Self {
        self.terms.reads.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Read access to the component with the given runtime id.
    pub fn read_id(mut self, id: ComponentId) -> Self {
        self.terms.reads.push(id);
        self
    }

    /// Read access to the component with the given full type name.
    /// An unknown name is a loud [`DynQueryError::UnknownComponent`] at build.
    pub fn read_name(mut self, name: &str) -> Self {
        if let Some(id) = self.terms.resolve_name(self.world, name) {
            self.terms.reads.push(id);
        }
        self
    }

    /// Write access to `T`. Recorded for archetype matching, but
    /// [`build`](Self::build) rejects write terms — see
    /// [`DynQueryError::WriteNotSupported`]. Use [`QueryBuilderMut`] to mutate.
    pub fn write<T: Component>(mut self) -> Self {
        self.terms.writes.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Write access by runtime id (matching only; `build` rejects writes).
    pub fn write_id(mut self, id: ComponentId) -> Self {
        self.terms.writes.push(id);
        self
    }

    /// Presence filter: archetype must contain `T` (no data access).
    pub fn with<T: Component>(mut self) -> Self {
        self.terms.withs.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Presence filter by runtime id.
    pub fn with_id(mut self, id: ComponentId) -> Self {
        self.terms.withs.push(id);
        self
    }

    /// Presence filter by full type name (unknown name is loud at build).
    pub fn with_name(mut self, name: &str) -> Self {
        if let Some(id) = self.terms.resolve_name(self.world, name) {
            self.terms.withs.push(id);
        }
        self
    }

    /// Absence filter: archetype must NOT contain `T`. An unregistered `T` is
    /// vacuously absent, so the term is trivially satisfied.
    pub fn exclude<T: Component>(mut self) -> Self {
        self.terms.excludes.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Absence filter by runtime id.
    pub fn exclude_id(mut self, id: ComponentId) -> Self {
        self.terms.excludes.push(id);
        self
    }

    /// Absence filter by full type name (unknown name is loud at build).
    pub fn exclude_name(mut self, name: &str) -> Self {
        if let Some(id) = self.terms.resolve_name(self.world, name) {
            self.terms.excludes.push(id);
        }
        self
    }

    /// Relation term (S8): the entity must have relation `R` to `target`.
    ///
    /// Relations are matched per entity (they live in the subject index, not
    /// the archetype), so this is a post-filter over the archetype-matched set.
    /// A relation kind never used in this world matches nothing — the query is
    /// empty. This is a dynamic-query superpower our relations enable and a
    /// typed query cannot express at runtime (editor / scripting / IPC).
    pub fn with_relation<R: crate::relations::RelationKind>(mut self, _kind: R, target: Entity) -> Self {
        self.terms.push_relation::<R>(self.world, Some(target), false);
        self
    }

    /// Relation term: the entity must have relation `R` to ANY target
    /// (wildcard presence, e.g. "has a parent").
    pub fn with_any_relation<R: crate::relations::RelationKind>(mut self, _kind: R) -> Self {
        self.terms.push_relation::<R>(self.world, None, false);
        self
    }

    /// Absence relation term: the entity must NOT have relation `R` to `target`.
    pub fn without_relation<R: crate::relations::RelationKind>(mut self, _kind: R, target: Entity) -> Self {
        self.terms.push_relation::<R>(self.world, Some(target), true);
        self
    }

    /// Indices into `world.archetypes()` of the archetypes matching the
    /// builder's COMPONENT terms. Relation terms (a per-entity post-filter) are
    /// NOT applied here — iterate the built query for the full result. If a name
    /// failed to resolve the result is empty (loud error path is
    /// [`build`](Self::build)).
    pub fn matching_archetype_ids(&self) -> Vec<usize> {
        self.terms.matching_archetype_ids(self.world)
    }

    /// Build the read-only dynamic query.
    ///
    /// Errors loudly on unresolved component names and on write terms
    /// (§0.2a — no silent narrowing of what the caller asked for).
    pub fn build(self) -> Result<DynQuery<'w>, DynQueryError> {
        if let Some(name) = self.terms.unknown {
            return Err(DynQueryError::UnknownComponent(name));
        }
        if !self.terms.writes.is_empty() {
            return Err(DynQueryError::WriteNotSupported);
        }
        let arch_ids = self.terms.matching_archetype_ids(self.world);
        Ok(DynQuery {
            world: self.world,
            reads: self.terms.reads,
            arch_ids,
            relations: self.terms.relations,
        })
    }
}

/// Builder for a runtime-composed (dynamic) WRITE query.
///
/// Same term vocabulary as [`QueryBuilder`], but built from `&mut World` so
/// the resulting [`DynQueryMut`] can hand out `&mut T` soundly (B1(v)). Terms
/// use `&mut self` chaining. `write_*` terms request mutable access; the same
/// component may not be written twice ([`DynQueryError::AliasedWrite`]).
pub struct QueryBuilderMut<'w> {
    world: &'w mut World,
    terms: DynTerms,
}

impl<'w> QueryBuilderMut<'w> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            terms: DynTerms::default(),
        }
    }

    /// Read access to `T`.
    pub fn read<T: Component>(mut self) -> Self {
        self.terms.reads.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Read access by runtime id.
    pub fn read_id(mut self, id: ComponentId) -> Self {
        self.terms.reads.push(id);
        self
    }

    /// Read access by full type name (unknown name is loud at build).
    pub fn read_name(mut self, name: &str) -> Self {
        if let Some(id) = self.terms.resolve_name(self.world, name) {
            self.terms.reads.push(id);
        }
        self
    }

    /// Mutable access to `T`.
    pub fn write<T: Component>(mut self) -> Self {
        self.terms.writes.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Mutable access by runtime id.
    pub fn write_id(mut self, id: ComponentId) -> Self {
        self.terms.writes.push(id);
        self
    }

    /// Mutable access by full type name (unknown name is loud at build).
    pub fn write_name(mut self, name: &str) -> Self {
        if let Some(id) = self.terms.resolve_name(self.world, name) {
            self.terms.writes.push(id);
        }
        self
    }

    /// Presence filter: archetype must contain `T`.
    pub fn with<T: Component>(mut self) -> Self {
        self.terms.withs.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Presence filter by runtime id.
    pub fn with_id(mut self, id: ComponentId) -> Self {
        self.terms.withs.push(id);
        self
    }

    /// Presence filter by full type name.
    pub fn with_name(mut self, name: &str) -> Self {
        if let Some(id) = self.terms.resolve_name(self.world, name) {
            self.terms.withs.push(id);
        }
        self
    }

    /// Absence filter: archetype must NOT contain `T`.
    pub fn exclude<T: Component>(mut self) -> Self {
        self.terms.excludes.push(DynTerms::id_of::<T>(self.world));
        self
    }

    /// Absence filter by runtime id.
    pub fn exclude_id(mut self, id: ComponentId) -> Self {
        self.terms.excludes.push(id);
        self
    }

    /// Absence filter by full type name.
    pub fn exclude_name(mut self, name: &str) -> Self {
        if let Some(id) = self.terms.resolve_name(self.world, name) {
            self.terms.excludes.push(id);
        }
        self
    }

    /// Relation term (S8): the entity must have relation `R` to `target`. See
    /// [`QueryBuilder::with_relation`] — same per-entity semantics on the write
    /// path.
    pub fn with_relation<R: crate::relations::RelationKind>(mut self, _kind: R, target: Entity) -> Self {
        self.terms.push_relation::<R>(self.world, Some(target), false);
        self
    }

    /// Relation term: the entity must have relation `R` to ANY target.
    pub fn with_any_relation<R: crate::relations::RelationKind>(mut self, _kind: R) -> Self {
        self.terms.push_relation::<R>(self.world, None, false);
        self
    }

    /// Absence relation term: the entity must NOT have relation `R` to `target`.
    pub fn without_relation<R: crate::relations::RelationKind>(mut self, _kind: R, target: Entity) -> Self {
        self.terms.push_relation::<R>(self.world, Some(target), true);
        self
    }

    /// Build the read/write dynamic query.
    ///
    /// Errors loudly on unresolved names and on a component written twice
    /// (§0.2a / C2). Consumes the `&mut World` borrow into the query.
    pub fn build(self) -> Result<DynQueryMut<'w>, DynQueryError> {
        if let Some(name) = self.terms.unknown {
            return Err(DynQueryError::UnknownComponent(name));
        }
        self.terms.check_write_alias()?;
        let arch_ids = self.terms.matching_archetype_ids(self.world);
        Ok(DynQueryMut {
            world: self.world.as_unsafe_world_cell(),
            reads: self.terms.reads,
            writes: self.terms.writes,
            arch_ids,
            relations: self.terms.relations,
        })
    }
}

/// A built read-only dynamic query. Iterate with [`iter`](Self::iter) or do a
/// point lookup with [`get`](Self::get); items are untyped [`DynItem`]s.
pub struct DynQuery<'w> {
    world: &'w World,
    reads: Vec<ComponentId>,
    /// Matched archetype indices, ascending (see `matching_archetype_ids`).
    arch_ids: Vec<usize>,
    /// Relation terms (S8) — per-entity post-filter over the archetype set.
    relations: Vec<RelationFilter>,
}

impl<'w> DynQuery<'w> {
    /// Component ids the builder requested read access to, in call order.
    pub fn reads(&self) -> &[ComponentId] {
        &self.reads
    }

    /// Indices into `world.archetypes()` matched by this query (COMPONENT terms
    /// only; relation terms are a per-entity filter applied during iteration).
    pub fn archetype_ids(&self) -> &[usize] {
        &self.arch_ids
    }

    /// Number of matching entities. Without relation terms this is the sum of
    /// matched archetype lengths (O(archetypes)); with relation terms it counts
    /// the post-filtered entities (O(candidates)).
    pub fn count(&self) -> usize {
        if self.relations.is_empty() {
            let archs = self.world.archetypes();
            self.arch_ids.iter().map(|&i| archs[i].len()).sum()
        } else {
            self.iter().count()
        }
    }

    pub fn is_empty(&self) -> bool {
        if self.relations.is_empty() {
            self.arch_ids.iter().all(|&i| self.world.archetypes()[i].is_empty())
        } else {
            self.iter().next().is_none()
        }
    }

    /// Iterate all matching entities (archetype terms + relation post-filter).
    pub fn iter(&self) -> DynIter<'_, 'w> {
        DynIter {
            world: self.world,
            arch_ids: &self.arch_ids,
            relations: &self.relations,
            arch_cursor: 0,
            row: 0,
        }
    }

    /// Point lookup: the item for `entity`, or `None` if the entity is dead,
    /// its archetype does not match, or it fails a relation term.
    pub fn get(&self, entity: Entity) -> Option<DynItem<'w>> {
        let loc = self.world.entity_allocator().get_location(entity)?;
        let arch_idx = loc.archetype_id.as_usize();
        // arch_ids is ascending by construction.
        self.arch_ids.binary_search(&arch_idx).ok()?;
        if !DynTerms::entity_passes_relations(&self.relations, self.world, entity) {
            return None;
        }
        Some(DynItem {
            entity,
            arch: &self.world.archetypes()[arch_idx],
            row: loc.row as usize,
            registry: self.world.registry(),
        })
    }
}

impl std::fmt::Debug for DynQuery<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynQuery")
            .field("reads", &self.reads)
            .field("arch_ids", &self.arch_ids)
            .finish_non_exhaustive()
    }
}

impl<'q, 'w> IntoIterator for &'q DynQuery<'w> {
    type Item = DynItem<'w>;
    type IntoIter = DynIter<'q, 'w>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over the entities matched by a [`DynQuery`].
pub struct DynIter<'q, 'w> {
    world: &'w World,
    arch_ids: &'q [usize],
    /// Relation terms (S8) — entities failing them are skipped.
    relations: &'q [RelationFilter],
    arch_cursor: usize,
    row: usize,
}

impl<'q, 'w> Iterator for DynIter<'q, 'w> {
    type Item = DynItem<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let &arch_idx = self.arch_ids.get(self.arch_cursor)?;
            let arch = &self.world.archetypes()[arch_idx];
            if self.row < arch.len() {
                let row = self.row;
                self.row += 1;
                let entity = arch.entities()[row];
                // Relation post-filter (S8): skip entities that fail a term.
                if !DynTerms::entity_passes_relations(self.relations, self.world, entity) {
                    continue;
                }
                return Some(DynItem {
                    entity,
                    arch,
                    row,
                    registry: self.world.registry(),
                });
            }
            self.arch_cursor += 1;
            self.row = 0;
        }
    }
}

/// One entity yielded by a dynamic query: `{entity, archetype, row}` plus
/// untyped/typed component access.
pub struct DynItem<'w> {
    entity: Entity,
    arch: &'w Archetype,
    row: usize,
    registry: &'w crate::component::ComponentRegistry,
}

impl std::fmt::Debug for DynItem<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynItem")
            .field("entity", &self.entity)
            .field("row", &self.row)
            .finish_non_exhaustive()
    }
}

impl<'w> DynItem<'w> {
    #[inline]
    pub fn entity(&self) -> Entity {
        self.entity
    }

    #[inline]
    pub fn archetype(&self) -> &'w Archetype {
        self.arch
    }

    #[inline]
    pub fn row(&self) -> usize {
        self.row
    }

    /// Untyped pointer to this entity's component `id`, or `None` if the
    /// archetype has no such component.
    ///
    /// The pointer is valid while the underlying `&World` borrow is live (no
    /// structural change can occur) and must be interpreted as the component
    /// type registered under `id`. For zero-sized components the pointer is a
    /// dangling aligned sentinel and must not be read through.
    #[inline]
    pub fn get_ptr(&self, id: ComponentId) -> Option<*const u8> {
        let col_idx = self.arch.column_index(id)?;
        // SAFETY: `row < arch.len()` by construction (iterator bound /
        // entity location), and all columns share the archetype's length.
        Some(unsafe { self.arch.columns_raw()[col_idx].get_raw_ptr(self.row) })
    }

    /// Typed view of this entity's component `id`.
    ///
    /// Returns `None` if the archetype has no such component. A mismatch
    /// between `T` and the type registered under `id` is a caller bug and is
    /// reported loudly (throttled warn) before returning `None`.
    pub fn get<T: Component>(&self, id: ComponentId) -> Option<&'w T> {
        let col_idx = self.arch.column_index(id)?;
        let info = self.registry.get_info(id)?;
        if info.type_id != std::any::TypeId::of::<T>() {
            crate::warn_once!(
                "DynItem::get::<{}>: component id {:?} is registered as '{}' — type mismatch, returning None",
                std::any::type_name::<T>(),
                id,
                info.name,
            );
            return None;
        }
        // SAFETY: `row < arch.len()` by construction; the registry confirms
        // the column under `id` stores `T`; the shared `&World` borrow keeps
        // the storage alive and structurally unchanged for 'w.
        Some(unsafe { self.arch.columns_raw()[col_idx].get::<T>(self.row) })
    }
}

/// A built read/write dynamic query (see [`QueryBuilderMut`]). Holds the
/// world's exclusive access through an [`UnsafeWorldCell`]; iterate with
/// [`for_each_mut`](Self::for_each_mut) or point-look up with
/// [`get_mut`](Self::get_mut). Items are [`DynItemMut`]s and are vended one at
/// a time (a lending pattern), so mutable component access never aliases.
pub struct DynQueryMut<'w> {
    world: UnsafeWorldCell<'w>,
    reads: Vec<ComponentId>,
    writes: Vec<ComponentId>,
    /// Matched archetype indices, ascending.
    arch_ids: Vec<usize>,
    /// Relation terms (S8) — per-entity post-filter over the archetype set.
    relations: Vec<RelationFilter>,
}

impl std::fmt::Debug for DynQueryMut<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynQueryMut")
            .field("reads", &self.reads)
            .field("writes", &self.writes)
            .field("arch_ids", &self.arch_ids)
            .finish_non_exhaustive()
    }
}

impl<'w> DynQueryMut<'w> {
    /// Component ids requested for read access, in call order.
    pub fn reads(&self) -> &[ComponentId] {
        &self.reads
    }

    /// Component ids requested for mutable access, in call order.
    pub fn writes(&self) -> &[ComponentId] {
        &self.writes
    }

    /// Indices into `world.archetypes()` matched by this query.
    pub fn archetype_ids(&self) -> &[usize] {
        &self.arch_ids
    }

    /// Number of matching entities. With relation terms this counts the
    /// post-filtered set (O(candidates)); without, it sums archetype lengths.
    pub fn count(&self) -> usize {
        // SAFETY: read-only metadata view; no mutable view is live here.
        let world = unsafe { self.world.world() };
        if self.relations.is_empty() {
            let archs = world.archetypes();
            self.arch_ids.iter().map(|&i| archs[i].len()).sum()
        } else {
            let archs = world.archetypes();
            self.arch_ids
                .iter()
                .flat_map(|&i| archs[i].entities().iter().copied())
                .filter(|&e| DynTerms::entity_passes_relations(&self.relations, world, e))
                .count()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Visit every matching entity with a mutable item. The item is scoped to
    /// the call, so `&mut T` obtained from it can never alias another row.
    pub fn for_each_mut(&mut self, mut f: impl FnMut(DynItemMut<'_>)) {
        // SAFETY: `self` is borrowed exclusively for the call, and the
        // `UnsafeWorldCell` was created from `&mut World` — so this is the only
        // live view of the world. Items borrow it for the closure call only.
        let world = unsafe { self.world.world() };
        let this_run = world.current_tick();
        let registry = world.registry();
        let writes: &[ComponentId] = &self.writes;
        for &arch_idx in &self.arch_ids {
            let arch = &world.archetypes()[arch_idx];
            for row in 0..arch.len() {
                let entity = arch.entities()[row];
                // Relation post-filter (S8): skip entities that fail a term.
                if !DynTerms::entity_passes_relations(&self.relations, world, entity) {
                    continue;
                }
                f(DynItemMut {
                    entity,
                    arch,
                    row,
                    registry,
                    this_run,
                    writes,
                    world,
                });
            }
        }
    }

    /// Point lookup: a mutable item for `entity`, or `None` if the entity is
    /// dead, its archetype does not match, or it fails a relation term.
    pub fn get_mut(&mut self, entity: Entity) -> Option<DynItemMut<'_>> {
        // SAFETY: exclusive borrow of `self` + `&mut World`-derived cell ⇒ sole
        // live view; the returned item borrows it for its own lifetime only.
        let world = unsafe { self.world.world() };
        let loc = world.entity_allocator().get_location(entity)?;
        let arch_idx = loc.archetype_id.as_usize();
        self.arch_ids.binary_search(&arch_idx).ok()?;
        if !DynTerms::entity_passes_relations(&self.relations, world, entity) {
            return None;
        }
        Some(DynItemMut {
            entity,
            arch: &world.archetypes()[arch_idx],
            row: loc.row as usize,
            registry: world.registry(),
            this_run: world.current_tick(),
            writes: &self.writes,
            world,
        })
    }
}

/// One entity yielded by a dynamic WRITE query. Provides the same read access
/// as [`DynItem`] plus mutable access (`get_mut` / `get_mut_ptr`), which marks
/// the component changed. Mutable accessors take `&mut self`, so only one
/// `&mut T` is live at a time.
pub struct DynItemMut<'a> {
    entity: Entity,
    arch: &'a Archetype,
    row: usize,
    registry: &'a crate::component::ComponentRegistry,
    this_run: Tick,
    /// S7: the query's DECLARED write ids. `get_mut`/`get_mut_ptr` refuse any id
    /// not in this set, so the write declaration is enforced, not decorative.
    writes: &'a [ComponentId],
    /// For the S7 refusal path's `anomaly!` (the world's `ErrorHandler`).
    world: &'a World,
}

impl std::fmt::Debug for DynItemMut<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynItemMut")
            .field("entity", &self.entity)
            .field("row", &self.row)
            .finish_non_exhaustive()
    }
}

impl<'a> DynItemMut<'a> {
    #[inline]
    pub fn entity(&self) -> Entity {
        self.entity
    }

    #[inline]
    pub fn archetype(&self) -> &'a Archetype {
        self.arch
    }

    #[inline]
    pub fn row(&self) -> usize {
        self.row
    }

    /// Shared untyped pointer to component `id` (does not mark changed). See
    /// [`DynItem::get_ptr`] for the pointer-validity contract.
    #[inline]
    pub fn get_ptr(&self, id: ComponentId) -> Option<*const u8> {
        let col_idx = self.arch.column_index(id)?;
        // SAFETY: `row < arch.len()`; columns share the archetype's length.
        Some(unsafe { self.arch.columns_raw()[col_idx].get_raw_ptr(self.row) })
    }

    /// Shared typed view of component `id` (does not mark changed). Type
    /// mismatch is a loud (throttled) warn + `None`.
    pub fn get<T: Component>(&self, id: ComponentId) -> Option<&T> {
        let col_idx = self.arch.column_index(id)?;
        let info = self.registry.get_info(id)?;
        if info.type_id != std::any::TypeId::of::<T>() {
            crate::warn_once!(
                "DynItemMut::get::<{}>: component id {:?} is registered as '{}' — type mismatch, returning None",
                std::any::type_name::<T>(),
                id,
                info.name,
            );
            return None;
        }
        // SAFETY: registry confirms the column stores `T`; `row` in bounds.
        Some(unsafe { self.arch.columns_raw()[col_idx].get::<T>(self.row) })
    }

    /// Mutable untyped pointer to component `id`, marking it changed. `None`
    /// if the archetype lacks it. Borrows the item mutably, so at most one
    /// mutable accessor is live at a time. For ZSTs the pointer is a dangling
    /// aligned sentinel and must not be written through.
    #[inline]
    pub fn get_mut_ptr(&mut self, id: ComponentId) -> Option<*mut u8> {
        if !self.writes.contains(&id) {
            self.refuse_undeclared_write("DynItemMut::get_mut_ptr", id);
            return None;
        }
        let col_idx = self.arch.column_index(id)?;
        let col = &self.arch.columns_raw()[col_idx];
        // SAFETY: `row < arch.len()`; the query holds the world's exclusive
        // access, and `&mut self` means no other view of this row is live, so
        // both the change-tick write and the returned `*mut` are sound.
        unsafe {
            col.set_change_tick(self.row, self.this_run);
            Some(col.get_ptr(self.row))
        }
    }

    /// S7: report an attempt to mutate a component the query did not declare as a
    /// write. The declaration drives archetype matching AND the parallel access
    /// model, so an undeclared `&mut` (e.g. from scripting / agent-IPC over a
    /// dynamic query) would be a live-write the scheduler cannot see — refuse it
    /// loudly (§0.2a) rather than hand out the pointer.
    #[cold]
    fn refuse_undeclared_write(&self, op: &'static str, id: ComponentId) {
        crate::anomaly!(
            self.world,
            crate::Severity::Warn,
            op,
            Some(self.entity),
            self.registry.get_info(id).map(|i| i.name),
            "component id {id:?} is not in the query's declared writes \
             — mutation refused (declare it via write/write_id/write_name)"
        );
    }

    /// Mutable typed view of component `id`, marking it changed. Type mismatch
    /// is a loud (throttled) warn + `None`.
    pub fn get_mut<T: Component>(&mut self, id: ComponentId) -> Option<&mut T> {
        if !self.writes.contains(&id) {
            self.refuse_undeclared_write("DynItemMut::get_mut", id);
            return None;
        }
        let col_idx = self.arch.column_index(id)?;
        let info = self.registry.get_info(id)?;
        if info.type_id != std::any::TypeId::of::<T>() {
            crate::warn_once!(
                "DynItemMut::get_mut::<{}>: component id {:?} is registered as '{}' — type mismatch, returning None",
                std::any::type_name::<T>(),
                id,
                info.name,
            );
            return None;
        }
        let col = &self.arch.columns_raw()[col_idx];
        // SAFETY: registry confirms the column stores `T`; `row` in bounds;
        // `&mut self` + the query's exclusive world access ⇒ no other live
        // reference to this row.
        unsafe {
            col.set_change_tick(self.row, self.this_run);
            Some(&mut *(col.get_ptr(self.row) as *mut T))
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod query_filter_tests {
    use super::*;
    use crate::component::Component;
    use crate::world::World;

    #[derive(Debug, PartialEq)]
    struct Hp(u32);
    impl Component for Hp {}
    #[derive(Debug, PartialEq)]
    struct Mana(u32);
    impl Component for Mana {}
    struct Boss;
    impl Component for Boss {}

    /// Bevy form `Query<Data, Filter>`: the filter is not yielded.
    #[test]
    fn query_data_filter_form() {
        let mut world = World::new();
        let boss = world.spawn((Hp(100), Mana(50), Boss));
        let _mob = world.spawn((Hp(10), Mana(5)));

        // With a filter (With<Boss>,): the item is data only; entity — via an
        // explicit query shape (P1).
        let q = Query::<(Entity, Read<Hp>, Read<Mana>), (With<Boss>,)>::new(&world);
        let got: Vec<(Entity, &Hp, &Mana)> = q.iter().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, boss);
        assert_eq!(*got[0].1, Hp(100));
        drop(q);

        // A single filter without a tuple works too.
        let n = Query::<Read<Hp>, With<Boss>>::new(&world).iter().count();
        assert_eq!(n, 1);

        // Changed in the filter: after advance — empty, after mutation — 1 again.
        world.advance_change_tick();
        let lr = world.last_run_tick();
        let n = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 0);
        world.get_mut::<Hp>(boss).unwrap().0 += 1;
        let n = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 1);
    }

    /// `for item in &q` / `&mut q` — IntoIterator over iter() (D2-2/P1): the item
    /// without a forced entity (Bevy 1:1); entity — via the query shape.
    #[test]
    fn query_for_loop_iteration() {
        let mut world = World::new();
        world.spawn((Hp(1),));
        world.spawn((Hp(2),));

        let q = Query::<Read<Hp>>::new(&world);
        let mut sum = 0;
        for hp in &q {
            sum += hp.0;
        }
        assert_eq!(sum, 3);

        let mut q = Query::<Write<Hp>>::new_mut(&mut world);
        for mut hp in &mut q {
            hp.0 *= 10;
        }
        let total: u32 = Query::<Read<Hp>>::new(&world).iter().map(|hp| hp.0).sum();
        assert_eq!(total, 30);

        // Entity — via an explicit shape, as in Bevy:
        let q = Query::<(Entity, Read<Hp>)>::new(&world);
        let pairs: Vec<(Entity, &Hp)> = q.iter().collect();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|(e, _)| world.is_alive(*e)));
    }

    /// A2 regression: mutating through `Query<Write<T>>` stamps the row's
    /// change-tick via a `*mut Tick` whose provenance is the `TickCell`
    /// interior — a write through a *shared* `&Column`. Under the pre-fix
    /// `Vec<Tick>` layout that write was undefined behavior (Miri Tree Borrows
    /// rejects a write to non-interior-mutable memory reached from `&self`).
    /// Covers both the per-item hot path (`Mut::deref_mut` → `ticks_ptr`) and
    /// the dense chunk path (`stamp_range`); the `Changed<T>` asserts confirm
    /// the stamp actually lands. Run under
    /// `cargo +nightly miri test -Zmiri-tree-borrows` to validate soundness.
    #[test]
    fn a2_write_stamps_change_tick_soundly() {
        let mut world = World::new();
        let e = world.spawn((Hp(1),));
        let f = world.spawn((Hp(2),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        // Nothing changed since the advance.
        assert_eq!(
            Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr)
                .iter()
                .count(),
            0
        );

        // Per-item Write path: `Mut::deref_mut` writes the tick through the
        // shared `&Column` via `ticks_ptr()`.
        {
            let mut q = Query::<Write<Hp>>::new_mut(&mut world);
            for mut hp in &mut q {
                hp.0 += 100;
            }
        }
        assert_eq!(world.get::<Hp>(e), Some(&Hp(101)));
        assert_eq!(
            Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr)
                .iter()
                .count(),
            2
        );

        // Dense chunk path: `Write<T>::fetch_slices` calls `stamp_range` over
        // the same cell buffer.
        world.advance_change_tick();
        let lr2 = world.last_run_tick();
        {
            let mut q = Query::<Write<Hp>>::new_mut(&mut world);
            q.for_each_chunk_mut(|_entities, hps: &mut [Hp]| {
                for hp in hps {
                    hp.0 += 1;
                }
            });
        }
        assert_eq!(world.get::<Hp>(e), Some(&Hp(102)));
        assert_eq!(world.get::<Hp>(f), Some(&Hp(103)));
        assert_eq!(
            Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr2)
                .iter()
                .count(),
            2
        );
    }

    /// C2 regression: a query that borrows one component's data mutably more
    /// than once — or both mutably and immutably — panics at construction. Such
    /// a shape would hand out aliasing references to a single row on every
    /// iteration (safe-code UB). Distinct components, repeated shared reads, and
    /// filters (`With`/`Changed`) over a written component are all legal and must
    /// NOT panic.
    #[test]
    fn c2_rejects_self_aliasing_query_shapes() {
        let mut world = World::new();
        world.spawn((Hp(1), Mana(2)));

        // Aliasing shapes must panic.
        let ww = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Query::<(Write<Hp>, Write<Hp>)>::new_mut(&mut world);
        }));
        assert!(ww.is_err(), "write+write of same component must panic");

        let rw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Query::<(Read<Hp>, Write<Hp>)>::new_mut(&mut world);
        }));
        assert!(rw.is_err(), "read+write of same component must panic");

        let refmut = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Query::<(&mut Hp, &mut Hp)>::new_mut(&mut world);
        }));
        assert!(refmut.is_err(), "&mut + &mut of same component must panic");

        let maybe_alias = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Query::<(Write<Hp>, MaybeWrite<Hp>)>::new_mut(&mut world);
        }));
        assert!(maybe_alias.is_err(), "write + optional write of same component must panic");

        // Legal shapes must NOT panic (construction runs the check).
        let _ = Query::<(Read<Hp>, Read<Hp>)>::new(&world); // shared + shared
        let _ = Query::<(Write<Hp>, Write<Mana>)>::new_mut(&mut world); // distinct components
        let _ = Query::<(Write<Hp>,), (With<Hp>,)>::new_mut(&mut world); // filter over written comp
        let _ = Query::<Write<Hp>, Changed<Hp>>::new_mut(&mut world); // Changed filter is not a data borrow
        let _ = Query::<(Write<Hp>, Maybe<Mana>)>::new_mut(&mut world); // write + optional distinct
    }

    /// A12: `len`/`is_empty` must honor per-row filters (and row ranges), not
    /// just sum full archetype lengths. A `Changed<T>` query over unchanged rows
    /// has length 0, not the archetype size.
    #[test]
    fn a12_len_honors_row_filters() {
        let mut world = World::new();
        let e1 = world.spawn((Hp(1),));
        let _e2 = world.spawn((Hp(2),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        // Nothing changed since the advance → the Changed query is empty.
        let q = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr);
        assert_eq!(q.len(), 0, "no rows changed — len must be 0, not the archetype size");
        assert!(q.is_empty());
        drop(q);

        // Change exactly one row → len == 1.
        world.get_mut::<Hp>(e1).unwrap().0 += 10;
        let q = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr);
        assert_eq!(q.len(), 1);
        assert!(!q.is_empty());
        assert_eq!(q.iter().count(), 1, "len must agree with the actual iteration");
        drop(q);

        // Control: the unfiltered query still counts every row via the fast path.
        assert_eq!(Query::<Read<Hp>>::new(&world).len(), 2);
    }

    /// `single()` — Bevy parity: 0 → NoEntities, 1 → Ok, 2+ → MultipleEntities.
    #[test]
    fn query_single() {
        let mut world = World::new();
        assert_eq!(
            Query::<Read<Hp>>::new(&world).single().unwrap_err(),
            QuerySingleError::NoEntities
        );

        let e = world.spawn((Hp(42),));
        // Entity when needed — via the query shape (P1).
        let q = Query::<(Entity, Read<Hp>)>::new(&world);
        let (got_e, hp) = q.single().unwrap();
        assert_eq!((got_e, hp.0), (e, 42));
        drop(q);

        // single_mut: mutation through Mut<T>.
        let mut q = Query::<Write<Hp>>::new_mut(&mut world);
        let mut hp = q.single_mut().unwrap();
        hp.0 = 7;
        drop(q);
        assert_eq!(world.get::<Hp>(e), Some(&Hp(7)));

        world.spawn((Hp(1),));
        assert_eq!(
            Query::<Read<Hp>>::new(&world).single().unwrap_err(),
            QuerySingleError::MultipleEntities
        );
    }

    /// `get(entity)` — random access within a query (P3, Bevy parity):
    /// O(1) by location, filters (archetype- and per-row) are applied.
    #[test]
    fn query_get_by_entity() {
        let mut world = World::new();
        let boss = world.spawn((Hp(100), Boss));
        let mob = world.spawn((Hp(10),));

        let q = Query::<Read<Hp>, With<Boss>>::new(&world);
        assert_eq!(q.get(boss), Some(&Hp(100)));
        assert_eq!(q.get(mob), None, "does not match the With<Boss> filter");
        drop(q);

        // Per-row filter: Changed is applied in get() too.
        world.advance_change_tick();
        let lr = world.last_run_tick();
        world.get_mut::<Hp>(mob).unwrap().0 += 1;
        let q = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr);
        assert_eq!(q.get(mob), Some(&Hp(11)));
        assert_eq!(q.get(boss), None, "boss did not change");
        drop(q);

        // get_mut: mutation through Mut<T>.
        let mut q = Query::<Write<Hp>>::new_mut(&mut world);
        q.get_mut(boss).unwrap().0 = 1;
        drop(q);
        assert_eq!(world.get::<Hp>(boss), Some(&Hp(1)));
    }

    /// An archetype filter is compatible with dense iteration; data comes only
    /// from the filtered archetypes.
    #[test]
    fn query_filter_with_chunks() {
        let mut world = World::new();
        world.spawn((Hp(1), Boss));
        world.spawn((Hp(2),));

        let mut sum = 0;
        Query::<Read<Hp>, With<Boss>>::new(&world).for_each_chunk(|_, hp| {
            sum += hp.iter().map(|h| h.0).sum::<u32>();
        });
        assert_eq!(sum, 1, "for_each_chunk respects the archetype filter");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::World;

    use crate::component::Component;

    struct A;
    impl Component for A {}
    struct B;
    impl Component for B {}

    #[test]
    fn without_exclude_mask_works() {
        let mut world = World::new();

        // Spawn an entity with only A
        let e1 = world.spawn((A,));
        // Spawn an entity with A and B
        let _e2 = world.spawn((A, B));
        // Spawn an entity with only B
        let e3 = world.spawn((B,));

        // Query<Read<A>, Without<B>> must return only e1
        let query: Query<'_, '_, (Entity, Read<A>, Without<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _, _)| e).collect();
        assert_eq!(
            results,
            vec![e1],
            "Without<B> must exclude entities with B"
        );

        // Query<Read<B>, Without<A>> must return only e3
        let query: Query<'_, '_, (Entity, Read<B>, Without<A>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _, _)| e).collect();
        assert_eq!(
            results,
            vec![e3],
            "Without<A> must exclude entities with A"
        );

        // Query<Without<A>, Without<B>> — empty result (all have at least one component)
        let query: Query<'_, '_, (Without<A>, Without<B>)> = Query::new(&world);
        assert!(query.is_empty(), "Without A and B nothing should remain");
    }

    #[test]
    fn without_with_large_world() {
        let mut world = World::new();

        // Spawn many entities with (A) and many with (A, B)
        let mut only_a = Vec::new();
        let mut with_b = Vec::new();

        for _ in 0..50 {
            only_a.push(world.spawn((A,)));
        }
        for _ in 0..50 {
            with_b.push(world.spawn((A, B)));
        }

        // Query<Read<A>, Without<B>> — must get only entities with A and without B
        let query: Query<'_, '_, (Entity, Read<A>, Without<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _, _)| e).collect();

        assert_eq!(results.len(), 50, "There must be 50 entities with A and without B");
        for e in &results {
            assert!(only_a.contains(e), "The entity must be from only_a");
            assert!(!with_b.contains(e), "The entity must not be from with_b");
        }
    }

    #[derive(Debug)]
    struct Pos {
        x: f32,
    }
    impl Component for Pos {}

    /// C1: mutation through `Query<Write<T>>` must make `Changed<T>` reliable
    /// (previously the change-tick was set only in `World::get_mut`).
    #[test]
    fn write_query_marks_changed() {
        let mut world = World::new();
        let target = world.spawn((Pos { x: 0.0 },));
        let _other = world.spawn((Pos { x: 100.0 },));

        // Baseline: the tick against which we measure "changes".
        world.tick();
        let last_run = world.current_tick();
        world.tick();

        // Mutate ONLY target through Query<Write<Pos>>.
        {
            let mut q: Query<'_, '_, Write<Pos>> = Query::new_mut(&mut world);
            q.for_each_mut(|e, mut p| {
                if e == target {
                    p.x += 1.0;
                }
            });
        }

        // Changed<Pos> relative to last_run must return exactly target.
        let changed: Vec<_> =
            Query::<(Entity, crate::query::Changed<Pos>, Read<Pos>)>::new_with_tick(&world, last_run)
                .iter()
                .map(|(e, _, _)| e)
                .collect();
        assert_eq!(
            changed,
            vec![target],
            "mutation through Query<Write<T>> must mark Changed<T>"
        );
    }

    /// C3: Bevy-like `&T` / `&mut T` syntax in queries.
    #[test]
    fn bevy_ref_syntax_query() {
        let mut world = World::new();
        let e = world.spawn((Pos { x: 1.0 },));

        // Read through &Pos, mutate through &mut Pos — as in Bevy.
        Query::<(&Pos,)>::new(&world).for_each(|_, (p,)| {
            assert_eq!(p.x, 1.0);
        });
        Query::<&mut Pos>::new_mut(&mut world).for_each_mut(|_, mut p| {
            p.x += 10.0;
        });
        assert_eq!(world.get::<Pos>(e).unwrap().x, 11.0);

        // &mut stamps the change-tick (like Write) — Changed is reliable.
        world.tick();
        let lr = world.current_tick();
        world.tick();
        Query::<&mut Pos>::new_mut(&mut world).for_each_mut(|_, mut p| {
            p.x += 1.0;
        });
        let changed = Query::<crate::query::Changed<Pos>>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(changed, 1, "&mut T must mark Changed like Write<T>");
    }

    /// A pure read through `Write<T>` without `DerefMut` must NOT mark it changed
    /// (the stamp happens only on a mutable dereference).
    #[test]
    fn write_query_read_only_no_change() {
        let mut world = World::new();
        let _e = world.spawn((Pos { x: 5.0 },));

        world.tick();
        let last_run = world.current_tick();
        world.tick();

        {
            let mut q: Query<'_, '_, Write<Pos>> = Query::new_mut(&mut world);
            let mut sink = 0.0;
            q.for_each_mut(|_, p| {
                // Only Deref (read) — no DerefMut.
                sink += p.x;
            });
            std::hint::black_box(sink);
        }

        let changed_count = Query::<crate::query::Changed<Pos>>::new_with_tick(&world, last_run)
            .iter()
            .count();
        assert_eq!(changed_count, 0, "reading through Write<T> must not mark Changed");
    }

    #[test]
    fn without_alone_query() {
        let mut world = World::new();

        let _e1 = world.spawn((A,));
        let e2 = world.spawn((B,));
        let _e3 = world.spawn((A, B));

        // Pure Without<A> — all entities without A
        let query: Query<'_, '_, (Entity, Without<A>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _)| e).collect();
        assert_eq!(
            results,
            vec![e2],
            "Without<A> must return entities without A"
        );
    }

    #[test]
    fn maybe_optional_component_simple() {
        let mut world = World::new();
        // Every entity has both A and B — a single archetype
        world.spawn((A, B));
        world.spawn((A, B));

        let query: Query<'_, '_, (Read<A>, Maybe<B>)> = Query::new(&world);
        let count = query.iter().count();
        assert_eq!(count, 2, "There must be 2 entities with A");
    }

    #[test]
    fn maybe_optional_component_mixed_archetypes() {
        let mut world = World::new();
        // Two archetypes: [A,B] and [A]
        world.spawn((A, B));
        world.spawn((A,));
        world.spawn((B,));

        let query: Query<'_, '_, (Read<A>, Maybe<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(_, b)| b.is_some()).collect();

        assert_eq!(results.len(), 2, "There must be 2 entities with A");
        // e1 has A+B → b.is_some() == true
        // e2 has A → b.is_some() == false
        assert!(results[0] != results[1], "One must have B, the other not");
    }

    #[test]
    fn maybe_write_optional_mut() {
        let mut world = World::new();

        world.spawn((A, B));
        world.spawn((A,));

        // Optional write: those that have B — double them
        let mut query: Query<'_, '_, (Read<A>, MaybeWrite<B>)> = Query::new_mut(&mut world);
        let results: Vec<_> = query
            .iter_mut()
            .map(|(_, b_opt)| b_opt.is_some())
            .collect();

        assert_eq!(results.len(), 2);
        let has_b_count = results.iter().filter(|b| **b).count();
        assert_eq!(has_b_count, 1, "Only one entity has B");
    }

    #[test]
    fn maybe_with_unregistered_component() {
        let mut world = World::new();
        // Register only A — B is not used
        world.spawn((A,));

        // Query<Maybe<B>> — B was never registered
        let query: Query<'_, '_, Maybe<B>> = Query::new(&world);
        // Must return the entity, but B will be None
        let results: Vec<_> = query.iter().collect();
        assert_eq!(results.len(), 1, "The entity without B must be returned");
        assert!(results[0].is_none(), "B must be None");
    }

    /// W2 regression: (Maybe<X>, Read<A>) with an UNregistered X used to shift
    /// the ids — Read<A> read the wrong segment and the query was falsely empty.
    /// The INVALID sentinel preserves the alignment.
    #[test]
    fn maybe_unregistered_does_not_misalign_following_ids() {
        struct NeverRegistered;
        impl Component for NeverRegistered {}

        let mut world = World::new();
        world.spawn((A,));
        world.spawn((A, B));

        let query: Query<'_, '_, (Maybe<NeverRegistered>, Read<A>)> = Query::new(&world);
        assert_eq!(
            query.iter().count(),
            2,
            "an unregistered Maybe must not empty the query"
        );
    }

    // ── Or<> (W2-5) ────────────────────────────────────────────

    #[test]
    fn or_changed_matches_either_branch() {
        let mut world = World::new();
        let ea = world.spawn((Pos { x: 0.0 }, A));
        let eb = world.spawn((Pos { x: 0.0 }, B));
        let _ec = world.spawn((Pos { x: 0.0 },));

        #[derive(Debug)]
        struct Marker2(f32);
        impl Component for Marker2 {}
        let em = world.spawn((Pos { x: 0.0 }, Marker2(0.0)));

        world.tick();
        let last_run = world.current_tick();
        world.tick();

        // Mutate Pos on ea and Marker2 on em — catch Or<(Changed<Pos>, Changed<Marker2>)>.
        if let Some(mut p) = world.get_mut::<Pos>(ea) {
            p.x = 1.0;
        }
        if let Some(mut m) = world.get_mut::<Marker2>(em) {
            m.0 = 1.0;
        }

        let hits: Vec<_> = Query::<(
            Entity,
            Read<Pos>,
            Or<(Changed<Pos>, Changed<Marker2>)>,
        )>::new_with_tick(&world, last_run)
        .iter()
        .map(|(e, _, _)| e)
        .collect();

        assert!(hits.contains(&ea), "the Changed<Pos> branch");
        assert!(hits.contains(&em), "the Changed<Marker2> branch");
        assert!(!hits.contains(&eb), "B did not change");
        assert_eq!(hits.len(), 2);
    }

    /// A13: `World::get_mut` hands out a `Mut<T>` that stamps the change-tick
    /// LAZILY — read-only access does NOT mark the component `Changed`, only an
    /// actual mutation does. Before the fix `get_mut` stamped eagerly, so merely
    /// touching a component produced a false `Changed<T>`.
    #[test]
    fn a13_get_mut_is_lazy_about_change_detection() {
        let mut world = World::new();
        let e_read = world.spawn((Pos { x: 5.0 },));
        let e_write = world.spawn((Pos { x: 5.0 },));

        world.tick();
        let last_run = world.current_tick();
        world.tick();

        // Read-only: obtain the Mut and Deref-read it, but never DerefMut.
        if let Some(m) = world.get_mut::<Pos>(e_read) {
            assert_eq!(m.x, 5.0); // Deref (read) — must NOT mark Changed
        }
        // Mutating: DerefMut stamps the change-tick.
        if let Some(mut m) = world.get_mut::<Pos>(e_write) {
            m.x = 9.0;
        }

        let changed: Vec<_> = Query::<(Entity, Read<Pos>, Changed<Pos>)>::new_with_tick(
            &world, last_run,
        )
        .iter()
        .map(|(e, _, _)| e)
        .collect();

        assert!(
            changed.contains(&e_write),
            "a mutated component must be Changed"
        );
        assert!(
            !changed.contains(&e_read),
            "read-only get_mut must NOT mark Changed (A13)"
        );
    }

    #[test]
    fn or_with_matches_archetype_union() {
        let mut world = World::new();
        let ea = world.spawn((Pos { x: 0.0 }, A));
        let eb = world.spawn((Pos { x: 0.0 }, B));
        let none = world.spawn((Pos { x: 0.0 },));

        let hits: Vec<_> = Query::<(Entity, Read<Pos>, Or<(With<A>, With<B>)>)>::new(&world)
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert!(hits.contains(&ea) && hits.contains(&eb));
        assert!(!hits.contains(&none));
    }

    /// An Or branch with an unregistered component is dead, but does NOT empty
    /// the query — the other branch still works (and does not crash on
    /// fetch_state).
    #[test]
    fn or_with_unregistered_branch_is_dead_not_fatal() {
        struct NeverRegistered;
        impl Component for NeverRegistered {}

        let mut world = World::new();
        let ea = world.spawn((Pos { x: 0.0 }, A));
        let _e = world.spawn((Pos { x: 0.0 },));

        let hits: Vec<_> =
            Query::<(Entity, Read<Pos>, Or<(With<NeverRegistered>, With<A>)>)>::new(&world)
                .iter()
                .map(|(e, _, _)| e)
                .collect();
        assert_eq!(hits, vec![ea]);
    }

    /// Or in CachedQuery: `(Or<(With<A>,)>, With<B>)` and `Or<(With<A>, With<B>)>`
    /// have the same ids but different semantics — the group markers in the cache
    /// key must separate them into different entries.
    #[test]
    fn or_cache_key_distinguishes_grouping() {
        let mut world = World::new();
        let _only_a = world.spawn((A,));
        let both = world.spawn((A, B));

        // (Or<(With<A>,)>, With<B>) ≡ With<A> AND With<B> → only both
        let strict: Vec<_> = world
            .query::<(Entity, Or<(With<A>,)>, With<B>)>()
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert_eq!(strict, vec![both]);

        // Or<(With<A>, With<B>)> → both entities
        let union_count = world.query::<Or<(With<A>, With<B>)>>().iter().count();
        assert_eq!(union_count, 2);
    }

    #[test]
    fn maybe_tuple_integration() {
        let mut world = World::new();

        #[derive(Debug, PartialEq)]
        struct C(u32);
        impl Component for C {}
        #[derive(Debug, PartialEq)]
        struct D(u32);
        impl Component for D {}

        let _e1 = world.spawn((A, C(1)));
        let _e2 = world.spawn((B, D(2)));

        // (Maybe<A>, Maybe<C>) — every entity
        let query: Query<'_, '_, (Maybe<A>, Maybe<C>)> = Query::new(&world);
        let results: Vec<_> = query
            .iter()
            .map(|(a, c)| (a.is_some(), c.is_some()))
            .collect();

        // There must be 2 entities
        assert_eq!(results.len(), 2);
        // e1: A=Some, C=Some
        assert!(results[0].0 && results[0].1);
        // e2: A=None, C=None
        assert!(!results[1].0 && !results[1].1);
    }
}

// ── Dynamic query tests ────────────────────────────────────────

#[cfg(test)]
mod dyn_query_tests {
    use super::*;
    use crate::component::Component;
    use crate::world::World;

    #[derive(Debug, PartialEq)]
    struct Hp(u32);
    impl Component for Hp {}
    #[derive(Debug, PartialEq)]
    struct Mana(u32);
    impl Component for Mana {}
    struct Boss;
    impl Component for Boss {}
    /// Never spawned/registered in these tests.
    struct Ghost;
    impl Component for Ghost {}

    fn type_name<T>() -> &'static str {
        std::any::type_name::<T>()
    }

    #[test]
    fn dyn_read_by_name_iter_typed_and_untyped() {
        let mut world = World::new();
        let a = world.spawn((Hp(100), Mana(50)));
        let b = world.spawn((Hp(10),));
        let _c = world.spawn((Mana(5),));

        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
        let q = world
            .query_builder()
            .read_name(type_name::<Hp>())
            .build()
            .unwrap();
        assert_eq!(q.reads(), &[hp_id]);
        assert_eq!(q.count(), 2);

        let mut seen: Vec<(Entity, u32)> = q
            .iter()
            .map(|item| (item.entity(), item.get::<Hp>(hp_id).unwrap().0))
            .collect();
        seen.sort_by_key(|(_, hp)| *hp);
        assert_eq!(seen, vec![(b, 10), (a, 100)]);

        // Untyped pointer access sees the same bytes.
        let item = q.get(a).unwrap();
        let ptr = item.get_ptr(hp_id).unwrap();
        // SAFETY: hp_id is registered as Hp; the shared &World borrow is live.
        let via_ptr = unsafe { &*(ptr as *const Hp) };
        assert_eq!(via_ptr, &Hp(100));
    }

    #[test]
    fn dyn_unknown_name_is_loud() {
        let world = World::new();
        let err = world
            .query_builder()
            .read_name("does::not::Exist")
            .build()
            .unwrap_err();
        assert_eq!(err, DynQueryError::UnknownComponent("does::not::Exist".into()));
        // The non-Result matching path degrades to "matches nothing".
        assert!(world
            .query_builder()
            .read_name("does::not::Exist")
            .matching_archetype_ids()
            .is_empty());
    }

    /// Regression: an unregistered *typed* term used to be silently DROPPED,
    /// making the query match MORE than requested. It must match nothing.
    #[test]
    fn dyn_unregistered_typed_read_matches_nothing() {
        let mut world = World::new();
        world.spawn((Hp(1),));

        let q = world.query_builder().read::<Hp>().read::<Ghost>();
        assert!(q.matching_archetype_ids().is_empty());
        assert_eq!(q.build().unwrap().count(), 0);

        // Excluding an unregistered component is vacuously satisfied.
        let q = world.query_builder().read::<Hp>().exclude::<Ghost>();
        assert_eq!(q.build().unwrap().count(), 1);
    }

    #[test]
    fn dyn_with_and_exclude_filters() {
        let mut world = World::new();
        let boss = world.spawn((Hp(100), Mana(50), Boss));
        let mob = world.spawn((Hp(10), Mana(5)));

        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();

        let q = world
            .query_builder()
            .read_id(hp_id)
            .with::<Boss>()
            .build()
            .unwrap();
        let seen: Vec<Entity> = q.iter().map(|i| i.entity()).collect();
        assert_eq!(seen, vec![boss]);

        let q = world
            .query_builder()
            .read_id(hp_id)
            .exclude::<Boss>()
            .build()
            .unwrap();
        let seen: Vec<Entity> = q.iter().map(|i| i.entity()).collect();
        assert_eq!(seen, vec![mob]);
    }

    #[test]
    fn dyn_point_get_respects_filter_and_liveness() {
        let mut world = World::new();
        let boss = world.spawn((Hp(100), Boss));
        let mob = world.spawn((Hp(10),));
        let dead = world.spawn((Hp(1), Boss));
        world.despawn(dead);

        let q = world
            .query_builder()
            .read::<Hp>()
            .with::<Boss>()
            .build()
            .unwrap();
        assert!(q.get(boss).is_some());
        assert!(q.get(mob).is_none()); // archetype not matched
        assert!(q.get(dead).is_none()); // dead entity

        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
        assert_eq!(q.get(boss).unwrap().get::<Hp>(hp_id).unwrap().0, 100);
    }

    #[test]
    fn dyn_write_build_is_rejected() {
        let mut world = World::new();
        world.spawn((Hp(1),));
        let err = world.query_builder().write::<Hp>().build().unwrap_err();
        assert_eq!(err, DynQueryError::WriteNotSupported);
        // Matching-only usage keeps working for write terms.
        assert_eq!(
            world.query_builder().write::<Hp>().matching_archetype_ids().len(),
            1
        );
    }

    #[test]
    fn dyn_typed_get_type_mismatch_returns_none() {
        let mut world = World::new();
        let e = world.spawn((Hp(1), Mana(2)));
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
        let mana_id = world.component_id_by_name(type_name::<Mana>()).unwrap();

        let q = world.query_builder().read_id(hp_id).build().unwrap();
        let item = q.get(e).unwrap();
        // Wrong T for the id — loud warn + None, never a reinterpreted value.
        assert!(item.get::<Mana>(hp_id).is_none());
        // Component present on the archetype but not requested: still readable
        // (read path over shared &World — access is per-archetype, like Bevy's
        // FilteredEntityRef it is bounded by what the entity actually has).
        assert_eq!(item.get::<Mana>(mana_id).unwrap().0, 2);
        // Component id the archetype lacks → None.
        let boss_arch_missing = world.query_builder().read_id(hp_id).build().unwrap();
        let item = boss_arch_missing.get(e).unwrap();
        assert!(item.get_ptr(ComponentId::INVALID).is_none());
    }

    #[test]
    fn dyn_zst_component_typed_get() {
        let mut world = World::new();
        let e = world.spawn((Hp(1), Boss));
        let boss_id = world.component_id_by_name(type_name::<Boss>()).unwrap();

        let q = world.query_builder().with_id(boss_id).read::<Hp>().build().unwrap();
        let item = q.get(e).unwrap();
        // ZST: typed get works (dangling aligned pointer is fine for a ZST ref).
        assert!(item.get::<Boss>(boss_id).is_some());
        // Untyped pointer exists but is a sentinel — callers must not read it.
        assert!(item.get_ptr(boss_id).is_some());
    }

    #[test]
    fn dyn_iter_spans_multiple_archetypes() {
        let mut world = World::new();
        world.spawn((Hp(1),));
        world.spawn((Hp(2), Mana(1)));
        world.spawn((Hp(3), Boss));
        world.spawn((Mana(9),));

        let q = world.query_builder().read::<Hp>().build().unwrap();
        assert!(q.archetype_ids().len() >= 3);
        assert_eq!(q.count(), 3);
        let sum: u32 = (&q)
            .into_iter()
            .map(|i| {
                let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
                i.get::<Hp>(hp_id).unwrap().0
            })
            .sum();
        assert_eq!(sum, 6);
        assert!(!q.is_empty());
    }

    /// S8: relation terms on the dynamic query — a per-entity post-filter that a
    /// typed query cannot express at runtime.
    #[test]
    fn dyn_query_relation_terms() {
        use crate::relations::{ChildOf, RelationKind};

        let mut world = World::new();
        let p1 = world.spawn((Hp(1),));
        let p2 = world.spawn((Hp(2),));
        let a = world.spawn((Hp(10),));
        let b = world.spawn((Hp(11),));
        let c = world.spawn((Hp(12),));
        let d = world.spawn((Hp(13),)); // no parent
        world.add_relation(a, ChildOf, p1);
        world.add_relation(b, ChildOf, p1);
        world.add_relation(c, ChildOf, p2);

        // with_relation: Hp entities that are ChildOf p1 → {a, b}.
        let q = world
            .query_builder()
            .read::<Hp>()
            .with_relation(ChildOf, p1)
            .build()
            .unwrap();
        let ids: std::collections::HashSet<_> = q.iter().map(|i| i.entity()).collect();
        assert_eq!(ids, std::collections::HashSet::from([a, b]));
        assert_eq!(q.count(), 2);
        assert!(!q.is_empty());

        // with_any_relation: Hp entities with ANY ChildOf → {a, b, c}.
        let q = world
            .query_builder()
            .read::<Hp>()
            .with_any_relation(ChildOf)
            .build()
            .unwrap();
        assert_eq!(q.count(), 3);

        // without_relation: Hp entities NOT ChildOf p1 → p1, p2, c, d.
        let q = world
            .query_builder()
            .read::<Hp>()
            .without_relation(ChildOf, p1)
            .build()
            .unwrap();
        let got: std::collections::HashSet<_> = q.iter().map(|i| i.entity()).collect();
        assert!(!got.contains(&a) && !got.contains(&b));
        assert!(got.contains(&c) && got.contains(&d) && got.contains(&p1) && got.contains(&p2));

        // get(): point lookup honors the relation term.
        let q = world
            .query_builder()
            .read::<Hp>()
            .with_relation(ChildOf, p1)
            .build()
            .unwrap();
        assert!(q.get(a).is_some());
        assert!(q.get(c).is_none(), "c is ChildOf p2, not p1");

        // A never-used relation kind: a REQUIRED term matches nothing…
        #[derive(Clone, Copy)]
        struct Likes;
        impl RelationKind for Likes {}
        let q = world
            .query_builder()
            .read::<Hp>()
            .with_any_relation(Likes)
            .build()
            .unwrap();
        assert_eq!(q.count(), 0, "never-used kind matches nothing");
        // …while an ABSENCE term is trivially satisfied (all 6 Hp entities).
        let q = world
            .query_builder()
            .read::<Hp>()
            .without_relation(Likes, p1)
            .build()
            .unwrap();
        assert_eq!(q.count(), 6);

        // Write path: with_relation filters `for_each_mut`.
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
        let mut q = world
            .query_builder_mut()
            .write::<Hp>()
            .with_relation(ChildOf, p1)
            .build()
            .unwrap();
        assert_eq!(q.count(), 2);
        q.for_each_mut(|mut item| {
            item.get_mut::<Hp>(hp_id).unwrap().0 += 100;
        });
        assert_eq!(world.get::<Hp>(a), Some(&Hp(110)));
        assert_eq!(world.get::<Hp>(b), Some(&Hp(111)));
        assert_eq!(world.get::<Hp>(c), Some(&Hp(12)), "c untouched (ChildOf p2)");
    }

    // ── Dynamic WRITE path ──────────────────────────────────────

    #[test]
    fn dyn_write_for_each_mut_typed_and_untyped() {
        let mut world = World::new();
        let a = world.spawn((Hp(100), Mana(50)));
        let b = world.spawn((Hp(10),));
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();

        let mut q = world
            .query_builder_mut()
            .write_name(type_name::<Hp>())
            .build()
            .unwrap();
        assert_eq!(q.writes(), &[hp_id]);
        assert_eq!(q.count(), 2);

        // Typed mutation.
        q.for_each_mut(|mut item| {
            item.get_mut::<Hp>(hp_id).unwrap().0 += 1;
        });
        assert_eq!(world.get::<Hp>(a), Some(&Hp(101)));
        assert_eq!(world.get::<Hp>(b), Some(&Hp(11)));

        // Untyped mutation through the raw pointer.
        let mut q = world.query_builder_mut().write_id(hp_id).build().unwrap();
        q.for_each_mut(|mut item| {
            let ptr = item.get_mut_ptr(hp_id).unwrap();
            // SAFETY: hp_id is registered as Hp; exclusive access via the query.
            unsafe {
                (*(ptr as *mut Hp)).0 += 1000;
            }
        });
        assert_eq!(world.get::<Hp>(a), Some(&Hp(1101)));
    }

    /// S7: a mutable accessor refuses any component the query did NOT declare as
    /// a write — the write declaration is enforced, not decorative. A
    /// read-declared component cannot be obtained mutably (typed or untyped),
    /// while shared read of it still works.
    #[test]
    fn dyn_write_gate_refuses_undeclared_write() {
        let mut world = World::new();
        let a = world.spawn((Hp(100), Mana(50)));
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
        let mana_id = world.component_id_by_name(type_name::<Mana>()).unwrap();

        let mut q = world
            .query_builder_mut()
            .write::<Hp>()
            .read::<Mana>()
            .build()
            .unwrap();

        q.for_each_mut(|mut item| {
            // Declared write: allowed.
            item.get_mut::<Hp>(hp_id).unwrap().0 += 1;
            // Read-declared (not a write): refused — typed and untyped.
            assert!(
                item.get_mut::<Mana>(mana_id).is_none(),
                "read-declared Mana must not be writable"
            );
            assert!(
                item.get_mut_ptr(mana_id).is_none(),
                "read-declared Mana must not yield a mut ptr"
            );
            // Shared read of Mana still works.
            assert_eq!(item.get::<Mana>(mana_id).map(|m| m.0), Some(50));
        });

        assert_eq!(world.get::<Hp>(a), Some(&Hp(101)));
        assert_eq!(
            world.get::<Mana>(a),
            Some(&Mana(50)),
            "Mana untouched — undeclared write refused"
        );
    }

    #[test]
    fn dyn_write_marks_changed() {
        let mut world = World::new();
        let e = world.spawn((Hp(1),));
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();

        world.advance_change_tick();
        let lr = world.last_run_tick();
        world.advance_change_tick();

        // Nothing changed since `lr` yet.
        assert_eq!(
            Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr).iter().count(),
            0
        );

        world
            .query_builder_mut()
            .write_id(hp_id)
            .build()
            .unwrap()
            .for_each_mut(|mut item| {
                item.get_mut::<Hp>(hp_id).unwrap().0 = 42;
            });

        // The dynamic write stamped the change tick.
        let changed: Vec<Entity> = Query::<Entity, Changed<Hp>>::new_with_tick(&world, lr)
            .iter()
            .collect();
        assert_eq!(changed, vec![e]);
        assert_eq!(world.get::<Hp>(e), Some(&Hp(42)));
    }

    #[test]
    fn dyn_write_aliased_is_loud() {
        let mut world = World::new();
        world.spawn((Hp(1),));
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
        let err = world
            .query_builder_mut()
            .write_id(hp_id)
            .write_id(hp_id)
            .build()
            .unwrap_err();
        assert_eq!(err, DynQueryError::AliasedWrite(hp_id));
    }

    #[test]
    fn dyn_write_unknown_name_is_loud() {
        let mut world = World::new();
        world.spawn((Hp(1),));
        let err = world
            .query_builder_mut()
            .write_name("nope::Nope")
            .build()
            .unwrap_err();
        assert_eq!(err, DynQueryError::UnknownComponent("nope::Nope".into()));
    }

    #[test]
    fn dyn_write_read_build_rejects_write_terms() {
        let mut world = World::new();
        world.spawn((Hp(1),));
        // The READ builder still rejects write terms (points at query_builder_mut).
        let err = world.query_builder().write::<Hp>().build().unwrap_err();
        assert_eq!(err, DynQueryError::WriteNotSupported);
    }

    #[test]
    fn dyn_write_get_mut_point_lookup_and_filter() {
        let mut world = World::new();
        let boss = world.spawn((Hp(100), Boss));
        let mob = world.spawn((Hp(10),));
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();

        let mut q = world
            .query_builder_mut()
            .write_id(hp_id)
            .with::<Boss>()
            .build()
            .unwrap();
        // Point-mutate the boss; the mob is filtered out.
        q.get_mut(boss).unwrap().get_mut::<Hp>(hp_id).unwrap().0 = 7;
        assert!(q.get_mut(mob).is_none());
        assert_eq!(world.get::<Hp>(boss), Some(&Hp(7)));
        assert_eq!(world.get::<Hp>(mob), Some(&Hp(10)));
    }

    #[test]
    fn dyn_write_mixed_read_and_write_terms() {
        let mut world = World::new();
        let e = world.spawn((Hp(3), Mana(10)));
        let hp_id = world.component_id_by_name(type_name::<Hp>()).unwrap();
        let mana_id = world.component_id_by_name(type_name::<Mana>()).unwrap();

        let mut q = world
            .query_builder_mut()
            .read_id(hp_id)
            .write_id(mana_id)
            .build()
            .unwrap();
        q.for_each_mut(|mut item| {
            // Read Hp, scale Mana by it. Sequential accessors (one &mut at a time).
            let hp = item.get::<Hp>(hp_id).unwrap().0;
            item.get_mut::<Mana>(mana_id).unwrap().0 *= hp;
        });
        assert_eq!(world.get::<Mana>(e), Some(&Mana(30)));
    }

    /// S1 part 2 (concurrency vector): write iteration is confined to the
    /// `&mut self` accessors (`iter_mut`/`for_each_mut`/`par_for_each_mut`),
    /// while `&self` iteration (`iter`/`for_each`) is bound to
    /// `ReadOnlyWorldQuery`. This keeps a shared `&Query`/`&CachedQuery` (both
    /// `Sync`) from handing out `Mut<T>` to two scoped threads at once. The
    /// read/write split must stay behavior-preserving: same rows visited, same
    /// mutations applied.
    #[test]
    fn s1_read_write_accessor_split() {
        let mut world = World::new();
        for i in 0u32..8 {
            world.spawn((Hp(i),));
        }

        // Exclusive `&mut self` write iteration mutates every row.
        Query::<Write<Hp>>::new_mut(&mut world).for_each_mut(|_, mut hp| hp.0 += 100);
        // `iter_mut` yields `Mut<T>` too (bump a second time via the iterator).
        for mut hp in Query::<Write<Hp>>::new_mut(&mut world).iter_mut() {
            hp.0 += 1000;
        }

        // Read-only `&self` iteration observes the writes; sum is stable.
        let sum: u32 = Query::<Read<Hp>>::new(&world).iter().map(|hp| hp.0).sum();
        // Σ i + 8*(100+1000) = 28 + 8800.
        assert_eq!(sum, 28 + 8800);

        let mut via_for_each = 0u32;
        Query::<Read<Hp>>::new(&world).for_each(|_, hp| via_for_each += hp.0);
        assert_eq!(via_for_each, sum);
    }
}
