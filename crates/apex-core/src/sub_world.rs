use crate::{
    system_param::{EventReader, EventWriter, Res, ResMut},
    World,
};

/// A view over a subset of a World's archetypes.
///
/// Holds the archetype indices matching a system's AccessDescriptor.
/// Does not own the data — it only references it through the World.
///
/// # Row-level splits (5.7)
///
/// If the `row_ranges` field is non-empty, iteration is restricted to the given
/// row ranges `(arch_idx, start, end)`. This lets multiple systems with the same
/// ArchetypeMask process different rows of the same archetype in parallel.
///
/// # Safety
/// - SubWorld does not own the data — the World must stay alive for the entire
///   duration of use.
/// - Different SubWorlds for different systems in the same Stage do not overlap
///   by archetype (verified by compile() via AccessDescriptor).
/// - Structural changes are forbidden while systems are running.
///
/// # Storage
/// `world`, `archetype_indices` and `row_ranges` are stored as raw pointers, to
/// avoid the UB of extending a lifetime and to eliminate the `&World`/`&mut World`
/// conflict under parallel execution.
pub struct SubWorld<'w> {
    /// Raw pointer to the World. Always valid while the SubWorld exists.
    world: *const World,
    /// Indices of the archetypes belonging to this SubWorld (raw fat pointer)
    archetype_indices: *const [usize],
    /// Optional row constraints for row-level splits.
    row_ranges: *const [(usize, usize, usize)],
    #[allow(dead_code)]
    _phantom: std::marker::PhantomData<&'w ()>,
}

impl<'w> SubWorld<'w> {
    /// All input references are tied to `'w`: the raw pointers inside cannot
    /// outlive the borrowed data (otherwise one could obtain a
    /// `SubWorld<'static>` from temporary references and dereference a dangling
    /// pointer).
    ///
    /// # Safety
    /// A `SubWorld` vends mutable views (`resource_mut`, `event_writer`, write
    /// queries via `Query::from_sub_world`) out of the shared `&'w World`.
    /// The caller must guarantee that, for the SubWorld's lifetime, no other
    /// live access conflicts with the accesses performed through it (the
    /// scheduler guarantees this by validating declared system accesses).
    #[inline]
    pub unsafe fn new(world: &'w World, archetype_indices: &'w [usize]) -> Self {
        Self {
            world: world as *const World,
            archetype_indices: archetype_indices as *const [usize],
            row_ranges: std::ptr::slice_from_raw_parts(
                std::ptr::null::<(usize, usize, usize)>(),
                0,
            ),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create a SubWorld with row-level range constraints. All input references
    /// are tied to `'w` (the lifetimes guarantee the raw pointers cannot outlive
    /// the data).
    ///
    /// # Safety
    /// Same contract as [`new`](Self::new).
    #[inline]
    pub unsafe fn with_ranges(
        world: &'w World,
        archetype_indices: &'w [usize],
        row_ranges: &'w [(usize, usize, usize)],
    ) -> Self {
        Self {
            world: world as *const World,
            archetype_indices: archetype_indices as *const [usize],
            row_ranges: row_ranges as *const [(usize, usize, usize)],
            _phantom: std::marker::PhantomData,
        }
    }

    /// Build a SubWorld from a RAW `*const World` whose lifetime is NOT tied to `'w`.
    ///
    /// Takes a raw pointer (not `&World`) DELIBERATELY (MIRI-CD, 2026-07-06): the
    /// scheduler interleaves this read view with `&mut World` writes (the per-stage tick
    /// bump, `Commands` apply, sequential systems) through a sibling `*mut World`. If
    /// this took `&World`, the stored pointer would inherit that reference's borrow tag,
    /// which the later `&mut World` write would invalidate → reading `world()` afterwards
    /// is UB under both Stacked and Tree Borrows. A raw pointer carries no such tag: the
    /// caller passes the SAME raw `*const World` it derives its transient `&mut *ptr`
    /// writes from, so those writes (children of that pointer) never disable it.
    ///
    /// # Safety
    /// `world` must point to a live `World` for at least the SubWorld's lifetime, and
    /// while the SubWorld is in use there must be no CONCURRENT `&mut World` aliasing the
    /// covered archetypes (the scheduler guarantees this: transient, non-overlapping
    /// `&mut *ptr` reborrows, no structural changes while systems run). `archetype_indices`
    /// must likewise outlive the SubWorld.
    #[inline]
    pub unsafe fn from_raw(world: *const World, archetype_indices: &[usize]) -> Self {
        Self {
            world,
            archetype_indices: archetype_indices as *const [usize],
            row_ranges: std::ptr::slice_from_raw_parts(
                std::ptr::null::<(usize, usize, usize)>(),
                0,
            ),
            _phantom: std::marker::PhantomData,
        }
    }

    /// [`from_raw`](Self::from_raw) with row-level range constraints.
    ///
    /// # Safety
    /// See [`from_raw`](Self::from_raw); additionally `row_ranges` must live at
    /// least as long as the SubWorld.
    #[inline]
    pub unsafe fn from_raw_with_ranges(
        world: *const World,
        archetype_indices: &[usize],
        row_ranges: &[(usize, usize, usize)],
    ) -> Self {
        Self {
            world,
            archetype_indices: archetype_indices as *const [usize],
            row_ranges: row_ranges as *const [(usize, usize, usize)],
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get a reference to the World.
    #[inline]
    fn world_ref(&self) -> &World {
        unsafe { &*self.world }
    }

    /// Return the slice of archetype indices.
    #[inline]
    fn archetype_indices_slice(&self) -> &[usize] {
        if self.archetype_indices.is_null() {
            return &[];
        }
        unsafe { &*self.archetype_indices }
    }

    /// Return the row_ranges slice.
    #[inline]
    fn row_ranges_slice(&self) -> &[(usize, usize, usize)] {
        if self.row_ranges.is_null() {
            return &[];
        }
        unsafe { &*self.row_ranges }
    }

    // ── Public API ──────────────────────────────

    /// The underlying world, at the full `'w` lifetime (the raw pointer is valid
    /// for the SubWorld's whole lifetime). Returning `&'w World` — rather than the
    /// shorter `&self` borrow — lets [`Query::from_sub_world`](crate::query::Query::from_sub_world)
    /// keep the world borrow (`'w`) independent of the state borrow (`'s`).
    #[inline]
    pub fn world(&self) -> &'w World {
        unsafe { &*self.world }
    }

    #[inline]
    pub fn archetype_indices(&self) -> &[usize] {
        self.archetype_indices_slice()
    }

    #[inline]
    pub fn row_ranges(&self) -> &[(usize, usize, usize)] {
        self.row_ranges_slice()
    }

    /// Number of archetypes in this SubWorld.
    #[inline]
    pub fn archetype_count(&self) -> usize {
        self.archetype_indices_slice().len()
    }

    /// Total number of entities across all archetypes of this SubWorld.
    pub fn entity_count(&self) -> usize {
        let w = self.world_ref();
        self.archetype_indices_slice()
            .iter()
            .map(|&idx| unsafe { (&*w.archetype_ptr(idx)).len() })
            .sum()
    }

    // ── Resource API ────────────────────────────

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(self.world_ref().resource::<T>())
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = self
                .world_ref()
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    // ── Event API ───────────────────────────────

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        unsafe {
            let ptr = self
                .world_ref()
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self
                .world_ref()
                .event_queue_ptr::<T>()
                .expect("event_writer: event queue not found");
            EventWriter::from_ptr(ptr)
        }
    }

}

unsafe impl Send for SubWorld<'_> {}
unsafe impl Sync for SubWorld<'_> {}
