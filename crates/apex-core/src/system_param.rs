//! SystemParam — type-safe wrappers for system parameters.
//!
//! # API hierarchy (from simple to flexible)
//!
//! ## 1. AutoSystem — the recommended approach (automatic access inference)
//!
//! Access is inferred statically from `type Query`, `type Resources` and `type Events`.
//! It is impossible to accidentally forget a component, resource or event.
//!
//! ```ignore
//! struct MovementSystem;
//! impl AutoSystem for MovementSystem {
//!     type Query = (&Velocity, &mut Position);
//!     type Resources = ();
//!     type Events = ();
//!     fn run(&mut self, ctx: SystemContext<'_>) {
//!         ctx.query::<Self::Query>().for_each(|_, (vel, pos)| {
//!             pos.x += vel.x * 0.016;
//!         });
//!     }
//! }
//! sched.add_auto_system("movement", MovementSystem);
//! ```
//!
//! ```ignore
//! struct PhysicsSystem;
//! impl AutoSystem for PhysicsSystem {
//!     type Query     = (&Mass, &mut Velocity, &mut Position);
//!     type Resources = (ResRead<PhysicsConfig>, ResRead<DeltaTime>);
//!     type Events    = Emit<CollisionEvent>;
//!     fn run(&mut self, ctx: SystemContext<'_>) {
//!         let cfg = ctx.resource::<PhysicsConfig>();
//!         let mut writer = ctx.event_writer::<CollisionEvent>();
//!         ctx.query::<Self::Query>().for_each(|entity, (mass, vel, pos)| {
//!             vel.y -= cfg.gravity * mass.0 * cfg.dt;
//!             pos.x += vel.x * cfg.dt;
//!             if pos.y < 0.0 { writer.send(CollisionEvent { entity }); }
//!         });
//!     }
//! }
//! ```
//!
//! ## 2. FnParSystem — a closure with explicit access
//!
//! ```ignore
//! sched.add_fn_par_system("ai", |ctx| { ... },
//!     AccessDescriptor::new().read::<Enemy>().write::<Velocity>()
//! );
//! ```
//!
//! ## 3. Sequential — full &mut World
//!
//! ```ignore
//! sched.add_system("commands", |world: &mut World| { ... });
//! ```

use crate::{
    access::AccessDescriptor,
    component::Tick,
    events::{EventCursor, EventReadGuard, Events},
    query::WorldQuery,
};
use std::marker::PhantomData;

// ── Res / ResMut ───────────────────────────────────────────────

/// Immutable access to a resource.
///
/// The field is hidden (D2-1): a public `.0` SHADOWED Deref — `res.0` on a
/// tuple resource yielded `&T` instead of a resource field (a footgun for the
/// Bevy migrant). Access is via Deref (`*res`, `res.field`) or
/// [`into_inner`](Self::into_inner).
#[derive(Clone, Copy)]
pub struct Res<'w, T: Send + Sync + 'static>(pub(crate) &'w T);

impl<'w, T: Send + Sync + 'static> Res<'w, T> {
    /// Construct from a reference (low-level scheduler/bridge API).
    #[inline]
    pub fn new(value: &'w T) -> Self {
        Self(value)
    }

    /// Extract `&T` with the full world lifetime.
    #[inline]
    pub fn into_inner(self) -> &'w T {
        self.0
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for Res<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        self.0
    }
}

impl<T: Send + Sync + 'static + std::fmt::Debug> std::fmt::Debug for Res<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Res({:?})", self.0)
    }
}

/// Mutable access to a resource.
///
/// Carries the resource's change-tick cell and stamps it LAZILY on `deref_mut`
/// (RT-1, the A13 discipline of `Mut<T>`): merely holding a `ResMut<T>` is not
/// a change; writing through it is. `World::resource_changed_tick` /
/// `Resources::changed_tick` read the stamp.
pub struct ResMut<'w, T: Send + Sync + 'static> {
    ptr: *mut T,
    /// The resource's change-tick cell (`None` for bridge constructions that
    /// have no tick to stamp — see [`from_ptr`](Self::from_ptr)).
    tick: Option<*const crate::archetype::TickCell>,
    /// The tick stamped on mutation (the world's current tick at fetch).
    this_run: Tick,
    _marker: PhantomData<&'w mut T>,
}

impl<'w, T: Send + Sync + 'static> ResMut<'w, T> {
    /// # Safety
    /// `ptr` is valid for `'w`; the scheduler guarantees exclusive access.
    /// Change detection is BLIND through this constructor (no tick cell) —
    /// world/context fetch paths use [`from_raw_parts`](Self::from_raw_parts).
    pub unsafe fn from_ptr(ptr: *mut T) -> Self {
        Self {
            ptr,
            tick: None,
            this_run: Tick::ZERO,
            _marker: PhantomData,
        }
    }

    /// # Safety
    /// `ptr` and `tick` come from `Resources::get_raw_parts` and are valid for
    /// `'w`; the scheduler guarantees exclusive access to the resource.
    pub(crate) unsafe fn from_raw_parts(
        ptr: *mut T,
        tick: *const crate::archetype::TickCell,
        this_run: Tick,
    ) -> Self {
        Self {
            ptr,
            tick: Some(tick),
            this_run,
            _marker: PhantomData,
        }
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for ResMut<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.ptr }
    }
}

impl<T: Send + Sync + 'static> std::ops::DerefMut for ResMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        if let Some(tick) = self.tick {
            // SAFETY: the cell outlives `'w` (it lives in the resource slot);
            // exclusive access is the scheduler's guarantee for this ResMut.
            unsafe { (*tick).set(self.this_run) };
        }
        unsafe { &mut *self.ptr }
    }
}

