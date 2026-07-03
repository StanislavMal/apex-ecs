//! [`UnsafeWorldCell`] — the single, explicit escape hatch for interior world
//! access.
//!
//! The soundness model (Bevy-style): safe write paths are constructed from
//! `&mut World` (exclusivity proven by the borrow checker); the scheduler,
//! having validated that concurrently running systems declare disjoint
//! access, may construct views through an `UnsafeWorldCell` instead — the
//! `unsafe` block at that construction site is where the disjointness
//! argument lives. All other code stays safe.

use std::cell::UnsafeCell;
use std::marker::PhantomData;

use crate::world::World;

/// A cell-like view of a [`World`]: a copyable token that can be turned into
/// `&World` or `&mut World` through `unsafe` methods, with the caller
/// promising that Rust's aliasing rules are upheld across all live views.
///
/// Two flavors exist:
/// - created from `&mut World` ([`World::as_unsafe_world_cell`]) — may vend
///   mutable views;
/// - created from `&World` ([`World::as_unsafe_world_cell_readonly`]) — must
///   only vend shared views (enforced by a debug assertion).
pub struct UnsafeWorldCell<'w> {
    ptr: *mut World,
    /// In debug builds a readonly cell refuses to vend `&mut World`.
    #[cfg(debug_assertions)]
    allows_mutable_access: bool,
    _marker: PhantomData<(&'w World, &'w UnsafeCell<World>)>,
}

impl Clone for UnsafeWorldCell<'_> {
    fn clone(&self) -> Self {
        *self
    }
}
impl Copy for UnsafeWorldCell<'_> {}

// SAFETY: `&World` and `&mut World` are both Send + Sync; the cell is just a
// pointer plus the caller-side contract.
unsafe impl Send for UnsafeWorldCell<'_> {}
unsafe impl Sync for UnsafeWorldCell<'_> {}

impl<'w> UnsafeWorldCell<'w> {
    /// Cell that may only vend shared views.
    #[inline]
    pub(crate) fn new_readonly(world: &'w World) -> Self {
        Self {
            ptr: std::ptr::from_ref(world).cast_mut(),
            #[cfg(debug_assertions)]
            allows_mutable_access: false,
            _marker: PhantomData,
        }
    }

    /// Cell that may vend both shared and mutable views.
    #[inline]
    pub(crate) fn new_mutable(world: &'w mut World) -> Self {
        Self {
            ptr: std::ptr::from_mut(world),
            #[cfg(debug_assertions)]
            allows_mutable_access: true,
            _marker: PhantomData,
        }
    }

    /// Shared view of the world.
    ///
    /// # Safety
    /// For the lifetime of the returned reference no `&mut World` (or any
    /// mutable view derived from this or another cell of the same world) may
    /// be live, except mutation of interior-mutable state (ticks, command
    /// queues) permitted by their own contracts.
    #[inline]
    pub unsafe fn world(self) -> &'w World {
        &*self.ptr
    }

    /// Mutable view of the world.
    ///
    /// # Safety
    /// The cell must originate from [`World::as_unsafe_world_cell`] (a
    /// readonly cell must never vend `&mut World`), and for the lifetime of
    /// the returned reference NO other view of the same world may be live.
    #[inline]
    pub unsafe fn world_mut(self) -> &'w mut World {
        #[cfg(debug_assertions)]
        debug_assert!(
            self.allows_mutable_access,
            "UnsafeWorldCell created from &World must not vend &mut World"
        );
        &mut *self.ptr
    }
}

impl World {
    /// Escape hatch: a cell that may vend mutable views. Exclusivity of the
    /// receiver proves no other reference to this world exists at creation;
    /// splitting that exclusivity into concurrent disjoint views is the
    /// caller's unsafe contract (see [`UnsafeWorldCell`]).
    #[inline]
    pub fn as_unsafe_world_cell(&mut self) -> UnsafeWorldCell<'_> {
        UnsafeWorldCell::new_mutable(self)
    }

    /// Escape hatch limited to shared views.
    #[inline]
    pub fn as_unsafe_world_cell_readonly(&self) -> UnsafeWorldCell<'_> {
        UnsafeWorldCell::new_readonly(self)
    }
}

#[cfg(test)]
mod tests {
    use crate::component::Component;
    use crate::world::World;

    #[derive(Debug, PartialEq)]
    struct Val(u32);
    impl Component for Val {}

    #[test]
    fn cell_roundtrip_shared_and_mutable() {
        let mut world = World::new();
        let e = world.spawn((Val(1),));

        let cell = world.as_unsafe_world_cell();
        // SAFETY: no other view of the world is live in this test scope.
        let w = unsafe { cell.world_mut() };
        w.get_mut::<Val>(e).unwrap().0 = 2;

        let cell = world.as_unsafe_world_cell_readonly();
        // SAFETY: only shared views are live.
        let w = unsafe { cell.world() };
        assert_eq!(w.get::<Val>(e), Some(&Val(2)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "must not vend &mut World")]
    fn readonly_cell_refuses_mutable_view() {
        let world = World::new();
        let cell = world.as_unsafe_world_cell_readonly();
        // SAFETY: intentionally violating the debug contract to test the
        // assertion; the reference is never used.
        let _ = unsafe { cell.world_mut() };
    }
}