unsafe impl<T: Send + Sync + 'static> Send for ResMut<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for ResMut<'_, T> {}

// ── EventReader / EventWriter ──────────────────────────────────

/// Event reader — uses a per-reader cursor.
///
/// # "Running is reading" (the retention contract)
///
/// A PERSISTENT reader (the declared `EventReader<E>` system param / `system!`
/// event param / `Listen<E>`) consumes its backlog by the mere fact that its
/// system RAN: on drop at the end of the run the cursor advances to the end of
/// the buffer, exactly as if the body had called `.read()` and ignored the
/// items. A system therefore NEVER has to "drain" its readers on an early
/// return — an un-drained reader cannot pin the queue's no-loss retention and
/// re-deliver stale events (the input-spam class of bug is unrepresentable).
///
/// No-loss retention remains for systems the SCHEDULER did not run (a
/// run-condition gate, a disabled stage): their cursor does not move and
/// events wait for them. "I want to accumulate" is expressed by declarative
/// gating, not by an early return inside the body. For a deliberate partial
/// consume use the queue-level [`Events::read_partial`]/`advance_reader_by`.
pub struct EventReader<'w, T: Send + Sync + 'static> {
    /// Mutable queue pointer. Derived from `&mut Events<T>` via a DIRECT
    /// `as *mut` cast (never through a shared reborrow), so writes through it
    /// keep provenance under Stacked/Tree Borrows (the MIRI-ER fix: the old
    /// `*const` cast inherited a read-only tag and `read()`'s `as_mut` write
    /// through it was UB).
    ptr: *mut Events<T>,
    cursor: EventCursor,
    /// `false` (default) — this reader OWNS its cursor and frees it on drop
    /// (standalone / one-shot use). `true` — the cursor is PERSISTENT, owned by
    /// a system's `SystemParam` state across frames (F4), so drop must NOT free
    /// it — the next frame reuses the same cursor; drop instead ADVANCES it to
    /// the end of the buffer ("running is reading", see the type docs).
    persistent: bool,
    _marker: PhantomData<&'w Events<T>>,
}

impl<'w, T: Send + Sync + 'static> EventReader<'w, T> {
    /// Create a reader with a fresh cursor (owns it — frees it on drop).
    /// # Panics
    /// Panics if events of type T are not registered.
    pub fn new(events: &'w mut Events<T>) -> Self {
        let cursor = events.add_reader();
        Self {
            ptr: events as *mut Events<T>,
            cursor,
            persistent: false,
            _marker: PhantomData,
        }
    }

    /// Create a reader over an EXISTING persistent cursor (F4). The cursor is
    /// owned by the caller (a system's per-frame `SystemParam` state) and is NOT
    /// freed on drop, so its read position survives across frames/runs — a
    /// FixedUpdate catch-up reads each event exactly once (no reset-to-zero).
    pub(crate) fn from_persistent(events: &'w mut Events<T>, cursor: EventCursor) -> Self {
        Self {
            ptr: events as *mut Events<T>,
            cursor,
            persistent: true,
            _marker: PhantomData,
        }
    }

    /// Iterate over unread events WITHOUT advancing the cursor (a peek). Note
    /// that a persistent reader still advances on drop at the end of the run —
    /// a peek defers within the run, not across frames (use a run-condition
    /// gate or [`Events::read_partial`] for cross-frame accumulation).
    #[inline]
    pub fn iter(&self) -> &[T] {
        unsafe { (*self.ptr).iter(&self.cursor) }
    }

    /// Read and automatically advance the cursor (RAII).
    #[inline]
    pub fn read(&mut self) -> EventReadGuard<'_, T> {
        unsafe { (*self.ptr).read(&self.cursor) }
    }

    /// Number of unread events.
    #[inline]
    pub fn len(&self) -> usize {
        self.iter().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.iter().is_empty()
    }
}

impl<T: Send + Sync + 'static> Drop for EventReader<'_, T> {
    fn drop(&mut self) {
        if self.persistent {
            // "Running is reading": the system had its scheduled chance this
            // run — advance to the end of the buffer (idempotent after
            // `.read()`, which already advanced). The cursor itself is owned by
            // the system's state and survives; only a NOT-RUN system (gated by
            // the scheduler) retains events. This closes the class of bug where
            // an early return skipped `.read()` and the stalled cursor pinned
            // retention forever (unbounded growth + re-delivery every frame).
            unsafe { (*self.ptr).advance_reader_mut(&self.cursor) };
            return;
        }
        unsafe { (*self.ptr).remove_reader(self.cursor) };
    }
}

/// Event writer — mutable access to Events.
pub struct EventWriter<'w, T: Send + Sync + 'static> {
    ptr: *mut Events<T>,
    _marker: PhantomData<&'w mut Events<T>>,
}

impl<'w, T: Send + Sync + 'static> EventWriter<'w, T> {
    /// # Safety
    /// `ptr` is valid for `'w`; the scheduler guarantees exclusive access.
    pub unsafe fn from_ptr(ptr: *mut Events<T>) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn send(&mut self, event: T) {
        unsafe {
            (*self.ptr).send(event);
        }
    }

    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        unsafe {
            (*self.ptr).send_batch(events);
        }
    }

    /// Pre-reserve capacity for the events to be sent.
    /// Avoids reallocations during bulk sends.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        unsafe {
            (*self.ptr).reserve(additional);
        }
    }
}

unsafe impl<T: Send + Sync + 'static> Send for EventWriter<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for EventWriter<'_, T> {}

/// Bevy-compatible name for reading removals of component `T` (D2-3):
/// an ordinary [`EventReader`] of [`Removed<T>`](crate::events::Removed) events.
///
/// Requires tracking to be enabled — `world.track_removals::<T>()` at setup
/// (unlike Bevy, where removals are always written for all components, here it
/// is opt-in with zero cost by default).
///
/// ```ignore
/// fn cleanup(mut removed: RemovedComponents<PhysicsBody>, mut phys: ResMut<Physics>) {
///     for r in removed.read() { phys.remove_body(r.entity); }
/// }
/// ```
pub type RemovedComponents<'w, T> = EventReader<'w, crate::events::Removed<T>>;

// ── Markers for ResourceAccessList ────────────────────────────

/// Marker: read access to resource T in `AutoSystem::Resources`.
///
/// Not to be confused with the runtime wrapper `Res<'w, T>` — this is only a
/// static access declaration for the scheduler.
pub struct ResRead<T: Send + Sync + 'static>(PhantomData<T>);

/// Marker: write access to resource T in `AutoSystem::Resources`.
pub struct ResWrite<T: Send + Sync + 'static>(PhantomData<T>);

// ── Markers for EventAccessList ────────────────────────────────

/// Marker: subscription to events of type E in `AutoSystem::Events`.
///
/// Corresponds to `ctx.event_reader::<E>()` inside `run()`.
pub struct Listen<E: Send + Sync + 'static>(PhantomData<E>);

/// Marker: publishing events of type E in `AutoSystem::Events`.
///
/// Corresponds to `ctx.event_writer_unchecked::<E>()` inside `run()`.
pub struct Emit<E: Send + Sync + 'static>(PhantomData<E>);

// ── ResourceAccessList ─────────────────────────────────────────

/// Static resource access declaration — used in `AutoSystem::Resources`.
///
/// Implemented for:
/// - `()` — no resource access (default)
/// - `ResRead<T>` — read access to resource T
/// - `ResWrite<T>` — write access to resource T
/// - tuples of the above (up to 8 elements)
pub trait ResourceAccessList {
    fn resource_accesses() -> crate::access::AccessDescriptor;
}

impl ResourceAccessList for () {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new()
    }
}

impl<T: Send + Sync + 'static> ResourceAccessList for ResRead<T> {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().read::<T>()
    }
}

// NB: write access to a resource sets `resource_write()` (the ASD gate, TD-37).
impl<T: Send + Sync + 'static> ResourceAccessList for ResWrite<T> {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().write::<T>().resource_write()
    }
}

macro_rules! impl_resource_access_list_tuple {
    ( $($R:ident),+ ) => {
        impl< $($R: ResourceAccessList),+ > ResourceAccessList for ( $($R,)+ ) {
            fn resource_accesses() -> crate::access::AccessDescriptor {
                crate::access::AccessDescriptor::new()
                    $( .merge(&$R::resource_accesses()) )+
            }
        }
    };
}

impl_resource_access_list_tuple!(A);
impl_resource_access_list_tuple!(A, B);
impl_resource_access_list_tuple!(A, B, C);
impl_resource_access_list_tuple!(A, B, C, D);
impl_resource_access_list_tuple!(A, B, C, D, E);
impl_resource_access_list_tuple!(A, B, C, D, E, F);
impl_resource_access_list_tuple!(A, B, C, D, E, F, G);
impl_resource_access_list_tuple!(A, B, C, D, E, F, G, H);

// ── EventAccessList ────────────────────────────────────────────

/// Static event access declaration — used in `AutoSystem::Events`.
///
/// Implemented for:
/// - `()` — no event access (default)
/// - `Listen<E>` — subscription to events E (read_event)
/// - `Emit<E>`   — publishing events E  (write_event)
/// - tuples of the above (up to 8 elements)
pub trait EventAccessList {
    fn event_accesses() -> crate::access::AccessDescriptor;
}

impl EventAccessList for () {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new()
    }
}

impl<E: Send + Sync + 'static> EventAccessList for Listen<E> {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().read_event::<E>()
    }
}

impl<E: Send + Sync + 'static> EventAccessList for Emit<E> {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().write_event::<E>()
    }
}

macro_rules! impl_event_access_list_tuple {
    ( $($E:ident),+ ) => {
        impl< $($E: EventAccessList),+ > EventAccessList for ( $($E,)+ ) {
            fn event_accesses() -> crate::access::AccessDescriptor {
                crate::access::AccessDescriptor::new()
                    $( .merge(&$E::event_accesses()) )+
            }
        }
    };
}

impl_event_access_list_tuple!(A);
impl_event_access_list_tuple!(A, B);
impl_event_access_list_tuple!(A, B, C);
impl_event_access_list_tuple!(A, B, C, D);
impl_event_access_list_tuple!(A, B, C, D, E);
impl_event_access_list_tuple!(A, B, C, D, E, F);
impl_event_access_list_tuple!(A, B, C, D, E, F, G);
impl_event_access_list_tuple!(A, B, C, D, E, F, G, H);

// ── SystemParam ────────────────────────────────────────────────

/// Type-safe extraction of system parameters from `SystemContext`.
///
/// The analogue of Bevy `SystemParam`, but WITHOUT the State/Fetch split,
/// WITHOUT proc macros, WITHOUT changing the existing `system!`/`sequential_system!` macros.
///
/// # Examples
///
/// ```ignore
/// // A single resource
/// type MyParam = ResRead<DeltaTime>;
/// let dt = MyParam::fetch(&ctx);
///
/// // A tuple: resource + query + events
/// type MyParam = (ResRead<DeltaTime>, QueryParam<(&Vel, &mut Pos)>, Listen<CollisionEvent>);
/// let (dt, q, events) = MyParam::fetch(&ctx);
/// ```
///
/// # Why
///
/// Simplifies porting the Bevy renderer (where `RenderCommand::Param: SystemParam`)
/// and removes boilerplate in sequential systems.
pub trait SystemParam {
    /// What is returned by [`fetch`](SystemParam::fetch) (with lifetime `'w`).
    type Item<'w>;

    /// Per-system persistent state (V3, wave 6b). Lives in the `fn_sys` closure
    /// across frames — the home for `Query`/`CachedQuery`/`Single` arch-index caches
    /// (and, later, event cursors). Stateless params use the default `()`.
    /// `Send + Sync` so the boxed system fn keeps its existing bounds (no ripple);
    /// `'static` because it outlives every call and owns no borrows.
    type State: Send + Sync + Default + 'static;

    /// Static access declaration for the scheduler.
    fn access() -> AccessDescriptor;

    /// Extract the value from the system context.
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w>;

    /// Stateful fetch (V3): extract the value using the per-system `state`. The default
    /// ignores `state` and calls [`fetch`](Self::fetch) (for stateless params:
    /// `Res`/`ResMut`/`EventReader`/`EventWriter`/`Commands`). Caching params
    /// (the `Query` family) override this to reuse the resolve across frames.
    fn get_param<'w>(
        ctx: &'w crate::world::SystemContext<'w>,
        _state: &'w mut Self::State,
    ) -> Self::Item<'w> {
        Self::fetch(ctx)
    }

    /// The param uses deferred commands (`Commands`) — the scheduler inserts an
    /// auto-apply sync point after the system (D2-1).
    fn has_deferred() -> bool {
        false
    }

    /// Validation before running the system (E5): `false` ⇒ the system is SKIPPED
    /// this frame (the skip semantics of `Single<Q>`; Bevy `validate_param`).
    /// Called by the scheduler immediately before [`fetch`](Self::fetch).
    fn validate(_ctx: &crate::world::SystemContext<'_>) -> bool {
        true
    }
}

// ── impl SystemParam for resource markers ────────────────────

impl<T: Send + Sync + 'static> SystemParam for ResRead<T> {
    type Item<'w> = Res<'w, T>;
    type State = ();
    fn access() -> AccessDescriptor {
        <Self as ResourceAccessList>::resource_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Res<'w, T> {
        ctx.resource::<T>()
    }
}

impl<T: Send + Sync + 'static> SystemParam for ResWrite<T> {
    type Item<'w> = ResMut<'w, T>;
    type State = ();
    fn access() -> AccessDescriptor {
        <Self as ResourceAccessList>::resource_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> ResMut<'w, T> {
        ctx.resource_mut_unchecked::<T>()
    }
}

// ── impl SystemParam for event markers ─────────────────────

impl<E: Send + Sync + 'static> SystemParam for Listen<E> {
    type Item<'w> = EventReader<'w, E>;
    type State = ();
    fn access() -> AccessDescriptor {
        <Self as EventAccessList>::event_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventReader<'w, E> {
        // Declared access (`Listen<E>`) validated by the scheduler. Route through
        // the per-system persistent cursor store (installed by the AutoSystem
        // adapter) — exactly-once delivery + "running is reading"; falls back to
        // a one-shot reader only outside a scheduler context.
        ctx.event_reader_persistent_auto::<E>()
    }
}

impl<E: Send + Sync + 'static> SystemParam for Emit<E> {
    type Item<'w> = EventWriter<'w, E>;
    type State = ();
    fn access() -> AccessDescriptor {
        <Self as EventAccessList>::event_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventWriter<'w, E> {
        ctx.event_writer_unchecked::<E>()
    }
}

// ── QueryParam marker ──────────────────────────────────────────

/// Marker: a component-query param via [`SystemParam`].
///
/// ```ignore
/// type MyParams = QueryParam<(&Position, &mut Velocity)>;
/// let q = MyParams::fetch(&ctx);
/// q.for_each(|entity, (pos, vel)| { ... });
/// ```
pub struct QueryParam<Q: WorldQuery>(PhantomData<Q>);

impl<Q: WorldQuery + WorldQuerySystemAccess> SystemParam for QueryParam<Q> {
    type Item<'w> = crate::query::Query<'w, 'w, Q>;
    type State = ();
    fn access() -> AccessDescriptor {
        Q::system_access()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> crate::query::Query<'w, 'w, Q> {
        // `query_unchecked`: `Q` may be mutable (`&mut T`), which is sound here —
        // this IS the declared parameter, so the scheduler validated the access and
        // serializes conflicts (F3). The public `ctx.query` is read-only-bound.
        ctx.query_unchecked::<Q>()
    }
}

// ── CommandsParam marker ───────────────────────────────────────

/// Marker: an access param for [`Commands`](crate::commands::Commands).
///
/// ```ignore
/// type MyParams = CommandsParam;
/// let cmds = MyParams::fetch(&ctx);
/// cmds.spawn((Position { x: 0.0 }, Velocity { x: 1.0 }));
/// ```
pub struct CommandsParam;

impl SystemParam for CommandsParam {
    type Item<'w> = &'w mut crate::commands::Commands;
    type State = ();
    fn access() -> AccessDescriptor {
        // `commands_used` gates ASD: non-entity-local commands would be duplicated across chunks.
        AccessDescriptor::new().commands_used()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> &'w mut crate::commands::Commands {
        ctx.commands()
    }
    fn has_deferred() -> bool {
        true
    }
}

// ── Bevy-style params of plain-fn systems (D2-1) ────────────────
//
// In the plain-fn path (see [`SystemParamFunction`](crate::fn_system::SystemParamFunction))
// the parameter is the user type ITSELF: `Res<T>`, `ResMut<T>`,
// `Query<Q>`, `EventReader<E>`, `EventWriter<E>`, `&mut Commands` — Bevy
// semantics 1:1 (unlike `system!`, where `&T` means a resource).
// Item must be the same type constructor as the parameter (the double
// Fn-bound SystemParamFunction ties both forms together).

impl<'a, T: Send + Sync + 'static> SystemParam for Res<'a, T> {
    type Item<'w> = Res<'w, T>;
    type State = ();
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Res<'w, T> {
        ctx.resource::<T>()
    }
}

impl<'a, T: Send + Sync + 'static> SystemParam for ResMut<'a, T> {
    type Item<'w> = ResMut<'w, T>;
    type State = ();
    fn access() -> AccessDescriptor {
        // `write::<T>()` — for conflict analysis (two `ResMut<T>` do not parallelize);
        // `resource_write()` — the ASD gate: a system that mutates a resource cannot be split into chunks
        // (the body would run once per chunk ⇒ mutation × number of chunks). See TD-37.
        AccessDescriptor::new().write::<T>().resource_write()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> ResMut<'w, T> {
        ctx.resource_mut_unchecked::<T>()
    }
}

impl<'a, 's, Q, F> SystemParam for crate::query::Query<'a, 's, Q, F>
where
    Q: WorldQuery + WorldQuerySystemAccess,
    F: WorldQuery + WorldQuerySystemAccess,
{
    type Item<'w> = crate::query::Query<'w, 'w, Q, F>;
    type State = ();
    fn access() -> AccessDescriptor {
        Q::system_access().merge(&F::system_access())
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> crate::query::Query<'w, 'w, Q, F> {
        // D6: the Changed/Added baseline is THIS system's per-system `last_run` (set by
        // the scheduler), same as ctx.query() — so a gated system sharing a stage sees
        // changes since IT last ran, not since the stage last ran (TD-9 / Bevy parity).
        let sub = &ctx.sub_worlds[0];
        let last_run = ctx.last_run();
        // SAFETY: `ctx` (and its SubWorlds) is vended by the scheduler for a
        // system whose declared access (`Self::access()`) covers exactly this
        // shape; conflicting systems never run concurrently.
        unsafe { crate::query::Query::from_sub_world(sub, last_run) }
    }
}

/// `Single<Q, F>` — exactly one match; the system is skipped on 0 or >1 (E5).
impl<'a, Q, F> SystemParam for crate::query::Single<'a, Q, F>
where
    Q: WorldQuery + WorldQuerySystemAccess,
    F: WorldQuery + WorldQuerySystemAccess,
{
    type Item<'w> = crate::query::Single<'w, Q, F>;
    type State = ();
    fn access() -> AccessDescriptor {
        Q::system_access().merge(&F::system_access())
    }
    fn validate(ctx: &crate::world::SystemContext<'_>) -> bool {
        // `iter_mut` (not `iter`): the shape may be a write query, which no longer
        // satisfies the `&self` read-only iterator bound (S1 part 2). The local
        // owns `q`, so the exclusive borrow is trivially available.
        let mut q = <crate::query::Query<'_, '_, Q, F> as SystemParam>::fetch(ctx);
        q.iter_mut().take(2).count() == 1
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w> {
        let q = <crate::query::Query<'w, 'w, Q, F> as SystemParam>::fetch(ctx);
        match q.single_inner() {
            Ok((entity, item)) => crate::query::Single {
                entity,
                item,
                _filter: std::marker::PhantomData,
            },
            // Unreachable after validate (the scheduler calls validate→fetch
            // atomically within the system's slot); the panic is for manual
            // fetch calls without validate.
            Err(e) => panic!(
                "Single<…>: query yielded not exactly one match ({e:?});                  systems with Single are skipped by the scheduler"
            ),
        }
    }
}

/// `Option<Single<Q, F>>` — `None` on zero matches; skip only on >1 (E5).
impl<'a, Q, F> SystemParam for Option<crate::query::Single<'a, Q, F>>
where
    Q: WorldQuery + WorldQuerySystemAccess,
    F: WorldQuery + WorldQuerySystemAccess,
{
    type Item<'w> = Option<crate::query::Single<'w, Q, F>>;
    type State = ();
    fn access() -> AccessDescriptor {
        Q::system_access().merge(&F::system_access())
    }
    fn validate(ctx: &crate::world::SystemContext<'_>) -> bool {
        // `iter_mut`: see the `Single` impl above — the shape may be a write query.
        let mut q = <crate::query::Query<'_, '_, Q, F> as SystemParam>::fetch(ctx);
        q.iter_mut().take(2).count() <= 1
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w> {
        let q = <crate::query::Query<'w, 'w, Q, F> as SystemParam>::fetch(ctx);
        match q.single_inner() {
            Ok((entity, item)) => Some(crate::query::Single {
                entity,
                item,
                _filter: std::marker::PhantomData,
            }),
            Err(crate::query::QuerySingleError::NoEntities) => None,
            Err(e) => panic!(
                "Option<Single<…>>: more than one match ({e:?});                  systems are skipped by the scheduler"
            ),
        }
    }
}

/// Per-system persistent state for an [`EventReader`] param (F4). Holds the
/// system's own event cursor, created on first run and reused every frame so
/// the read position survives across frames and FixedUpdate catch-up runs
/// (`Send + Sync + Default + 'static` — [`EventCursor`] is a `Copy` `u32`).
pub struct EventReaderState<E> {
    cursor: Option<EventCursor>,
    _marker: PhantomData<fn() -> E>,
}

impl<E> Default for EventReaderState<E> {
    fn default() -> Self {
        Self { cursor: None, _marker: PhantomData }
    }
}

impl<'a, E: Send + Sync + 'static> SystemParam for EventReader<'a, E> {
    type Item<'w> = EventReader<'w, E>;
    type State = EventReaderState<E>;
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().read_event::<E>()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventReader<'w, E> {
        // Stateless fallback (no persistent cursor): a fresh one-shot reader.
        // Declared `EventReader<E>` access validated by the scheduler (S4 / ADR-002).
        ctx.event_reader_unchecked::<E>()
    }
    fn get_param<'w>(
        ctx: &'w crate::world::SystemContext<'w>,
        state: &'w mut Self::State,
    ) -> EventReader<'w, E> {
        // F4: reuse a persistent per-system cursor (in `state`) so reads resume
        // across frames / FixedUpdate runs instead of restarting from zero.
        ctx.event_reader_persistent::<E>(&mut state.cursor)
    }
}

impl<'a, E: Send + Sync + 'static> SystemParam for EventWriter<'a, E> {
    type Item<'w> = EventWriter<'w, E>;
    type State = ();
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().write_event::<E>()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventWriter<'w, E> {
        ctx.event_writer_unchecked::<E>()
    }
}

impl SystemParam for &mut crate::commands::Commands {
    type Item<'w> = &'w mut crate::commands::Commands;
    type State = ();
    fn access() -> AccessDescriptor {
        // `commands_used` gates ASD: non-entity-local commands would be duplicated across chunks.
        AccessDescriptor::new().commands_used()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> &'w mut crate::commands::Commands {
        ctx.commands()
    }
    fn has_deferred() -> bool {
        true
    }
}

// ── impl SystemParam for () (no parameters) ──────────────────

impl SystemParam for () {
    type Item<'w> = ();
    type State = ();
    fn access() -> AccessDescriptor {
        AccessDescriptor::new()
    }
    fn fetch<'w>(_ctx: &crate::world::SystemContext<'w>) {}
}

// ── SystemParam tuples (1..12) ───────────────────────────────

macro_rules! impl_system_param_tuple {
    ( $($P:ident),+ ) => {
        impl< $($P: SystemParam),+ > SystemParam for ( $($P,)+ ) {
            type Item<'w> = ( $($P::Item<'w>,)+ );
            type State = ( $($P::State,)+ );
            fn access() -> AccessDescriptor {
                AccessDescriptor::new()
                    $( .merge(&$P::access()) )+
            }
            fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w> {
                ( $($P::fetch(ctx),)+ )
            }
            #[allow(non_snake_case)]
            fn get_param<'w>(
                ctx: &'w crate::world::SystemContext<'w>,
                state: &'w mut Self::State,
            ) -> Self::Item<'w> {
                // Thread each param's own state slot (V3). Names reuse $P as the slot binding.
                let ( $($P,)+ ) = state;
                ( $($P::get_param(ctx, $P),)+ )
            }
            fn has_deferred() -> bool {
                false $( || $P::has_deferred() )+
            }
            fn validate(ctx: &crate::world::SystemContext<'_>) -> bool {
                true $( && $P::validate(ctx) )+
            }
        }
    };
}

impl_system_param_tuple!(A);
impl_system_param_tuple!(A, B);
impl_system_param_tuple!(A, B, C);
impl_system_param_tuple!(A, B, C, D);
impl_system_param_tuple!(A, B, C, D, E);
impl_system_param_tuple!(A, B, C, D, E, F);
impl_system_param_tuple!(A, B, C, D, E, F, G);
impl_system_param_tuple!(A, B, C, D, E, F, G, H);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

// ── WorldQuerySystemAccess ─────────────────────────────────────

/// An extension of WorldQuery — a static R/W access declaration for the scheduler.
///
/// Implemented for &T, &mut T, With<T>, Without<T>, Changed<T>
/// and tuples of them in query.rs.
///
/// It is the basis for `AutoSystem::access()` — it lets the scheduler
/// obtain an `AccessDescriptor` without manually enumerating components.
pub trait WorldQuerySystemAccess: WorldQuery {
    fn system_access() -> AccessDescriptor;
}

// ── AutoSystem ─────────────────────────────────────────────────

/// A parallel system with automatic AccessDescriptor inference.
///
/// # Motivation
///
/// When using `ParSystem` with an explicit `AccessDescriptor` there is a risk
/// of forgetting to declare a component:
///
/// ```ignore
/// // BUG: &mut Position is not listed — the scheduler does not see the conflict
/// fn access() -> AccessDescriptor {
///     AccessDescriptor::new().read::<Velocity>() // forgot write::<Position>()
/// }
/// fn run(&mut self, ctx: SystemContext<'_>) {
///     ctx.for_each::<(&Velocity, &mut Position), _>(...)
///     //                                        ^^^^^^^^^^^^^^^ we write, but did not declare it
/// }
/// ```
///
/// `AutoSystem` eliminates this class of bugs: access is inferred from `type Query`
/// statically at compile time.
///
/// # Resources and events
///
/// If a system needs access to resources or events, specify them in the
/// associated types `Resources` and `Events`:
///
/// # Examples
///
/// ```ignore
/// // Components only
/// struct MovementSystem;
/// impl AutoSystem for MovementSystem {
///     type Query = (&Velocity, &mut Position);
///     fn run(&mut self, ctx: SystemContext<'_>) {
///         ctx.query::<Self::Query>().for_each(|_, (vel, pos)| {
///             pos.x += vel.x * 0.016;
///         });
///     }
/// }
///
/// // Components + resources + events
/// struct PhysicsSystem;
/// impl AutoSystem for PhysicsSystem {
///     type Query     = (&Mass, &mut Velocity, &mut Position);
///     type Resources = ResRead<DeltaTime>;
///     type Events    = Emit<CollisionEvent>;
///     fn run(&mut self, ctx: SystemContext<'_>) {
///         let dt = ctx.resource::<DeltaTime>().0;
///         let mut writer = ctx.event_writer::<CollisionEvent>();
///         ctx.query::<Self::Query>().for_each(|entity, (mass, vel, pos)| {  });
///     }
/// }
///
/// ```
pub trait AutoSystem: Send + Sync {
    /// A component query — part of the `AccessDescriptor` is inferred from it.
    type Query: WorldQuery + WorldQuerySystemAccess;

    /// The resources the system needs.
    type Resources: ResourceAccessList;

    /// The events the system reads or writes.
    type Events: EventAccessList;

    /// The system needs ALL entities (global access).
    /// ASD chunking is forbidden; the system always receives the full SubWorld.
    /// Defaults to `false`.
    const NEEDS_WHOLE_WORLD: bool = false;

    /// The system uses Commands (deferred operations).
    /// Set by the `system!` macro automatically when it detects `cmd: Cmd`.
    /// Lets `compile()` insert auto-apply sync points.
    const HAS_DEFERRED: bool = false;

    fn run(&mut self, ctx: crate::world::SystemContext<'_>);

    fn name() -> &'static str
    where
        Self: Sized,
    {
        std::any::type_name::<Self>()
    }
}

/// An exclusive system — receives the full `&mut World`.
///
/// Generated by the same [`system!`](crate::system) macro when the parameters
/// include `world: &mut World`. Such a system declares **FULL access**
/// (conflicts with everything) and is run by the scheduler alone (a sync point),
/// between parallel batches. `world: &mut World` cannot be combined with
/// other data parameters — the macro checks this and emits a compile error.
///
/// This replaces the removed `sequential_system!`: a single `system!` macro for
/// parallel and exclusive systems, the mode chosen by the presence of `&mut World`.
pub trait ExclusiveSystem: Send + 'static {
    fn run(&mut self, world: &mut crate::world::World);

    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// Any closure/function `FnMut(&mut World)` is an exclusive system.
///
/// Unifies registration: `add_exclusive_system` / `add_systems` accept both
/// struct markers from `system!` and plain `fn(&mut World)` (for example,
/// `propagate_transforms`) and inline closures — a single professional path.
/// There is no conflict with hand-written `impl ExclusiveSystem` from the macro:
/// struct markers do not implement `FnMut`.
impl<F> ExclusiveSystem for F
where
    F: FnMut(&mut crate::world::World) + Send + 'static,
{
    #[inline]
    fn run(&mut self, world: &mut crate::world::World) {
        self(world)
    }
}

// ── Extract<P> — a Bevy-compatible SystemParam for extract systems ──

/// A Bevy-compatible parameter of extract systems — reads from [`MainWorld`].
///
/// During the extract stage the render world holds a temporary `MainWorld` resource.
/// `Extract<P>` transparently applies the inner `SystemParam P` to that world,
/// not to the render world.
///
/// # Example
///
/// ```ignore
/// system! {
///     fn extract_cameras(
///         q: &Extract<QueryParam<(&Camera, &GlobalTransform)>>,
///         out: &mut ExtractedCamera,
///     ) {
///         for (_, (cam, transform)) in q.iter() {
///             *out = ExtractedCamera::new(cam, transform);
///         }
///     }
/// }
/// ```
///
/// After the extract stage `MainWorld` is removed from the render world and returned to the main thread.
pub struct Extract<P>(PhantomData<P>);

// Extract<QueryParam<Q>> — reads components from MainWorld. The shape must be
// read-only: by contract extract does NOT write to the main world (a write shape
// now will not even compile — ReadOnlyWorldQuery).
impl<Q: WorldQuery + WorldQuerySystemAccess + crate::query::ReadOnlyWorldQuery> SystemParam
    for Extract<QueryParam<Q>>
{
    type Item<'w> = crate::query::Query<'w, 'w, Q>;
    type State = ();

    fn access() -> AccessDescriptor {
        Q::system_access()
    }

    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> crate::query::Query<'w, 'w, Q> {
        let mw: Res<'w, crate::world::MainWorld> = ctx.resource();
        // Res.0 is &'w MainWorld; MainWorld.world() borrows and returns &'w World
        crate::query::Query::from_world_cached(mw.0.world(), crate::component::Tick(0))
    }
}

// Extract<ResRead<T>> — reads a resource from MainWorld
impl<T: Send + Sync + 'static> SystemParam for Extract<ResRead<T>> {
    type Item<'w> = Res<'w, T>;
    type State = ();

    fn access() -> AccessDescriptor {
        <ResRead<T> as ResourceAccessList>::resource_accesses()
    }

    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Res<'w, T> {
        let mw: Res<'w, crate::world::MainWorld> = ctx.resource();
        let world: &crate::world::World = mw.0.world();
        Res(world.resource::<T>())
    }
}

// Extract<Listen<E>> — reads events from MainWorld
impl<E: Send + Sync + 'static> SystemParam for Extract<Listen<E>> {
    type Item<'w> = EventReader<'w, E>;
    type State = ();

    fn access() -> AccessDescriptor {
        <Listen<E> as EventAccessList>::event_accesses()
    }

    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventReader<'w, E> {
        let mw: Res<'w, crate::world::MainWorld> = ctx.resource();
        let world: &crate::world::World = mw.0.world();
        // S3 / ADR-002: only a shared `&World` is available here (MainWorld is read
        // through a `Res`). Sound because Extract runs in the sequential extract
        // stage under a declared `Extract<Listen<E>>` access — no concurrent access
        // to `E`'s queue — so the `_unchecked` cursor advance cannot race.
        world.event_reader_unchecked::<E>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventCursor, Events};

    /// RT-1: `ResMut` stamps the resource change tick LAZILY — holding it or
    /// reading through it is not a change; `deref_mut` is (the resource analog
    /// of `Mut<T>`'s A13 discipline).
    #[test]
    fn resmut_stamps_tick_only_on_deref_mut() {
        struct Score(u32);
        let mut res = crate::resources::Resources::new();
        res.insert(Score(1), Tick(1));
        let (ptr, tick) = res.get_raw_parts::<Score>().expect("present");

        // SAFETY: the test is the sole owner of the resource.
        let mut rm: ResMut<'_, Score> = unsafe { ResMut::from_raw_parts(ptr, tick, Tick(5)) };
        assert_eq!(rm.0, 1, "read through Deref");
        assert_eq!(res.changed_tick::<Score>(), Some(Tick(1)), "read is not a change");

        rm.0 = 2;
        assert_eq!(res.changed_tick::<Score>(), Some(Tick(5)), "deref_mut stamped this_run");
    }

    /// F4: a persistent per-system cursor resumes across runs — reading the same
    /// readable buffer twice (a FixedUpdate catch-up) does NOT duplicate, unlike
    /// a fresh cursor that restarts at zero every run.
    #[test]
    fn persistent_event_reader_no_duplicate_reads() {
        let mut events = Events::<u32>::new();
        events.send(10);
        events.send(20);
        events.update(); // 10, 20 now in the readable buffer

        // The system's SystemParam state — its persistent cursor across runs.
        let mut cursor: Option<EventCursor> = None;

        // Run 1: create/resume, read all, advance (the read() guard advances on drop).
        {
            let c = match cursor {
                Some(c) => c,
                None => {
                    let c = events.add_reader();
                    cursor = Some(c);
                    c
                }
            };
            let mut r = EventReader::from_persistent(&mut events, c);
            assert_eq!(r.read().as_slice().to_vec(), vec![10, 20]);
            // r dropped: persistent => cursor NOT freed; position advanced to end.
        }

        // Run 2 in the SAME frame (no update): resume from the persistent cursor.
        {
            let c = cursor.expect("cursor persisted across runs");
            let mut r = EventReader::from_persistent(&mut events, c);
            assert!(
                r.read().as_slice().is_empty(),
                "persistent cursor resumed — the second run reads nothing (no dup)"
            );
        }

        // Contrast: a FRESH cursor (the pre-F4 behavior) re-reads the whole buffer.
        {
            let mut fresh = EventReader::new(&mut events);
            assert_eq!(
                fresh.read().as_slice().to_vec(),
                vec![10, 20],
                "a fresh reader restarts at 0 — exactly the duplicate F4 removes"
            );
        }
    }

    /// "Running is reading": a persistent reader that is created and dropped
    /// WITHOUT `.read()` (a system body that early-returned) still advances its
    /// cursor — retention never pins on a ran-but-unread system, the buffer is
    /// GC'd on the next update, and nothing is re-delivered.
    #[test]
    fn persistent_reader_advances_on_drop_without_read() {
        let mut events = Events::<u32>::new();
        let cursor = events.add_reader();

        events.send(1);
        events.update(); // [1] readable

        // Run 1: the system ran but its body never called .read() (early return).
        {
            let _r = EventReader::from_persistent(&mut events, cursor);
        }
        // The scheduled chance was used: cursor advanced, everyone caught up.
        events.send(2);
        events.update();
        assert_eq!(events.retained_ticks(), 0, "no retention from a ran-but-unread system");

        // Run 2 actually reads: it sees ONLY the new event — no re-delivery of 1.
        {
            let mut r = EventReader::from_persistent(&mut events, cursor);
            assert_eq!(
                r.read().as_slice().to_vec(),
                vec![2],
                "event 1 was consumed by RUNNING (drop-advance); only 2 is new"
            );
        }
    }

    /// The no-loss counterpart: a cursor whose system did NOT run (scheduler
    /// gate ⇒ no reader wrapper was ever constructed) holds the buffer — events
    /// wait, and the gated system receives ALL of them when it finally runs.
    #[test]
    fn gated_reader_still_retains_events() {
        let mut events = Events::<u32>::new();
        let gated = events.add_reader(); // registered, but its system never runs

        events.send(1);
        events.update();
        events.send(2);
        events.update(); // gated cursor lags ⇒ retained, appended

        assert!(events.retained_ticks() > 0, "buffer retained for the gated system");

        // The gate finally opens: the system runs and reads EVERYTHING.
        {
            let mut r = EventReader::from_persistent(&mut events, gated);
            assert_eq!(
                r.read().as_slice().to_vec(),
                vec![1, 2],
                "no-loss: the gated system received every event on wake"
            );
        }
        events.update();
        assert_eq!(events.retained_ticks(), 0, "retention released after catch-up");
    }
}
