use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::any::TypeId;
use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::RwLock;

use crate::{
    archetype::{Archetype, ArchetypeId, TickCell},
    commands::Commands,
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry, Tick},
    entity::{Entity, EntityAllocator, EntityLocation},
    error::{ErrorHandler, ErrorMode},
    events::EventRegistry,
    query::{QueryBuilder, WorldQuery},
    relations::{RelationRegistry, SubjectIndex, TargetIndex},
    resources::Resources,
    system_param::{EventReader, EventWriter, Res, ResMut},
    template::TemplateRegistry,
};

// ── QueryCache ─────────────────────────────────────────────────

struct CacheEntry {
    arch_indices: Arc<[usize]>,
    /// How many of the world's archetypes this entry has already seen (archetypes
    /// are append-only — the list is only extended by the tail
    /// `archetypes[seen_arch_count..]`).
    seen_arch_count: usize,
}

/// Query cache key. The `ids` list ALONE is not enough: `(Read<A>, Read<B>)`
/// and `(Read<A>, Without<B>)` produce the same `fill_ids` but different match
/// semantics — a key on ids alone would poison the cache between them. The
/// triple (ids, positive, required) uniquely defines the shape's match
/// semantics: without-set = ids − positive, optional-set = positive − required.
/// Query cache key (CR-M2b): one `u64` per component — ComponentId in the
/// lower 32 bits, role (required/without/optional) in the upper bits
/// (`WorldQuery::fill_cache_key`). Uniquely encodes the shape's match semantics:
/// `(Read<A>, Read<B>)`, `(Read<A>, Without<B>)` and `(Read<A>, Maybe<B>)` are
/// DISTINCT entries (they previously shared one — cache poisoning).
///
/// Allocation-free hot path: the key is built in a single pass into an inline
/// SmallVec, lookup is zero-copy over `&[u64]` (Borrow), ownership is taken only
/// on insertion.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct QueryCacheKey(SmallVec<[u64; 8]>);

impl std::borrow::Borrow<[u64]> for QueryCacheKey {
    fn borrow(&self) -> &[u64] {
        &self.0
    }
}

/// Archetype-index-list cache backing `World::query` (the `Query` `Shared`
/// source; incremental, CR-M2).
///
/// The invariants it relies on:
/// - archetypes are append-only (never removed, never change composition) →
///   an entry is extended ONLY by new archetypes from index `seen_arch_count`;
/// - moving an entity between archetypes does NOT invalidate the list: which
///   archetypes match a query is a property of the archetype's composition, not
///   of its rows;
/// - empty archetypes ARE INCLUDED in the list (the consumer skips them during
///   iteration) — otherwise an entity moving into an emptied archetype would be
///   lost.
pub(crate) struct QueryCache {
    entries: RwLock<FxHashMap<QueryCacheKey, CacheEntry>>,
}

impl QueryCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(FxHashMap::default()),
        }
    }

    pub fn get_or_compute(
        &self,
        key: &SmallVec<[u64; 8]>,
        archetypes: &[Archetype],
        matches: impl Fn(&Archetype) -> bool,
    ) -> Arc<[usize]> {
        let total = archetypes.len();

        // Hit path: an entry is current if it has seen all current archetypes.
        // Lookup by &[u64] — without building an owning key.
        {
            // Poison is benign here: the map is an append-only cache of derived
            // data — a panic elsewhere while holding the lock cannot leave it in
            // a logically corrupt state, so recover the guard instead of
            // propagating an unrelated panic across every future query.
            let map = self.entries.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = map.get(key.as_slice()) {
                if entry.seen_arch_count == total {
                    return entry.arch_indices.clone();
                }
            }
        }

        let mut map = self.entries.write().unwrap_or_else(|e| e.into_inner());
        // Double-check: another thread may have extended it between the read and
        // write lock.
        let (mut indices, start) = match map.get(key.as_slice()) {
            Some(entry) if entry.seen_arch_count == total => {
                return entry.arch_indices.clone();
            }
            Some(entry) => (entry.arch_indices.to_vec(), entry.seen_arch_count),
            None => (Vec::new(), 0),
        };

        // Extend only with new archetypes (append-only invariant).
        indices.extend(
            archetypes[start..]
                .iter()
                .enumerate()
                .filter(|(_, arch)| matches(arch))
                .map(|(i, _)| start + i),
        );

        let arch_indices: Arc<[usize]> = indices.into();
        map.insert(
            QueryCacheKey(key.clone()),
            CacheEntry {
                arch_indices: arch_indices.clone(),
                seen_arch_count: total,
            },
        );

        arch_indices
    }

    /// Full invalidation. Not needed in the current model (archetypes are
    /// append-only); it stays as a hook point for despawn compaction (CR-M4),
    /// should that ever appear.
    #[allow(dead_code)]
    pub fn invalidate(&self) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

// ── ArchetypeStats ─────────────────────────────────────────────

/// Summary from [`World::archetype_stats`]: number of archetypes, how many are
/// empty, total live rows, the maximum rows in a single archetype, and memory
/// (W3-5).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchetypeStats {
    pub archetypes: usize,
    pub empty_archetypes: usize,
    pub total_rows: usize,
    pub max_rows_in_archetype: usize,
    /// Allocated for component data (Σ capacity × item_size).
    pub component_bytes: usize,
    /// Allocated for change/added ticks (Σ capacity × 4 × 2).
    pub tick_bytes: usize,
    /// Allocated for archetype entity lists (Σ capacity × 8).
    pub entity_bytes: usize,
}

impl ArchetypeStats {
    /// Total storage memory (components + ticks + entity lists).
    #[inline]
    pub fn total_bytes(&self) -> usize {
        self.component_bytes + self.tick_bytes + self.entity_bytes
    }
}

// ── ArchetypeKey ───────────────────────────────────────────────

/// Key for archetype_index — hashed without a heap allocation.
/// Stores components inline up to 12 of them via SmallVec.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ArchetypeKey(SmallVec<[ComponentId; 12]>);

impl From<&[ComponentId]> for ArchetypeKey {
    fn from(ids: &[ComponentId]) -> Self {
        Self(ids.iter().copied().collect())
    }
}

/// Zero-copy lookup: lets `archetype_index.get(components)` work directly with
/// `&[ComponentId]` without creating a temporary ArchetypeKey.
impl std::borrow::Borrow<[ComponentId]> for ArchetypeKey {
    fn borrow(&self) -> &[ComponentId] {
        &self.0
    }
}

// ── World ──────────────────────────────────────────────────────

/// Generator of unique world ids (for binding [`QueryState`] to a world).
/// Starts at 1: id 0 is reserved as "nobody's" (a fresh QueryState).
static WORLD_ID_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct World {
    /// Unique within the process; `QueryState` checks it so state from one world
    /// is not applied to another (main vs render vs isolated).
    pub(crate) world_id: u64,
    pub(crate) entities: EntityAllocator,
    pub(crate) registry: ComponentRegistry,
    pub(crate) archetypes: Vec<Archetype>,
    pub(crate) archetype_index: FxHashMap<ArchetypeKey, ArchetypeId>,
    /// Index component → list of archetypes containing that component.
    /// Used in Query::new_with_tick for O(1) lookup of candidate archetypes
    /// instead of a linear scan over all archetypes.
    pub(crate) component_arch_index: FxHashMap<ComponentId, SmallVec<[ArchetypeId; 16]>>,
    pub(crate) current_tick: Tick,
    /// The change-detection base for systems: the tick of the previous frame
    /// boundary. `Changed<T>` inside systems compares a row's change-tick against
    /// this value (advanced by the scheduler at the end of the frame /
    /// `advance_change_tick`).
    pub(crate) last_run_tick: Tick,
    pub(crate) query_cache: QueryCache,
    pub(crate) relations: RelationRegistry,
    pub(crate) subject_index: SubjectIndex,
    pub(crate) target_index: TargetIndex,
    pub(crate) resources: Resources,
    pub(crate) events: EventRegistry,
    /// Registry of named templates (EntityTemplate).
    pub(crate) templates: TemplateRegistry,
    /// Chunking configuration for parallel iteration.
    pub(crate) chunk_config: ChunkConfig,
    /// §0.2a policy: what to do with conscious drops/refusals (log/panic/
    /// silent/custom) + anomaly counters. Per-world (see [`crate::error`]).
    pub(crate) error_handler: ErrorHandler,
    /// Composition-hook queue (W3-1): structural operations complete FIRST, then
    /// the dispatcher invokes hooks on a consistent world. Nested structural
    /// operations from hooks append to the same queue — processed by the same
    /// (outer) dispatcher, without recursion.
    pub(crate) hook_queue: Vec<HookEvent>,
    /// The hook dispatcher is already running higher up the stack (re-entrancy
    /// guard).
    pub(crate) hook_dispatch_active: bool,
}

/// A deferred composition event for the hook dispatcher (W3-1).
#[derive(Clone, Copy)]
pub(crate) enum HookEvent {
    Added(Entity, ComponentId),
    Removed(Entity, ComponentId),
    RelationAdded {
        kind_idx: u32,
        subject: Entity,
        target: Entity,
    },
    RelationRemoved {
        kind_idx: u32,
        subject: Entity,
        target: Entity,
    },
}

/// Rolls back a partially-built bulk-spawn batch on unwind (A6).
///
/// Bulk spawn (`spawn_many_inner`, `spawn_bundles_bulk`) pushes entities into
/// `arch.entities` one at a time inside the write loop, but bumps each column's
/// `len` (and tick cells) only once, AFTER the loop. If a user closure
/// (`make_bundle(i)`) or a bundle's `write_data_into_batch` panics mid-loop,
/// `arch.entities.len()` ends up larger than `col.len` — a broken archetype
/// invariant that makes later queries read uninitialized column rows.
///
/// The guard is armed around the loop and disarmed on success. On unwind its
/// `Drop` truncates `arch.entities` back to `start_row` (which equals the
/// untouched `col.len`), restoring the invariant. Column bytes written for
/// completed rows are leaked rather than dropped, which is memory-safe under
/// unwind (the entities' locations were never published).
struct BulkSpawnRollback<'a> {
    world: &'a mut World,
    arch_idx: usize,
    start_row: usize,
    armed: bool,
}

impl Drop for BulkSpawnRollback<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.world.archetypes[self.arch_idx]
                .entities
                .truncate(self.start_row);
        }
    }
}

impl World {
    pub fn new() -> Self {
        let mut registry = ComponentRegistry::new();
        registry.register_all_auto();
        let mut world = Self {
            world_id: WORLD_ID_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            entities: EntityAllocator::new(),
            registry,
            archetypes: Vec::new(),
            archetype_index: FxHashMap::default(),
            component_arch_index: FxHashMap::default(),
            current_tick: Tick(1),
            last_run_tick: Tick::ZERO,
            query_cache: QueryCache::new(),
            relations: RelationRegistry::new(),
            subject_index: SubjectIndex::new(),
            target_index: TargetIndex::new(),
            resources: Resources::new(),
            events: EventRegistry::new(),
            templates: TemplateRegistry::new(),
            chunk_config: ChunkConfig::default(),
            error_handler: ErrorHandler::default(),
            hook_queue: Vec::new(),
            hook_dispatch_active: false,
        };
        world
            .archetypes
            .push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world
            .archetype_index
            .insert(ArchetypeKey(SmallVec::new()), ArchetypeId::EMPTY);
        world
    }

    /// The world's unique id within the process (binds [`QueryState`]).
    #[inline]
    pub fn id(&self) -> u64 {
        self.world_id
    }

    /// Auto-run interval for [`check_change_ticks`](Self::check_change_ticks):
    /// every 2²⁶ ticks (~3 days @250Hz). Must be much smaller than
    /// `2³¹ − Tick::MAX_CHANGE_AGE`, so a tick cannot "wrap around" between clamp
    /// passes.
    const TICK_CHECK_INTERVAL: u32 = 1 << 26;

    /// Advances the global tick. **Does not flush events** — that is the
    /// Scheduler's responsibility. For use without a Scheduler, call
    /// [`flush_all_events()`](Self::flush_all_events) manually.
    pub fn tick(&mut self) {
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
        if self.current_tick.0.is_multiple_of(Self::TICK_CHECK_INTERVAL) {
            self.check_change_ticks();
        }
    }

    /// Advance the change-tick at the frame boundary: remember the current tick
    /// as the `Changed<T>` base for the next frame and increment `current_tick`.
    ///
    /// Called by the scheduler at the end of `run()`/`run_sequential()`. After
    /// this, `Changed<T>` inside systems reliably detects mutations from **this**
    /// frame (rather than "everything"). On the first frame the base is
    /// `Tick::ZERO` (everything new is visible).
    #[inline]
    pub fn advance_change_tick(&mut self) {
        self.last_run_tick = self.current_tick;
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
        if self.current_tick.0.is_multiple_of(Self::TICK_CHECK_INTERVAL) {
            self.check_change_ticks();
        }
    }

    /// Clamp stale change-ticks to the [`Tick::MAX_CHANGE_AGE`] window (W2-3,
    /// analogous to Bevy's `check_change_ticks`).
    ///
    /// `Changed<T>` uses wrapping comparison, correct for a difference < 2³¹: a
    /// row unchanged for longer would become falsely Changed (~99 days of uptime
    /// @250Hz). The clamp pulls such ticks to the window boundary, keeping
    /// "unchanged for a long time" forever. Runs automatically from
    /// [`tick`](Self::tick)/[`advance_change_tick`](Self::advance_change_tick)
    /// once every `TICK_CHECK_INTERVAL`; public for prod servers / editors with
    /// their own loop.
    pub fn check_change_ticks(&mut self) {
        let current = self.current_tick;
        for arch in &mut self.archetypes {
            for col in &mut arch.columns {
                col.check_change_ticks(current);
            }
        }
        self.last_run_tick.check_against(current);
    }

    /// The change-detection base for systems (the tick of the previous frame
    /// boundary).
    #[inline]
    pub fn last_run_tick(&self) -> Tick {
        self.last_run_tick
    }

    /// Set the change-detection base (`Changed<T>`/`Added<T>` compare a row's
    /// change-tick against it). **Internal scheduler API:** it sets this before
    /// each STAGE equal to the tick at which that stage last ran. Together with
    /// advancing `current_tick` between stages ([`tick`]) this gives **cross-stage
    /// change detection**: a write in a late stage of frame N is visible to an
    /// earlier stage of frame N+1 (closing the per-frame-tick blind spot, TD-52).
    /// Direct use outside the scheduler is usually unnecessary.
    #[inline]
    pub fn set_last_run_tick(&mut self, tick: Tick) {
        self.last_run_tick = tick;
    }

    /// Flush specific event types (by TypeId). Used by the Scheduler for
    /// per-Stage flush.
    pub fn flush_events_by_type(&mut self, type_ids: &[std::any::TypeId]) {
        self.events.flush_by_type_id(type_ids);
    }

    /// Flush all events. Used when running without a Scheduler.
    pub fn flush_all_events(&mut self) {
        self.events.flush_all();
    }

    /// End the frame: **flush all events + advance the change-tick**.
    ///
    /// A self-contained replacement for the manual `flush_all_events()` +
    /// `tick()` pair when running **without a scheduler** (#9). Call it once at
    /// the end of each game-loop iteration:
    ///
    /// ```ignore
    /// loop {
    ///     // ... mutations, sending events ...
    ///     world.advance_frame(); // events visible next frame, change-tick++
    /// }
    /// ```
    ///
    /// The scheduler does per-stage flush itself; there it need not be called.
    pub fn advance_frame(&mut self) {
        self.flush_all_events();
        self.advance_change_tick();
    }

    pub fn current_tick(&self) -> Tick {
        self.current_tick
    }
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
    pub fn archetype_count(&self) -> usize {
        self.archetypes.len()
    }
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Get the current chunking configuration.
    #[inline]
    pub fn chunk_config(&self) -> &ChunkConfig {
        &self.chunk_config
    }

    /// Set the chunking configuration.
    #[inline]
    pub fn set_chunk_config(&mut self, config: ChunkConfig) {
        self.chunk_config = config;
    }

    /// The §0.2a error policy for this world (log/panic/silent/custom + anomaly
    /// counters). See [`crate::error`]. Default: [`ErrorMode::Warn`].
    #[inline]
    pub fn error_handler(&self) -> &ErrorHandler {
        &self.error_handler
    }

    /// Mutable access to the error policy — e.g. `world.error_handler_mut().
    /// reset_counts()` before an end-of-frame "zero drops" assertion.
    #[inline]
    pub fn error_handler_mut(&mut self) -> &mut ErrorHandler {
        &mut self.error_handler
    }

    /// Replace the whole error policy (e.g. [`ErrorHandler::from_env`]).
    #[inline]
    pub fn set_error_handler(&mut self, handler: ErrorHandler) {
        self.error_handler = handler;
    }

    /// Shorthand for `world.error_handler_mut().set_mode(mode)` — the common
    /// case of flipping strict/quiet without rebuilding the handler.
    #[inline]
    pub fn set_error_mode(&mut self, mode: ErrorMode) {
        self.error_handler.set_mode(mode);
    }

    /// Remove all entities, preserving resources, registered components, and
    /// events.
    ///
    /// Analogous to `World::clear()` in Bevy. Useful for restarting a level or
    /// resetting a simulation. After the call `entity_count()` returns 0, but
    /// resources and components remain.
    pub fn clear_entities(&mut self) {
        // Collect all entity IDs first to avoid borrow issues
        let entities: Vec<Entity> = self
            .archetypes
            .iter()
            .flat_map(|a| a.entities.iter().copied())
            .collect();
        for entity in entities {
            self.despawn(entity);
        }
    }

    pub fn register_component<T: Component>(&mut self) -> ComponentId {
        self.registry.register::<T>()
    }

    pub fn register_component_serde<T: crate::component::Serializable>(&mut self) -> ComponentId {
        self.registry.register_serde::<T>()
    }

    /// Register a component with JSON serialization support.
    pub fn register_component_serde_json<T: crate::component::Serializable>(
        &mut self,
    ) -> ComponentId {
        self.registry.register_serde_json::<T>()
    }

    /// Register a component with **context-dependent** serde functions (TD-44): a
    /// component with an external reference (asset Handle, Entity reference) is
    /// (de)serialized through [`SerdeContext`](crate::SerdeContext), which is
    /// passed into `WorldSerializer::snapshot_with`/`restore_with`. The resolver
    /// lives in the engine/editor ⇒ apex-ecs stays asset-agnostic. See
    /// [`ComponentRegistry::register_serde_with`].
    pub fn register_component_serde_with<T: Component>(
        &mut self,
        fns: crate::component::ComponentSerdeFns,
    ) -> ComponentId {
        self.registry.register_serde_with::<T>(fns)
    }

    /// Register Entity-reference remapping for component `T` (E6): on snapshot
    /// `restore`, old Entity ids INSIDE `T` (e.g. `Target(Entity)`) are updated to
    /// the new ones by a second restore pass via [`MapEntities::map_entities`].
    /// Without this, Entity references point into the void after restore.
    pub fn register_map_entities<T: Component + crate::component::MapEntities>(
        &mut self,
    ) -> ComponentId {
        let id = self.registry.get_or_register::<T>();
        let map_fn: crate::component::MapEntitiesFn = |ptr, f| {
            // SAFETY: `ptr` is a valid `*mut T` of a live component (restore contract).
            let val = unsafe { &mut *(ptr as *mut T) };
            val.map_entities(f);
        };
        self.registry.set_map_entities(id, map_fn);
        id
    }

    /// E6: remap Entity references inside all of `entity`'s components that
    /// registered a [`MapEntitiesFn`](crate::component::MapEntitiesFn), via `f`.
    /// Called by `restore` AFTER the full old→new map is built (including forward
    /// references). No-op for a dead entity / components without remapping.
    pub fn map_entity_refs(&mut self, entity: Entity, f: &mut dyn FnMut(Entity) -> Entity) {
        let Some(loc) = self.entities.get_location(entity) else {
            return;
        };
        let arch_idx = loc.archetype_id.0 as usize;
        let row = loc.row as usize;
        // Snapshot (component_id, map_fn) first — avoids holding a shared borrow
        // of the archetype while taking a mutable one below.
        let mapped: SmallVec<[(ComponentId, crate::component::MapEntitiesFn); 8]> = self.archetypes
            [arch_idx]
            .component_ids
            .iter()
            .filter_map(|&cid| {
                self.registry
                    .get_info(cid)
                    .and_then(|i| i.map_entities)
                    .map(|mf| (cid, mf))
            })
            .collect();
        for (cid, map_fn) in mapped {
            let arch = &mut self.archetypes[arch_idx];
            if let Some(col_idx) = arch.column_index(cid) {
                // SAFETY: `row` is live in this archetype; `map_fn` was registered
                // for the type stored in column `cid`.
                let ptr = unsafe { arch.columns[col_idx].get_ptr(row) };
                unsafe { map_fn(ptr, f) };
            }
        }
    }

    // ── Composition hooks (W3-1) ───────────────────────────────

    /// Register an `on_add` hook for component `T`: called after `T` APPEARED on
    /// an entity (spawn / insert of a new one; replacing the value of an existing
    /// component does NOT fire the hook — that is `Changed`, not `Added`).
    ///
    /// The hook is invoked on a consistent world (after the structural operation
    /// completes) and may perform any operations, including structural ones.
    /// One hook per component; re-registration panics (for multiple subscribers
    /// use events).
    pub fn on_add<T: Component>(&mut self, hook: crate::component::ComponentHookFn) {
        let cid = self.registry.get_or_register::<T>();
        let hooks = self.registry.hooks_mut(cid);
        assert!(
            hooks.on_add.is_none(),
            "on_add hook for `{}` is already registered (one hook per component; \
             use events for multiple subscribers)",
            std::any::type_name::<T>()
        );
        hooks.on_add = Some(hook);
        self.registry.set_flag(cid, crate::component::FLAG_ON_ADD);
    }

    /// Register an `on_remove` hook for component `T`: called after an entity
    /// LOST `T` (`remove` or `despawn` — in the latter case the entity is already
    /// dead, `is_alive == false`). The component value is already destroyed by the
    /// time of the call — the hook receives only the entity.
    ///
    /// One hook per component; re-registration panics.
    pub fn on_remove<T: Component>(&mut self, hook: crate::component::ComponentHookFn) {
        let cid = self.registry.get_or_register::<T>();
        let hooks = self.registry.hooks_mut(cid);
        assert!(
            hooks.on_remove.is_none(),
            "on_remove hook for `{}` is already registered (one hook per component; \
             use events for multiple subscribers)",
            std::any::type_name::<T>()
        );
        hooks.on_remove = Some(hook);
        self.registry.set_flag(cid, crate::component::FLAG_ON_REMOVE);
    }

    /// Declare: component `C` requires `R` (D2-4, analogous to Bevy's
    /// `#[require]`).
    ///
    /// When `C` appears on an entity (spawn / insert), a missing `R` is pulled in
    /// via `R::default()` — an explicitly given value always wins. Requirements
    /// are transitive (if `R` itself requires something). For derive types the
    /// attribute is more convenient: `#[derive(Component)]
    /// #[require(LocalTransform)]`.
    ///
    /// ```ignore
    /// world.require_component::<MeshRenderer, LocalTransform>();
    /// world.require_component::<MeshRenderer, GlobalTransform>();
    /// let e = world.spawn((MeshRenderer::new(mesh, mat),)); // transforms pulled in
    /// ```
    pub fn require_component<C: Component, R: Component + Default>(&mut self) {
        self.registry.register_required::<C, R>();
    }

    /// Enable emission of [`Removed<T>`](crate::events::Removed) events when
    /// component `T` is lost (remove/despawn) — analogous to Bevy's
    /// `RemovedComponents`. Read via the usual event paths (`&[Removed<T>]` in
    /// `system!`, `event_reader`); per-reader cursors exclude duplicates.
    ///
    /// Idempotent. For non-enabled types, removals are not recorded (zero cost).
    pub fn track_removals<T: Component>(&mut self) {
        let cid = self.registry.get_or_register::<T>();
        self.events.register::<crate::events::Removed<T>>();
        self.registry.hooks_mut(cid).emit_removed = Some(|events, entity| {
            events
                .get_or_register_mut::<crate::events::Removed<T>>()
                .send(crate::events::Removed::new(entity));
        });
        self.registry
            .set_flag(cid, crate::component::FLAG_TRACK_REMOVED);
    }

    /// Register an `on_add` hook for relation kind `R`: called after a successful
    /// `add_relation` with `(subject, target)`.
    /// One hook per kind; re-registration panics.
    pub fn on_relation_add<R: crate::relations::RelationKind>(
        &mut self,
        hook: crate::relations::RelationHookFn,
    ) {
        let kind_idx = self.relations.get_or_register::<R>();
        self.relations.set_on_add(kind_idx, hook);
    }

    /// Register an `on_remove` hook for relation kind `R`: called after a pair
    /// disappears — an explicit `remove_relation` OR cleanup on despawn of the
    /// subject/target (including cascade; the entities may be dead by then). One
    /// hook per kind; re-registration panics.
    pub fn on_relation_remove<R: crate::relations::RelationKind>(
        &mut self,
        hook: crate::relations::RelationHookFn,
    ) {
        let kind_idx = self.relations.get_or_register::<R>();
        self.relations.set_on_remove(kind_idx, hook);
    }

    /// Hook dispatcher: called at the END of public structural operations.
    /// Fast path (no subscribers / queue empty) — a single check.
    #[inline]
    pub(crate) fn flush_hooks(&mut self) {
        if self.hook_queue.is_empty() || self.hook_dispatch_active {
            return;
        }
        self.flush_hooks_slow();
    }

    #[cold]
    fn flush_hooks_slow(&mut self) {
        // RAII: reset the dispatch flag and drop any undelivered hooks on the way
        // out — INCLUDING a panic in a user hook. Otherwise a panicking hook would
        // leave `hook_dispatch_active` set forever, silently disabling all future
        // hook dispatch and letting the queue grow unbounded (A7).
        struct DispatchGuard<'a> {
            world: &'a mut World,
        }
        impl Drop for DispatchGuard<'_> {
            fn drop(&mut self) {
                self.world.hook_queue.clear();
                self.world.hook_dispatch_active = false;
            }
        }

        self.hook_dispatch_active = true;
        let guard = DispatchGuard { world: self };
        let world = &mut *guard.world;

        let mut i = 0;
        // Hooks may append events to the tail of the queue (nested structural
        // operations) — a plain while over a growing Vec, without recursion.
        while i < world.hook_queue.len() {
            let ev = world.hook_queue[i];
            i += 1;
            match ev {
                HookEvent::Added(entity, cid) => {
                    // Required components (D2-4) — BEFORE the user's on_add: the
                    // hook sees the entity already with its full composition.
                    // Transitive requires go through this same queue (inserting R
                    // queues its own Added event).
                    if world.registry.flags(cid) & crate::component::FLAG_REQUIRES != 0 {
                        let fns: SmallVec<[crate::component::RequiredInsertFn; 4]> = world
                            .registry
                            .requires(cid)
                            .map(|s| s.iter().copied().collect())
                            .unwrap_or_default();
                        for f in fns {
                            f(world, entity);
                        }
                    }
                    let hook = world.registry.hooks(cid).and_then(|h| h.on_add);
                    if let Some(f) = hook {
                        f(world, entity);
                    }
                }
                HookEvent::Removed(entity, cid) => {
                    let hook = world.registry.hooks(cid).and_then(|h| h.on_remove);
                    if let Some(f) = hook {
                        f(world, entity);
                    }
                }
                HookEvent::RelationAdded {
                    kind_idx,
                    subject,
                    target,
                } => {
                    if let Some(f) = world.relations.on_add_hook(kind_idx) {
                        f(world, subject, target);
                    }
                }
                HookEvent::RelationRemoved {
                    kind_idx,
                    subject,
                    target,
                } => {
                    if let Some(f) = world.relations.on_remove_hook(kind_idx) {
                        f(world, subject, target);
                    }
                }
            }
        }
        // `guard` drops here → clears the queue and resets the flag (also on panic).
    }

    /// Queue `Added` hooks for a freshly created entity by the list of its
    /// components (the caller has already checked `registry.any_flags()`).
    fn queue_added_hooks(&mut self, entity: Entity, ids: &[ComponentId]) {
        for &cid in ids {
            if self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0 {
                self.hook_queue.push(HookEvent::Added(entity, cid));
            }
        }
    }

    /// Notifications about component LOSS: queue the `on_remove` hook + immediate
    /// emission of the `Removed<T>` event (the caller has already checked
    /// `registry.any_flags()`).
    fn notify_removed(&mut self, entity: Entity, cid: ComponentId) {
        let flags = self.registry.flags(cid);
        if flags & crate::component::FLAG_ON_REMOVE != 0 {
            self.hook_queue.push(HookEvent::Removed(entity, cid));
        }
        if flags & crate::component::FLAG_TRACK_REMOVED != 0 {
            let emit = self.registry.hooks(cid).and_then(|h| h.emit_removed);
            if let Some(f) = emit {
                f(&mut self.events, entity);
            }
        }
    }

    pub fn registry(&self) -> &ComponentRegistry {
        &self.registry
    }

    /// Mutable access to the component registry.
    pub fn registry_mut(&mut self) -> &mut ComponentRegistry {
        &mut self.registry
    }

    pub fn archetypes(&self) -> &[Archetype] {
        &self.archetypes
    }

    /// Archetype summary — debug / profiling (CR-M4).
    ///
    /// Empty archetypes are not reused for another composition and are not
    /// compacted (the append-only invariant is cheaper; a slot for a MATCHING
    /// composition is reused via archetype_index). This summary is a tool for
    /// observing their count.
    pub fn archetype_stats(&self) -> ArchetypeStats {
        let mut stats = ArchetypeStats {
            archetypes: self.archetypes.len(),
            ..Default::default()
        };
        for arch in &self.archetypes {
            let rows = arch.len();
            stats.total_rows += rows;
            if rows == 0 {
                stats.empty_archetypes += 1;
            }
            stats.max_rows_in_archetype = stats.max_rows_in_archetype.max(rows);
            stats.entity_bytes += arch.entities.capacity() * std::mem::size_of::<Entity>();
            for col in &arch.columns {
                let (data, ticks) = col.allocated_bytes();
                stats.component_bytes += data;
                stats.tick_bytes += ticks;
            }
        }
        stats
    }

    pub fn relation_registry(&self) -> &RelationRegistry {
        &self.relations
    }

    pub fn relation_registry_mut(&mut self) -> &mut RelationRegistry {
        &mut self.relations
    }

    /// Insert a component's raw serialized bytes by [`ComponentId`] (dynamic:
    /// the component type is not known statically — used on snapshot/prefab
    /// restore). See the `_dyn` naming canon in `docs/CONVENTIONS.md`.
    #[inline]
    pub fn insert_dyn(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        data: Vec<u8>,
        tick: Tick,
    ) {
        self.insert_raw(entity, component_id, data, tick);
    }

    // ── Parallel access ────────────────────────────────────────

    /// # Safety
    /// The caller guarantees the absence of structural changes and the
    /// correctness of the AccessDescriptor of all parallel systems.
    pub unsafe fn as_parallel_world(&self) -> ParallelWorld<'_> {
        ParallelWorld {
            world: self as *const World,
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

    /// E7: include resource `R` in the snapshot (opt-in, bincode). After this,
    /// `WorldSerializer::snapshot` saves the present resource `R`, and `restore`
    /// restores it. Without registration, resources do NOT go into the snapshot
    /// (the world may contain non-serializable resources — GPU handles, etc.).
    pub fn register_resource_serde<
        R: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    >(
        &mut self,
    ) {
        self.resources.register_serde::<R>();
    }

    /// Snapshot every resource registered via
    /// [`register_resource_serde`](Self::register_resource_serde) as
    /// `(type_name, bytes)` pairs. Consumed by `WorldSerializer`.
    pub fn snapshot_resources_serde(&self) -> Vec<(String, Vec<u8>)> {
        self.resources.snapshot_serde()
    }

    /// Restore one resource from its serde bytes. `Ok(true)` = applied,
    /// `Ok(false)` = type not registered for serde on this world (caller warns,
    /// §0.2a). Consumed by `WorldSerializer`.
    pub fn restore_resource_serde(&mut self, type_name: &str, bytes: &[u8]) -> Result<bool, String> {
        self.resources.restore_serde(type_name, bytes)
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
    pub fn events<T: Send + Sync + 'static>(&self) -> &crate::events::Events<T> {
        self.events.get::<T>()
    }

    #[track_caller]
    pub fn events_mut<T: Send + Sync + 'static>(&mut self) -> &mut crate::events::Events<T> {
        self.events.get_mut::<T>()
    }

    /// Send an event.
    ///
    /// If the event type is not yet registered, it is registered automatically
    /// (a call to `world.add_event::<T>()` is not required).
    pub fn send_event<T: Send + Sync + 'static>(&mut self, event: T) {
        self.events.get_or_register_mut::<T>().send(event);
    }

    /// Pre-allocate capacity for events of the given type.
    ///
    /// Avoids repeated reallocations when sending events in bulk within one tick.
    /// Call before the send loop.
    pub fn event_reserve<T: Send + Sync + 'static>(&mut self, capacity: usize) {
        self.events.get_or_register_mut::<T>().reserve(capacity);
    }

    /// Reserve capacity for events by TypeId.
    ///
    /// Called by the scheduler based on `AccessDescriptor::event_reserve()`.
    pub fn event_reserve_by_type(&mut self, type_id: TypeId, capacity: usize) {
        self.events.reserve_by_type(type_id, capacity);
    }

    pub(crate) fn event_queue_ptr<T: Send + Sync + 'static>(
        &self,
    ) -> Option<*mut crate::events::Events<T>> {
        self.events.get_raw_ptr::<T>()
    }

    /// Create an event reader with a per-reader cursor.
    ///
    /// Analogous to `EventReader::new(world.events_mut::<T>())`.
    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        unsafe {
            let ptr = self
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    /// Create an event writer.
    ///
    /// Analogous to `EventWriter::from_ptr(...)`.
    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered");
            EventWriter::from_ptr(ptr)
        }
    }

    // ── Spawn ──────────────────────────────────────────────────

    /// Create an entity from a Bundle.
    ///
    /// For an empty entity (no components) use `spawn(())`.
    /// For a single component — `spawn((MyComponent,))`.
    /// For several — `spawn((A, B, C))`.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
        let entity = self.entities.allocate();
        self.spawn_at(entity, bundle);
        entity
    }

    /// Entity reserver descriptor (shares the atomic high-water with the
    /// allocator). Cloned into [`Commands`](crate::commands::Commands) so that
    /// `commands.spawn().id()` yields a real `Entity` from a parallel system (1:1
    /// Bevy `Entities::reserve_entity`).
    #[inline]
    pub fn entity_reserver(&self) -> crate::entity::EntityReserver {
        self.entities.reserver()
    }

    /// D8b: reserve a deterministic reuse-aware block of `size` entity indices
    /// (freed slots first, then fresh ones) and return a bound reserver. The
    /// scheduler calls this on the main thread in the rank order of the stage's
    /// systems → block bases/slices are deterministic, and reuse bounds the id
    /// space under churn (see [`EntityReserver`],
    /// [`reclaim_entity_block_tail`](Self::reclaim_entity_block_tail)).
    #[inline]
    pub fn reserve_entity_block(&self, size: u32) -> crate::entity::EntityReserver {
        self.entities.reserve_block(size)
    }

    /// D8b: return the unused tail of a block to the reuse pool (indices reserved
    /// by the block but not spawned). The scheduler calls this AFTER applying the
    /// stage's commands (`flush` grew the records), in rank order → deterministic
    /// reuse and a bounded id space under despawn+respawn churn.
    #[inline]
    pub fn reclaim_entity_block_tail(&mut self, unused: &[crate::entity::Entity]) {
        self.entities.reclaim_block_tail(unused);
    }

    /// Materialize records for all indices reserved via
    /// [`World::entity_reserver`]. Called by
    /// [`Commands::apply`](crate::commands::Commands::apply) before processing the
    /// queue, before spawn commands set components on reserved entities.
    /// Idempotent / cheap.
    #[inline]
    pub fn flush_reserved(&mut self) {
        self.entities.flush();
    }

    /// Spawn components onto an ALREADY reserved (via [`World::entity_reserver`])
    /// entity — the `commands.spawn().id()` path. Semantically identical to
    /// [`World::spawn`], but does not allocate a new id; it fills the passed one
    /// (its record is guaranteed by [`World::flush_reserved`] at the apply
    /// boundary).
    #[inline]
    pub fn spawn_reserved<B: Bundle>(&mut self, entity: Entity, bundle: B) {
        self.spawn_at(entity, bundle);
    }

    /// Shared spawn body: fill a SPECIFIC entity with the components of `bundle`.
    /// The entity's record is created if needed (`ensure_record`) — required for
    /// reserved ids that ran ahead of the flush; for a direct `spawn` (the
    /// allocator already created the record) this is a no-op.
    fn spawn_at<B: Bundle>(&mut self, entity: Entity, bundle: B) {
        // §0.2a (A10): spawning onto an entity that is ALREADY located would
        // orphan its current row (a phantom duplicate that later iterations walk
        // over uninitialised/stale storage). Reserved-but-unflushed ids are
        // location-less, so they pass; only a genuinely live entity trips this.
        // In debug it is a hard error (developer bug); in release we refuse
        // loudly and drop `bundle` rather than corrupt storage.
        if self.entities.get_location(entity).is_some() {
            #[cfg(debug_assertions)]
            panic!(
                "spawn_at on already-live entity {}:{} — would orphan its row",
                entity.index, entity.generation,
            );
            #[cfg(not(debug_assertions))]
            {
                crate::anomaly!(
                    self, crate::Severity::Warn, "World::spawn_at",
                    Some(entity), None,
                    "already-live entity refused (bundle dropped) to avoid orphaning its row"
                );
                return;
            }
        }
        self.entities.ensure_record(entity.index());
        let ids = bundle.component_ids(&mut self.registry);
        if ids.is_empty() {
            // Fast path for an empty entity (spawn(()))
            let row = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
            self.entities.set_location(
                entity,
                EntityLocation {
                    archetype_id: ArchetypeId::EMPTY,
                    row,
                },
            );
            return;
        }
        // Normal path
        let archetype_id = self.get_or_create_archetype(&ids);
        let row = self.archetypes[archetype_id.0 as usize].entities.len();
        let tick = self.current_tick;
        self.archetypes[archetype_id.0 as usize]
            .entities
            .push(entity);
        bundle.write_into(self, archetype_id, row, tick);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id,
                row: row as u32,
            },
        );
        if self.registry.any_flags() {
            self.queue_added_hooks(entity, &ids);
            self.flush_hooks();
        }
    }

    /// Internal shared method for `spawn_many` / `spawn_many_silent`.
    /// Always returns `Vec<Entity>`; the public wrappers decide whether to return
    /// it or ignore it.
    fn spawn_many_inner<B, F>(&mut self, count: usize, mut make_bundle: F) -> Vec<Entity>
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        if count == 0 {
            return Vec::new();
        }

        // `decl_ids` — the bundle's DECLARATION order (= `write_into_batch`
        // traversal order); `ids` — SORTED (for the archetype). Their SEPARATION
        // is critical: col_indices MUST be in traversal order, otherwise a
        // component is written to the wrong column (UB). §10.10: the composition
        // is taken STATICALLY (by the type `B`), without a `make_bundle(0)` probe
        // — the closure is no longer called an extra time for composition and need
        // not be pure.
        let mut decl_ids: SmallVec<[ComponentId; 8]> = SmallVec::new();
        B::static_component_ids(&mut self.registry, &mut decl_ids);
        let mut ids = decl_ids.clone();
        ids.sort_unstable();

        let archetype_id = self.get_or_create_archetype(&ids);
        let arch_idx = archetype_id.0 as usize;
        let start_row = self.archetypes[arch_idx].entities.len();
        let tick = self.current_tick;

        self.archetypes[arch_idx].entities.reserve(count);
        for col in &mut self.archetypes[arch_idx].columns {
            col.reserve(count);
        }

        let entities = self.entities.allocate_batch(count);

        // Precompute column indices in DECLARATION order (`decl_ids`) — EXACTLY as
        // `write_into_batch` consumes them. Avoids repeated
        // get_or_register/column_index in write_into for each entity (~40k HashMap
        // lookups at 10k). CRITICALLY from `decl_ids`, NOT from the sorted `ids`
        // (otherwise, when "declaration order ≠ id order", a write goes to the
        // wrong column).
        let col_indices: SmallVec<[usize; 8]> = decl_ids
            .iter()
            .filter_map(|&id| self.archetypes[arch_idx].column_index(id))
            .collect();

        // ALWAYS per-entity: `make_bundle(i)` is called for EACH entity (the
        // closure contract is per-index data). The former bulk-copy "copy row 0 to
        // all" WAS INCORRECT: it called `make_bundle` only for row 0 ⇒
        // `spawn_many(n, |i| A(i))` silently gave ALL of them A(0) (loss of
        // per-entity data). `col_indices` (traversal order) makes the per-column
        // write correct. Perf: we write DATA per-entity (`write_data_into_batch`,
        // without ticks/len), and set ticks/`len` PER-COLUMN once per batch (resize
        // instead of count×ncols pushes).
        {
            // A6: if `make_bundle(i)` panics mid-loop, roll the archetype back to
            // a consistent state (entities.len() == col.len) instead of leaving
            // ghost entity rows over uninitialized column memory.
            let mut guard = BulkSpawnRollback {
                world: &mut *self,
                arch_idx,
                start_row,
                armed: true,
            };
            {
                let world = &mut *guard.world;
                for (i, &entity) in entities.iter().enumerate() {
                    let row = start_row + i;
                    let bundle = make_bundle(i);
                    world.archetypes[arch_idx].entities.push(entity);
                    bundle.write_data_into_batch(world, archetype_id, row, tick, &col_indices);
                }
            }
            guard.armed = false;
            drop(guard);
            // Ticks + len — PER-COLUMN, to the ABSOLUTE target (start_row+count).
            // This is robust to BOTH write paths: for data-only overrides
            // (leaf/tuple/derive) it fills count new slots; for the default (manual
            // impl → write_into_batch already set ticks/len) it is a no-op.
            let target_len = start_row + count;
            let arch = &mut self.archetypes[arch_idx];
            for &col_idx in &col_indices {
                let col = &mut arch.columns[col_idx];
                col.change_ticks
                    .resize_with(target_len, || TickCell::new(tick));
                col.added_ticks
                    .resize_with(target_len, || TickCell::new(tick));
                col.len = target_len;
            }
        }

        self.entities
            .set_locations_batch(&entities, archetype_id, start_row as u32);

        if self.registry.any_flags() {
            let flagged: SmallVec<[ComponentId; 8]> = ids
                .iter()
                .copied()
                .filter(|&cid| self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0)
                .collect();
            if !flagged.is_empty() {
                for &entity in &entities {
                    for &cid in &flagged {
                        self.hook_queue.push(HookEvent::Added(entity, cid));
                    }
                }
                self.flush_hooks();
            }
        }
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

    /// Bulk-spawn a batch of entities with PER-ELEMENT bundles of a single type
    /// `B` into ONE archetype with a single resolve
    /// (`component_ids`/archetype/columns — once per batch, NOT per spawn). The
    /// `Commands::apply` path for consecutive same-type spawn commands (see
    /// `spawn_apply_batch`): removes the per-spawn `spawn_at` tax (10k archetype
    /// lookups for 10k spawns). `entities` — either all reserved (the system path;
    /// their records were materialized by a preceding `flush_reserved`), or all
    /// `PLACEHOLDER` (standalone `Commands` — then the ids are allocated here with
    /// a single `allocate_batch`).
    pub(crate) fn spawn_bundles_bulk<B: Bundle>(&mut self, entities: Vec<Entity>, bundles: Vec<B>) {
        let count = bundles.len();
        if count == 0 {
            return;
        }
        debug_assert_eq!(entities.len(), count);

        // PLACEHOLDER (standalone) ⇒ allocate fresh ids; otherwise take the
        // reserved ones.
        let placeholder = entities[0] == Entity::PLACEHOLDER;
        debug_assert!(
            entities
                .iter()
                .all(|&e| (e == Entity::PLACEHOLDER) == placeholder),
            "a batch of spawn commands from one Commands is homogeneous in reserver presence"
        );
        let entities: Vec<Entity> = if placeholder {
            self.entities.allocate_batch(count)
        } else {
            for &e in &entities {
                self.entities.ensure_record(e.index());
            }
            entities
        };

        // Resolve archetype/ids/columns — ONCE per batch (instead of per-spawn in
        // spawn_at). `decl_ids` (declaration order = write_into_batch traversal
        // order) SEPARATE from `ids` (sorted for the archetype) — col_indices is
        // built from decl_ids, otherwise a component is written to the wrong column
        // (UB).
        let mut decl_ids: SmallVec<[ComponentId; 8]> = SmallVec::new();
        B::static_component_ids(&mut self.registry, &mut decl_ids);
        let mut ids = decl_ids.clone();
        ids.sort_unstable();
        if ids.is_empty() {
            // Empty bundle (`spawn(())`) — into the EMPTY archetype.
            for (i, _bundle) in bundles.into_iter().enumerate() {
                let entity = entities[i];
                let row = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
                self.entities.set_location(
                    entity,
                    EntityLocation {
                        archetype_id: ArchetypeId::EMPTY,
                        row,
                    },
                );
            }
            return;
        }
        let archetype_id = self.get_or_create_archetype(&ids);
        let arch_idx = archetype_id.0 as usize;
        let start_row = self.archetypes[arch_idx].entities.len();
        let tick = self.current_tick;
        self.archetypes[arch_idx].entities.reserve(count);
        for col in &mut self.archetypes[arch_idx].columns {
            col.reserve(count);
        }
        let col_indices: SmallVec<[usize; 8]> = decl_ids
            .iter()
            .filter_map(|&id| self.archetypes[arch_idx].column_index(id))
            .collect();

        // Bundles are DIFFERENT per item ⇒ write the DATA of each via
        // write_data_into_batch (with precomputed col_indices in traversal order —
        // without repeated get_or_register / archetype lookup); ticks/len —
        // per-column to the absolute target (robust to data-only override and the
        // default).
        {
            // A6: a custom `Bundle::write_data_into_batch` that panics mid-loop
            // must not leave ghost entity rows over uninitialized column memory.
            let mut guard = BulkSpawnRollback {
                world: &mut *self,
                arch_idx,
                start_row,
                armed: true,
            };
            {
                let world = &mut *guard.world;
                for (i, bundle) in bundles.into_iter().enumerate() {
                    let entity = entities[i];
                    let row = start_row + i;
                    world.archetypes[arch_idx].entities.push(entity);
                    bundle.write_data_into_batch(world, archetype_id, row, tick, &col_indices);
                }
            }
            guard.armed = false;
            drop(guard);
        }
        let target_len = start_row + count;
        for &col_idx in &col_indices {
            let col = &mut self.archetypes[arch_idx].columns[col_idx];
            col.change_ticks
                .resize_with(target_len, || TickCell::new(tick));
            col.added_ticks
                .resize_with(target_len, || TickCell::new(tick));
            col.len = target_len;
        }
        self.entities
            .set_locations_batch(&entities, archetype_id, start_row as u32);

        if self.registry.any_flags() {
            let flagged: SmallVec<[ComponentId; 8]> = ids
                .iter()
                .copied()
                .filter(|&cid| self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0)
                .collect();
            if !flagged.is_empty() {
                for &entity in &entities {
                    for &cid in &flagged {
                        self.hook_queue.push(HookEvent::Added(entity, cid));
                    }
                }
                self.flush_hooks();
            }
        }
    }

    /// Create entities from an iterator of bundles (like Bevy's `spawn_batch`).
    ///
    /// Lets you spawn entities with different component sets in one batch:
    ///
    /// ```rust
    /// # use apex_core::prelude::*;
    /// # let mut world = World::new();
    /// # #[derive(Component)] struct Health(f32);
    /// # #[derive(Component)] struct Armor(f32);
    /// world.spawn_batch([
    ///     (Health(100.0), Armor(10.0)),
    ///     (Health(50.0),  Armor(5.0)),
    /// ]);
    /// ```
    ///
    /// Internally collects the iterator into a `Vec` and calls `spawn` for each
    /// element. For bulk-spawning **identical** bundles use [`spawn_many`] — it is
    /// optimized via bulk-copy.
    pub fn spawn_batch<I>(&mut self, iter: I) -> Vec<Entity>
    where
        I: IntoIterator,
        I::Item: Bundle,
    {
        let items: Vec<I::Item> = iter.into_iter().collect();
        let mut entities = Vec::with_capacity(items.len());
        for bundle in items {
            entities.push(self.spawn(bundle));
        }
        entities
    }

    // ── Component ops ──────────────────────────────────────────

    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) {
        let component_id = self.registry.get_or_register::<T>();
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => {
                // §0.2a: inserting into a dead entity is a no-op — `component` is
                // dropped here (no leak), but the write is silently lost. Surface
                // it so the caller learns the entity was already despawned.
                crate::anomaly!(
                    self, crate::Severity::Warn, "World::insert",
                    Some(entity), Some(std::any::type_name::<T>()),
                    "component dropped, not inserted (entity already despawned)"
                );
                return;
            }
        };
        let current_idx = location.archetype_id.0 as usize;

        if self.archetypes[current_idx].has_component(component_id) {
            let tick = self.current_tick;
            unsafe {
                if let Some(col_idx) = self.archetypes[current_idx].column_index(component_id) {
                    let col = &mut self.archetypes[current_idx].columns[col_idx];
                    // replace_at drops the OLD value (W2-1: write_at silently lost
                    // it — a leak for Drop types: String, Vec, Arc…).
                    col.replace_at(
                        location.row as usize,
                        &component as *const T as *const u8,
                        tick,
                    );
                }
            }
            std::mem::forget(component);
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        let tick = self.current_tick;
        unsafe {
            self.archetypes[new_arch_id.0 as usize].write_component(
                new_row as usize,
                component_id,
                &component as *const T as *const u8,
                tick,
            );
        }
        std::mem::forget(component);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
        if self.registry.any_flags()
            && self.registry.flags(component_id) & crate::component::ADDED_NOTIFY_MASK != 0
        {
            self.hook_queue.push(HookEvent::Added(entity, component_id));
            self.flush_hooks();
        }
    }

    /// Insert a component from raw data.
    ///
    /// `data` — the byte representation of an owned `T` (the source is
    /// `forget`-ten by the caller). The length MUST match the component size.
    pub(crate) fn insert_raw(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        data: Vec<u8>,
        tick: Tick,
    ) {
        // The raw bytes must match the component's storage layout exactly — a
        // short buffer would make the column copy read past the Vec (OOB) (B8).
        let info_size = self
            .registry
            .get_info(component_id)
            .map(|i| i.size)
            .unwrap_or_else(|| panic!("insert_raw: component {component_id:?} is not registered"));
        assert!(
            data.len() == info_size,
            "insert_raw: data length {} != component size {info_size} for {component_id:?}",
            data.len(),
        );
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => {
                // Entity is dead: `data` is a moved-out `T`, so dropping the raw
                // `Vec<u8>` would free the buffer without running `T::drop`,
                // leaking owned fields (String/Arc/Vec). Run drop_fn so ownership
                // is honored exactly once (A9); the column never received a copy.
                // §0.2a: the insert is silently lost — surface the dead-entity hit.
                crate::anomaly!(
                    self, crate::Severity::Warn, "World::insert_raw",
                    Some(entity), None,
                    "data for {component_id:?} dropped, not inserted (entity already despawned)"
                );
                if !data.is_empty() {
                    if let Some(info) = self.registry.get_info(component_id) {
                        // `data` is a `Vec<u8>` (alignment 1), but `drop_fn` runs
                        // `drop_in_place::<T>`, which requires a `T`-aligned
                        // pointer — dropping straight off the `Vec` is unaligned
                        // UB for any `T` with `align > 1`. Move the bytes into a
                        // correctly-aligned scratch allocation and drop there.
                        let layout =
                            std::alloc::Layout::from_size_align(info.size, info.align).unwrap();
                        unsafe {
                            let aligned = std::alloc::alloc(layout);
                            assert!(!aligned.is_null(), "insert_raw: allocation failed");
                            std::ptr::copy_nonoverlapping(data.as_ptr(), aligned, info.size);
                            // Ownership now lives in `aligned`; drop it exactly
                            // once. The original `data` buffer is freed as raw
                            // bytes when it drops at end of scope (no `T::drop`).
                            (info.drop_fn)(aligned);
                            std::alloc::dealloc(aligned, layout);
                        }
                    }
                }
                return;
            }
        };
        let current_idx = location.archetype_id.0 as usize;

        if self.archetypes[current_idx].has_component(component_id) {
            if !data.is_empty() {
                unsafe {
                    if let Some(col_idx) = self.archetypes[current_idx].column_index(component_id) {
                        let col = &mut self.archetypes[current_idx].columns[col_idx];
                        // replace_at: drop of the old value (see W2-1).
                        col.replace_at(location.row as usize, data.as_ptr(), tick);
                    }
                }
            }
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        unsafe {
            self.archetypes[new_arch_id.0 as usize].write_component(
                new_row as usize,
                component_id,
                data.as_ptr(),
                tick,
            );
        }
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
        if self.registry.any_flags()
            && self.registry.flags(component_id) & crate::component::ADDED_NOTIFY_MASK != 0
        {
            self.hook_queue.push(HookEvent::Added(entity, component_id));
            self.flush_hooks();
        }
    }

    /// Batch insertion of components for one entity (W2-1): ONE archetype move
    /// for the whole batch instead of a move per component. Used by
    /// `Commands::apply` for bursts of `insert` on a single entity.
    ///
    /// `parts` — (ComponentId, pointer to value, tick). Values are TRANSFERRED BY
    /// OWNERSHIP (byte copy into the column; the caller must `forget` the source /
    /// not drop the bytes). Already-existing components are overwritten with a
    /// drop of the old value; duplicates in the batch are applied in order (the
    /// last one survives, intermediate ones are dropped).
    ///
    /// Returns `false` (nothing written) if the entity is dead — the caller must
    /// free the payloads itself.
    pub(crate) fn insert_parts(
        &mut self,
        entity: Entity,
        parts: &[(ComponentId, *const u8, Tick)],
    ) -> bool {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => {
                // §0.2a: batch insert onto a dead entity — the caller owns the
                // payloads and frees them (see `apply_insert_group`), but the
                // write is lost. Surface the dead-entity hit.
                crate::anomaly!(
                    self, crate::Severity::Warn, "World::insert_parts",
                    Some(entity), None,
                    "{} component(s) dropped, not inserted (entity already despawned)",
                    parts.len()
                );
                return false;
            }
        };

        // Final archetype — via a chain of add_edges, WITHOUT moving data. Along
        // the way we collect FRESHLY added components with an on_add subscription
        // (a duplicate in the batch does not reach here a second time — the
        // component is already in the target composition).
        let any_flags = self.registry.any_flags();
        let mut added_hooked: SmallVec<[ComponentId; 8]> = SmallVec::new();
        let mut target = location.archetype_id;
        for &(cid, _, _) in parts {
            if !self.archetypes[target.0 as usize].has_component(cid) {
                target = self.find_or_create_archetype_with(target, cid);
                if any_flags && self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0 {
                    added_hooked.push(cid);
                }
            }
        }

        let row = if target != location.archetype_id {
            let new_row = self.move_entity(entity, location, target);
            self.entities.set_location(
                entity,
                EntityLocation {
                    archetype_id: target,
                    row: new_row,
                },
            );
            new_row as usize
        } else {
            location.row as usize
        };

        let arch = &mut self.archetypes[target.0 as usize];
        for &(cid, ptr, tick) in parts {
            // New column (len == row) — push; existing — replace with a drop of
            // the old value.
            unsafe { arch.write_or_replace_component(row, cid, ptr, tick) };
        }
        for &cid in &added_hooked {
            self.hook_queue.push(HookEvent::Added(entity, cid));
        }
        self.flush_hooks();
        true
    }

    /// Remove a component by raw ComponentId.
    pub(crate) fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => {
                // §0.2a: remove targeting a dead entity is a no-op — surface it.
                crate::anomaly!(
                    self, crate::Severity::Warn, "World::remove_raw",
                    Some(entity), None,
                    "no-op: {component_id:?} not removed (entity already despawned)"
                );
                return;
            }
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return;
        }
        let new_arch_id =
            self.find_or_create_archetype_without(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
        if self.registry.any_flags() {
            self.notify_removed(entity, component_id);
            self.flush_hooks();
        }
    }

    pub fn remove<T: Component>(&mut self, entity: Entity) -> bool {
        let component_id = match self.registry.get_id::<T>() {
            Some(id) => id,
            None => return false,
        };
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => {
                // §0.2a: remove targeting a dead entity is a no-op. `false` is
                // returned (as for "component absent"), but most callers ignore
                // it — surface the dead-entity case explicitly.
                crate::anomaly!(
                    self, crate::Severity::Warn, "World::remove",
                    Some(entity), Some(std::any::type_name::<T>()),
                    "no-op: component not removed (entity already despawned)"
                );
                return false;
            }
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return false;
        }
        let new_arch_id =
            self.find_or_create_archetype_without(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
        if self.registry.any_flags() {
            self.notify_removed(entity, component_id);
            self.flush_hooks();
        }
        true
    }

    /// Remove an entity and ALL of its relations (as subject and as target).
    ///
    /// For relation kinds with `cascade_delete_on_target_despawn()` (e.g.
    /// `ChildOf`) subjects are despawned in cascade — iteratively, without
    /// recursion. For other kinds the pairs are cleaned out of the indices: no
    /// relation outlives its target (the generation-honesty of TargetIndex).
    pub fn despawn(&mut self, entity: Entity) -> bool {
        if !self.entities.is_alive(entity) {
            return false;
        }
        let mut stack: SmallVec<[Entity; 8]> = SmallVec::new();
        stack.push(entity);

        while let Some(cur) = stack.pop() {
            if !self.entities.is_alive(cur) {
                continue; // already removed by cascade via another path
            }

            // ── Relations where cur is the target ──────────────
            if self.target_index.has_target(cur.index) {
                for kind_idx in 0..self.relations.kind_count() as u32 {
                    let Some(subjects) = self.target_index.take_subjects(kind_idx, cur.index)
                    else {
                        continue;
                    };
                    let pair = crate::relations::RelationPair {
                        kind_idx,
                        target: cur,
                    };
                    for &s in &subjects {
                        self.subject_index.remove(s.index, pair);
                    }
                    if self.relations.has_remove_hook(kind_idx) {
                        for &s in &subjects {
                            self.hook_queue.push(HookEvent::RelationRemoved {
                                kind_idx,
                                subject: s,
                                target: cur,
                            });
                        }
                    }
                    if self.relations.is_cascade(kind_idx) {
                        stack.extend(subjects);
                    }
                }
            }

            // ── Relations where cur is the subject ─────────────
            for pair in self.subject_index.take_all(cur.index) {
                self.target_index
                    .remove(pair.kind_idx, pair.target.index, cur);
                if self.relations.has_remove_hook(pair.kind_idx) {
                    self.hook_queue.push(HookEvent::RelationRemoved {
                        kind_idx: pair.kind_idx,
                        subject: cur,
                        target: pair.target,
                    });
                }
            }

            // ── Storage row ────────────────────────────────────
            let location = match self.entities.get_location(cur) {
                Some(loc) => loc,
                None => {
                    self.entities.free(cur);
                    continue;
                }
            };
            let arch_idx = location.archetype_id.0 as usize;

            // Notifications about the loss of ALL of the entity's components
            // (on_remove / Removed<T>); hooks will see the entity already dead —
            // after despawn.
            if self.registry.any_flags() {
                let ids: SmallVec<[ComponentId; 8]> = self.archetypes[arch_idx]
                    .component_ids
                    .iter()
                    .copied()
                    .filter(|&cid| self.registry.flags(cid) != 0)
                    .collect();
                for cid in ids {
                    self.notify_removed(cur, cid);
                }
            }

            unsafe {
                if let Some(displaced) =
                    self.archetypes[arch_idx].remove_row(location.row as usize)
                {
                    self.entities.set_location(
                        displaced,
                        EntityLocation {
                            archetype_id: location.archetype_id,
                            row: location.row,
                        },
                    );
                }
            }
            self.entities.free(cur);
        }
        self.flush_hooks();
        true
    }

    // ── Read / Write ───────────────────────────────────────────

    #[inline]
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let component_id = self.registry.get_id::<T>()?;
        let location = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component::<T>(location.row as usize, component_id)
        }
    }

    /// Mutable access that updates the row's change-tick (change detection).
    ///
    /// Stamps the world's current tick → `Changed<T>` fires (as with a mutation
    /// through `Query<&mut T>`/`Write<T>`, C1).
    #[inline]
    /// Mutable access with LAZY change-detection (A13): the returned [`Mut<T>`]
    /// stamps the change-tick only when the caller actually mutates it (via
    /// `DerefMut` / `set_changed`), not on mere access. Previously `get_mut`
    /// stamped eagerly, marking components `Changed<T>` even for read-only
    /// access (false positives → wasted downstream work). `Mut<T>` derefs to
    /// `&mut T`, so most call sites are unchanged; use `bypass_change_detection`
    /// to opt out or `&mut *m` where a `&mut T` is required explicitly.
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<crate::query::Mut<'_, T>> {
        let component_id = self.registry.get_id::<T>()?;
        self.get_mut_by_id(entity, component_id)
    }

    // ── Random-access fast path (CR-M3) ────────────────────────
    //
    // Hot loops (animation: ~22k get_mut/frame) take the ComponentId ONCE per
    // pass via `component_id::<T>()` and then go through `get_by_id`/
    // `get_mut_by_id` — without a TypeId hash on every call.

    /// The ComponentId of type `T`, if it is registered.
    #[inline]
    pub fn component_id<T: Component>(&self) -> Option<ComponentId> {
        self.registry.get_id::<T>()
    }

    /// `get` by a pre-taken ComponentId (see [`component_id`](Self::component_id)).
    ///
    /// `component_id` must correspond to `T` (debug_assert).
    #[inline]
    pub fn get_by_id<T: Component>(&self, entity: Entity, component_id: ComponentId) -> Option<&T> {
        debug_assert_eq!(
            self.registry.get_id::<T>(),
            Some(component_id),
            "get_by_id: ComponentId does not correspond to T"
        );
        let location = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component::<T>(location.row as usize, component_id)
        }
    }

    /// `get_mut` by a pre-taken ComponentId — with LAZY change-detection through
    /// [`Mut<T>`](crate::query::Mut) (A13; see [`get_mut`](Self::get_mut)).
    #[inline]
    pub fn get_mut_by_id<T: Component>(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Option<crate::query::Mut<'_, T>> {
        debug_assert_eq!(
            self.registry.get_id::<T>(),
            Some(component_id),
            "get_mut_by_id: ComponentId does not correspond to T"
        );
        let location = self.entities.get_location(entity)?;
        let this_run = self.current_tick;
        let row = location.row as usize;

        let arch = &mut self.archetypes[location.archetype_id.0 as usize];
        let col_idx = arch.column_index(component_id)?;
        let col = &mut arch.columns[col_idx];
        // A13: hand out a `Mut<T>` that stamps the change-tick lazily on
        // mutation, instead of eagerly here. `change_ticks` is `Vec<TickCell>`
        // (interior-mutable), so the base cast to `*mut Tick` carries cell
        // provenance (A2); the tick pointer and the `&mut T` value point into
        // disjoint buffers (ticks vs data), so they do not alias.
        debug_assert!(row < col.change_ticks.len());
        let change_tick = unsafe { (col.change_ticks.as_ptr() as *mut Tick).add(row) };
        let value = unsafe { col.get_mut::<T>(row) };
        Some(crate::query::Mut {
            value,
            change_tick,
            this_run,
        })
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.entities.is_alive(entity)
    }

    /// Check whether the entity has component `T`.
    ///
    /// O(1) after the first call for the given archetype (column_index is cached).
    #[inline]
    pub fn has_component<T: Component>(&self, entity: Entity) -> bool {
        let Some(cid) = self.registry.get_id::<T>() else {
            return false;
        };
        let Some(loc) = self.entities.get_location(entity) else {
            return false;
        };
        self.archetypes[loc.archetype_id.0 as usize].has_component(cid)
    }

    // ── Query API ──────────────────────────────────────────────

    /// Cached typed read-only query (like Bevy's `world.query::<Q>()`; a mirror
    /// of `ctx.query` in systems). The archetype list is taken from the
    /// incremental global cache. Write shapes — via
    /// [`query_mut`](Self::query_mut) (exclusive borrow).
    pub fn query<Q: WorldQuery + crate::query::ReadOnlyWorldQuery>(
        &self,
    ) -> crate::query::Query<'_, '_, Q> {
        crate::query::Query::from_world_cached(self, Tick::ZERO)
    }

    /// The same with an explicit change-detection base (`Changed<T>`/`Added<T>`
    /// in Q).
    pub fn query_changed<Q: WorldQuery + crate::query::ReadOnlyWorldQuery>(
        &self,
        last_run: Tick,
    ) -> crate::query::Query<'_, '_, Q> {
        crate::query::Query::from_world_cached(self, last_run)
    }

    /// A query of any shape (including `Write<T>`) under an exclusive world
    /// borrow.
    pub fn query_mut<Q: WorldQuery>(&mut self) -> crate::query::Query<'_, '_, Q> {
        crate::query::Query::from_world_cached(self, Tick::ZERO)
    }

    /// [`query_mut`](Self::query_mut) with an explicit change-detection base.
    pub fn query_mut_changed<Q: WorldQuery>(
        &mut self,
        last_run: Tick,
    ) -> crate::query::Query<'_, '_, Q> {
        crate::query::Query::from_world_cached(self, last_run)
    }

    /// Dynamic READ query by runtime `ComponentId`/name (rare case: types not
    /// known statically — scripting/inspector/agent-IPC). For ordinary code — the
    /// typed [`query`](Self::query). Mutation —
    /// [`query_builder_mut`](Self::query_builder_mut).
    pub fn query_builder(&self) -> QueryBuilder<'_> {
        QueryBuilder::new(self)
    }

    /// Dynamic READ/WRITE query (exclusive world borrow ⇒ the yielded `&mut T`
    /// are guaranteed not to alias — B1(v)). See
    /// [`QueryBuilderMut`](crate::query::QueryBuilderMut).
    pub fn query_builder_mut(&mut self) -> crate::query::QueryBuilderMut<'_> {
        crate::query::QueryBuilderMut::new(self)
    }

    // ── Internal methods ───────────────────────────────────────

    pub(crate) fn find_or_create_archetype_with(
        &mut self,
        current: ArchetypeId,
        add: ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize].add_edges.get(&add) {
            return id;
        }
        let mut new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids
            .iter()
            .copied()
            .collect();
        new_components.push(add);
        new_components.sort_unstable();
        let new_id = self.get_or_create_archetype(&new_components);
        self.archetypes[current.0 as usize]
            .add_edges
            .insert(add, new_id);
        self.archetypes[new_id.0 as usize]
            .remove_edges
            .insert(add, current);
        new_id
    }

    pub(crate) fn find_or_create_archetype_without(
        &mut self,
        current: ArchetypeId,
        remove: ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize]
            .remove_edges
            .get(&remove)
        {
            return id;
        }
        let new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids
            .iter()
            .copied()
            .filter(|&id| id != remove)
            .collect();
        let new_id = self.get_or_create_archetype(&new_components);
        self.archetypes[current.0 as usize]
            .remove_edges
            .insert(remove, new_id);
        self.archetypes[new_id.0 as usize]
            .add_edges
            .insert(remove, current);
        new_id
    }

    #[inline(never)]
    pub(crate) fn get_or_create_archetype(&mut self, components: &[ComponentId]) -> ArchetypeId {
        // Borrow<[ComponentId]> — zero-copy lookup without creating an ArchetypeKey
        if let Some(&id) = self.archetype_index.get(components) {
            return id;
        }
        // A duplicate component id means the same component was listed twice
        // (e.g. once in a tuple and again inside a nested Bundle, or an insert of
        // a component the entity already has). Building an archetype from it would
        // create a phantom second column that later drops through a null pointer
        // on despawn — reject loudly (§0.2a; Bevy panics the same way). All callers
        // pass a sorted list, so duplicates are adjacent.
        debug_assert!(
            components.windows(2).all(|w| w[0] <= w[1]),
            "get_or_create_archetype requires a sorted component list"
        );
        if let Some(dup) = components.windows(2).find_map(|w| (w[0] == w[1]).then_some(w[0])) {
            let name = self
                .registry
                .get_info(dup)
                .map(|i| i.name)
                .unwrap_or("<unknown>");
            panic!(
                "duplicate component `{name}` in bundle — a component may appear only once per entity"
            );
        }
        let id = ArchetypeId(self.archetypes.len() as u32);
        let infos: Vec<&ComponentInfo> = components
            .iter()
            .filter_map(|&cid| self.registry.get_info(cid))
            .collect();
        let arch = Archetype::new(id, components.iter().copied().collect(), &infos);
        for &cid in &arch.component_ids {
            self.component_arch_index.entry(cid).or_default().push(id);
        }
        self.archetypes.push(arch);
        self.archetype_index
            .insert(ArchetypeKey::from(components), id);
        // We do not invalidate the QueryCache: cache entries pick up new
        // archetypes incrementally (seen_arch_count), and entity moves do not
        // change the list.
        id
    }

    pub(crate) fn move_entity(
        &mut self,
        entity: Entity,
        from_location: EntityLocation,
        to_archetype_id: ArchetypeId,
    ) -> u32 {
        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx = to_archetype_id.0 as usize;
        let from_row = from_location.row as usize;

        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        // Single pass: for each column of the source archetype, determine its
        // presence in the target and immediately copy or drop.
        let from_len = self.archetypes[from_idx].columns.len();

        for i in 0..from_len {
            let cid = self.archetypes[from_idx].columns[i].component_id;
            let item_size = self.archetypes[from_idx].columns[i].item_size;

            if let Some(to_col_idx) = self.archetypes[to_idx].column_index(cid) {
                // Component present in both archetypes — copy
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
                    // Moving the row preserves BOTH ticks: an archetype move does
                    // not "update" either Changed<T> or Added<T> (W3-1).
                    let changed = self.archetypes[from_idx].columns[i].get_tick(from_row);
                    let added = self.archetypes[from_idx].columns[i].get_added_tick(from_row);
                    self.archetypes[to_idx].columns[to_col_idx].push_moved_ticks(changed, added);

                    // swap_remove without drop (data moved into the target
                    // archetype)
                    self.archetypes[from_idx].columns[i].swap_remove_no_drop(from_row);
                }
            } else {
                // Component absent in the target — drop
                unsafe {
                    self.archetypes[from_idx].columns[i].swap_remove_and_drop(from_row);
                }
            }
        }

        // Fix up the location for the displaced entity (swap_remove)
        let from_last = self.archetypes[from_idx].entities.len() - 1;
        if from_row != from_last {
            let displaced = self.archetypes[from_idx].entities[from_last];
            self.archetypes[from_idx].entities.swap(from_row, from_last);
            self.archetypes[from_idx].entities.pop();
            self.entities.set_location(
                displaced,
                EntityLocation {
                    archetype_id: from_location.archetype_id,
                    row: from_row as u32,
                },
            );
        } else {
            self.archetypes[from_idx].entities.pop();
        }

        to_row as u32
    }

    // ── Optimization 4.1: add_relation_batch ───────────────────

    /// Batch-add the same relation from many subjects to a single target.
    ///
    /// After CR-M1 relations are not part of archetype identity, so this is just a
    /// bulk insert into the indices — O(S), without structural world changes.
    pub fn add_relation_batch<R: crate::relations::RelationKind>(
        &mut self,
        subjects: &[Entity],
        _kind: R,
        target: Entity,
    ) {
        if subjects.is_empty() {
            return;
        }
        let kind_idx = self.relations.get_or_register::<R>();
        for &subject in subjects {
            self.add_relation_by_kind_idx(subject, kind_idx, target);
        }
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

// ── MainWorld ──────────────────────────────────────────────────

/// Wraps a [`World`] for temporary insertion as a resource.
///
/// Used by extract systems to read the main world while running on the
/// render world's scheduler. Bevy-compatible pattern.
pub struct MainWorld(pub World);

impl MainWorld {
    pub fn world(&self) -> &World {
        &self.0
    }
}

// §1.4 audit (2026-07-06): the unsafe impls ARE required and ARE sound; journaled here
// so the invariant is explicit rather than assumed.
//
// WHY REQUIRED: `World` is not `Send`/`Sync` (it holds thread-affine internals). To let
// an extract system read the main world, the main world is inserted into the RENDER
// world as a `MainWorld` resource — and resource storage is `Send + Sync`-bounded (it is
// shared with the parallel scheduler). So `MainWorld` must be `Send + Sync` to be a
// resource at all; without these impls the extract path does not compile.
//
// WHY SOUND (load-bearing invariant): `MainWorld` exists in the render world ONLY for the
// duration of the EXTRACT stage, which the scheduler runs SEQUENTIALLY (extract is a
// whole-world/exclusive stage — no two systems touch `MainWorld` concurrently), and it is
// removed and handed back to the main thread when extract ends. The type is marked
// `Send + Sync`, but no concurrent access to the wrapped world ever occurs. If extract is
// ever made to run in parallel with a system touching `MainWorld`, this becomes unsound —
// keep extract sequential.
unsafe impl Send for MainWorld {}
unsafe impl Sync for MainWorld {}

// ── SystemContext ──────────────────────────────────────────────

// The constants were replaced by adaptive logic in adaptive_chunk_size.
// MIN_CHUNK_SIZE and MAX_CHUNK_SIZE are no longer used.
// Kept only for backward compatibility, if needed.

pub const DEFAULT_MAX_CHUNK_SIZE: usize = 65536;

/// Configuration of the parallel chunking strategy.
///
/// Defines how [`adaptive_chunk_size`] splits entities into chunks for parallel
/// iteration (`par_for_each`).
///
/// The single carrier of parallelism tuning: both chunk-sizing
/// ([`adaptive_chunk_size`]) and the scheduler's stage-level gating (whether to
/// parallelize a stage at all). Passed via `World::set_chunk_config()`; the
/// scheduler reads the stage-gating fields from `world.chunk_config()`. If not
/// set explicitly — [`ChunkConfig::default()`]; [`ChunkConfig::from_env()`] reads
/// overrides from the environment.
///
/// # Example
///
/// ```ignore
/// let config = ChunkConfig {
///     min_entities_per_thread: 32,
///     max_chunk_size: 8192,
///     ..Default::default()
/// };
/// world.set_chunk_config(config);
/// ```
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// The minimum number of entities per thread below which parallelism is not
    /// worthwhile. For 8 threads with `min = 16`, worlds up to 128 entities go
    /// into a single chunk (serial).
    ///
    /// Default: 16.
    pub min_entities_per_thread: usize,

    /// Dynamic minimum chunk size — protection against rayon micro-tasks. If the
    /// computed chunk size is smaller than this value, it is raised to it.
    ///
    /// Default: 128/32/64 (depends on world size, as before the refactoring).
    pub dynamic_min_chunk: usize,

    /// Maximum chunk size (a growth limiter for huge worlds).
    ///
    /// Default: 65536 (or from `PAR_CHUNK_SIZE`, if set).
    pub max_chunk_size: usize,

    /// If `true` — always use a single chunk for `N < min_entities_per_thread *
    /// threads`. If `false` — always split into `threads` chunks (even small
    /// ones).
    ///
    /// Default: `true`.
    pub auto_serial_fallback: bool,

    /// How many times more tasks to create than Rayon threads. 1.0 = exactly
    /// num_threads tasks, 2.0 = twice as many. More tasks → better work-stealing,
    /// but more per-task overhead.
    ///
    /// Default: `2.0`.
    pub task_multiplier: f32,

    // ── Stage-level parallelism gating (read by the scheduler) ──
    /// Explicit floor: a stage runs SEQUENTIALLY when the total entity count of
    /// its systems is below this. `0` = no floor (the cost-model / heuristic
    /// decides). When `> 0` it always wins over the auto-heuristic.
    ///
    /// Default: `0`.
    pub stage_parallel_min_entities: usize,

    /// When `true`, the scheduler auto-disables stage parallelism on the cold
    /// start (no cost history yet) via a per-system entity-count heuristic
    /// (≈15k/25k/80k entity per system for 3+/2/1 systems). Once the cost-model
    /// has measured a stage it supersedes this. `false` = always attempt
    /// parallel (subject to the cost-model once warm).
    ///
    /// Default: `true`.
    pub auto_disable_stage_parallel: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            min_entities_per_thread: 16,
            dynamic_min_chunk: 64,
            max_chunk_size: DEFAULT_MAX_CHUNK_SIZE,
            auto_serial_fallback: true,
            task_multiplier: 2.0,
            stage_parallel_min_entities: 0,
            auto_disable_stage_parallel: true,
        }
    }
}

impl ChunkConfig {
    /// Config from [`Default`] with environment overrides applied. Single env
    /// entry point (replaces the former `set_par_chunk_size` global atomic that
    /// silently fed `Default`). Reads `APEX_PAR_CHUNK_SIZE` → `max_chunk_size`.
    ///
    /// Apply with `world.set_chunk_config(ChunkConfig::from_env())`.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(val) = std::env::var("APEX_PAR_CHUNK_SIZE") {
            if let Ok(n) = val.trim().parse::<usize>() {
                if n > 0 {
                    cfg.max_chunk_size = n;
                }
            }
        }
        cfg
    }
}

/// Compute the adaptive chunk size based on the entity count and configuration.
///
/// Logic (accounting for `task_multiplier` for Rayon work-stealing):
/// 1. If `auto_serial_fallback` and `entity_count < min_entities_per_thread * thread_count` — one chunk (serial).
/// 2. Otherwise — `ceil(entity_count / thread_count / task_multiplier)`, clamped to `[dynamic_min_chunk, max_chunk_size]`.
pub fn adaptive_chunk_size(entity_count: usize, num_threads: usize, config: &ChunkConfig) -> usize {
    if entity_count == 0 {
        return 1;
    }
    let n = num_threads.max(1);
    let serial_threshold = config.min_entities_per_thread.saturating_mul(n);
    if config.auto_serial_fallback && entity_count < serial_threshold {
        return entity_count;
    }
    let targets = if n > 1 {
        (n as f32 * config.task_multiplier).ceil() as usize
    } else {
        1
    };
    let raw = entity_count.div_ceil(targets);
    raw.clamp(config.dynamic_min_chunk, config.max_chunk_size)
        .min(entity_count)
}

pub struct SystemContext<'w> {
    /// The SubWorlds this system sees. Usually one SubWorld, but there may be
    /// several if the system works with multiple groups of archetypes.
    pub(crate) sub_worlds: &'w [crate::sub_world::SubWorld<'w>],
    /// D8b: per-system command slot. Points at THIS system's private `Commands`
    /// buffer, owned by the scheduler for the duration of the stage. The scheduler
    /// hands a slot only to SINGLE-TASK systems (a command-emitting system declares
    /// `uses_commands` ⇒ `non_query_side_effects` ⇒ it is never row-split), so
    /// exactly one task ever forms `&mut *ptr` — no cross-task aliasing. Row-split
    /// (query-only) tasks get `None` and fall back to `inline_cmds`. Applying these
    /// per-system buffers in rank order gives deterministic command ordering (D8b).
    /// If `None` — `inline_cmds` is used (sequential / undeclared paths).
    pub(crate) deferred_cmds: Option<*mut Commands>,
    /// A local Commands for sequential systems or when deferred_cmds is not set.
    /// Used instead of the global static `DUMMY_COMMANDS`.
    pub(crate) inline_cmds: UnsafeCell<Commands>,
}

unsafe impl Send for SystemContext<'_> {}
unsafe impl Sync for SystemContext<'_> {}

impl<'w> SystemContext<'w> {
    pub fn new(sub_worlds: &'w [crate::sub_world::SubWorld<'w>]) -> Self {
        Self {
            sub_worlds,
            deferred_cmds: None,
            inline_cmds: UnsafeCell::new(Commands::new()),
        }
    }

    /// Creates a SystemContext from a single SubWorld (the most common case).
    pub fn from_sub_world(sub_world: &'w crate::sub_world::SubWorld<'w>) -> Self {
        Self {
            sub_worlds: std::slice::from_ref(sub_world),
            deferred_cmds: None,
            inline_cmds: UnsafeCell::new(Commands::new()),
        }
    }

    /// Create a context with a per-system command buffer (D8b).
    ///
    /// # Safety
    /// `deferred_cmds` must point to a `Commands` owned by the caller (scheduler)
    /// and alive for the context's lifetime, handed only to a SINGLE-TASK system
    /// (no row-split), so no other task forms a concurrent `&mut` to the same slot
    /// (see [`commands`](Self::commands)).
    pub unsafe fn with_commands(
        sub_worlds: &'w [crate::sub_world::SubWorld<'w>],
        deferred_cmds: *mut Commands,
    ) -> Self {
        Self {
            sub_worlds,
            deferred_cmds: Some(deferred_cmds),
            inline_cmds: UnsafeCell::new(Commands::new()),
        }
    }

    /// Get the `Commands` for the current thread. Commands are applied by the
    /// scheduler after the Stage completes.
    ///
    /// `&self → &mut Commands` is intentional: the command buffer is either a
    /// private per-system slot (D8b: the owner system is single-task, i.e. not
    /// row-split, → unique access from one thread) or an `UnsafeCell` in
    /// sequential/undeclared mode, so the uniqueness of `&mut` is guaranteed
    /// without `&mut self`.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn commands(&self) -> &mut Commands {
        let cmds = if let Some(ptr) = self.deferred_cmds {
            // SAFETY: `ptr` is this system's private command slot (D8b). The
            // scheduler hands a slot ONLY to single-task systems (a
            // command-emitting system carries `uses_commands` ⇒
            // `non_query_side_effects` ⇒ never row-split), so exactly one task
            // forms this `&mut`. Row-split (query-only) tasks get `None` and do not
            // alias the slot.
            unsafe { &mut *ptr }
        } else {
            // SAFETY: inline_cmds is used only when a per-system slot is not set
            // (sequential / row-split-inline). Access is exclusive — one thread,
            // its own ctx.
            unsafe { &mut *self.inline_cmds.get() }
        };
        // The single point of reserver injection: any `cmd.spawn().id()` from a
        // system gets a real cross-frame `Entity` (the reserver shares the atomic
        // high-water with the allocator of the same world the commands are applied
        // to). Idempotent — an Arc clone once per Commands lifetime.
        if !cmds.has_reserver() {
            cmds.set_reserver(self.world().entity_reserver());
        }
        cmds
    }

    /// Get the World (for backward compatibility).
    /// Used for query, resource, and event access.
    fn world(&self) -> &'w World {
        self.sub_worlds[0].world()
    }

    /// Read-only query over the system's SubWorld. Requires `Q: ReadOnlyWorldQuery`
    /// (F3): a mutable query obtained from `ctx` is UNDECLARED write access, and the
    /// scheduler parallelizes systems by their DECLARED access — an undeclared `&mut`
    /// is a safe-reachable data race (the exact class B1(v) closed for `Query::new`).
    /// Declare mutable access as a system PARAMETER (`q: Query<Write<T>>` in the
    /// signature) — the scheduler validates it and serializes conflicts — rather than
    /// reaching for a mutable `ctx.query`.
    #[inline]
    pub fn query<Q: WorldQuery + crate::query::ReadOnlyWorldQuery>(&self) -> crate::query::Query<'_, '_, Q> {
        // The change-detection base is the world's `last_run_tick` (the previous
        // frame boundary), so `Changed<T>` inside the system is reliable (TD-9)
        // rather than "everything".
        let last_run = self.sub_worlds[0].world().last_run_tick();
        // SAFETY: read-only `Q` cannot alias `&mut`; the SubWorld is scheduler-vended.
        unsafe { crate::query::Query::from_sub_world(&self.sub_worlds[0], last_run) }
    }

    /// Query with an ARBITRARY (possibly mutable) `Q`, WITHOUT the read-only bind.
    ///
    /// The caller must guarantee the system DECLARED this access, so the scheduler
    /// serializes conflicting systems (otherwise an undeclared `&mut` races). This
    /// holds for the declared query built by `SystemParam` / the `system!` macro —
    /// which is the ONLY intended caller. Prefer a mutable query PARAMETER. Hidden
    /// from docs: an internal escape, not blessed API (mirrors `Query::new_unchecked`).
    #[doc(hidden)]
    #[inline]
    pub fn query_unchecked<Q: WorldQuery>(&self) -> crate::query::Query<'_, '_, Q> {
        let last_run = self.sub_worlds[0].world().last_run_tick();
        // SAFETY: the caller guarantees the access was declared (see doc); the
        // context's SubWorlds are scheduler-vended under that validated access.
        unsafe { crate::query::Query::from_sub_world(&self.sub_worlds[0], last_run) }
    }

    #[inline]
    pub fn query_changed<Q: WorldQuery + crate::query::ReadOnlyWorldQuery>(
        &self,
        last_run: Tick,
    ) -> crate::query::Query<'_, '_, Q> {
        // SAFETY: see `query` above (read-only Q).
        unsafe { crate::query::Query::from_sub_world(&self.sub_worlds[0], last_run) }
    }

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(self.world().resource::<T>())
    }

    /// Mutable resource access WITHOUT a declared-access check (F3.2). The scheduler
    /// parallelizes systems by their DECLARED access, so a `ResMut` obtained from
    /// `ctx` that the system did NOT declare is a safe-reachable data race (the same
    /// class F3.1 closed for `ctx.query`). Declare it as a `ResMut<T>` PARAMETER — the
    /// scheduler validates it and serializes conflicts. The ONLY intended callers are
    /// `SystemParam` (`ResMut`/`ResWrite`) and the `system!` macro, where access is
    /// validated. `#[doc(hidden)]`: an internal escape, not blessed API.
    #[doc(hidden)]
    #[inline]
    pub fn resource_mut_unchecked<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = self
                .world()
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut_unchecked: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn try_resource<T: Send + Sync + 'static>(&self) -> Option<Res<'_, T>> {
        self.world().try_resource::<T>().map(Res)
    }

    /// Fallible [`resource_mut_unchecked`](Self::resource_mut_unchecked) — same F3.2
    /// caveat: declared access required, internal escape.
    #[doc(hidden)]
    #[inline]
    pub fn try_resource_mut_unchecked<T: Send + Sync + 'static>(&self) -> Option<ResMut<'_, T>> {
        unsafe {
            self.world()
                .resources
                .get_raw_ptr::<T>()
                .map(|ptr| ResMut::from_ptr(ptr))
        }
    }

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        unsafe {
            let ptr = self
                .world()
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    /// F4: build an [`EventReader`] over a PERSISTENT per-system cursor stored in
    /// `cursor` (the system's `SystemParam` state). Created on first call and
    /// reused every frame, so the read position survives across frames /
    /// FixedUpdate catch-up runs (no reset-to-zero ⇒ no duplicate reads). The
    /// only intended caller is `SystemParam for EventReader`.
    ///
    /// # Safety
    /// Same contract as [`event_reader`](Self::event_reader): the scheduler
    /// serializes conflicting event access, so this system has exclusive access
    /// to `T`'s queue for the call.
    #[inline]
    pub(crate) fn event_reader_persistent<T: Send + Sync + 'static>(
        &self,
        cursor: &mut Option<crate::events::EventCursor>,
    ) -> EventReader<'_, T> {
        unsafe {
            let ptr = self
                .world()
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            let events = &mut *ptr;
            let c = match *cursor {
                Some(c) => c,
                None => {
                    let c = events.add_reader();
                    *cursor = Some(c);
                    c
                }
            };
            EventReader::from_persistent(events, c)
        }
    }

    /// Event-writer access WITHOUT a declared-access check (F3.2). An `EventWriter`
    /// obtained from `ctx` that the system did NOT declare (`Emit<E>`) races with
    /// other undeclared writers/readers of the same event. Declare it as an
    /// `EventWriter<E>` PARAMETER. The ONLY intended callers are `SystemParam` and the
    /// `system!` macro. `#[doc(hidden)]`: an internal escape, not blessed API.
    #[doc(hidden)]
    #[inline]
    pub fn event_writer_unchecked<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self
                .world()
                .event_queue_ptr::<T>()
                .expect("event_writer_unchecked: event type not registered");
            EventWriter::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn entity_count(&self) -> usize {
        self.world().entity_count()
    }

    /// Fetch declared parameters via [`SystemParam`](crate::system_param::SystemParam).
    ///
    /// `#[doc(hidden)]` + `_unchecked`: this bypasses the scheduler's access
    /// validation — `P` is fetched whether or not the running system *declared*
    /// it, so `fetch_unchecked::<ResWrite<B>>()` from a system that didn't declare
    /// `B` is an undeclared mutable access (data race, F3/ADR-002 class). Sound
    /// only when `P` equals the system's declared params (`Self::Params`), which
    /// the scheduler validated. The blessed path is params-as-access (declared
    /// `SystemParam` arguments); direct `fetch` is the raw mechanism, not API.
    #[doc(hidden)]
    #[inline]
    pub fn fetch_unchecked<P: crate::system_param::SystemParam>(&self) -> P::Item<'_> {
        P::fetch(self)
    }

    // Iteration only via ctx.query::<Q>().for_each(...)
    // or ctx.query::<Q>().par_for_each(...)
}

// ── Relations API on SystemContext ─────────────────────────────────

impl<'w> SystemContext<'w> {
    /// Query by relation: find all entities with relation `R` to `target` that
    /// also have components `Q`.
    #[inline]
    pub fn query_relation<R: crate::relations::RelationKind, Q: WorldQuery>(
        &self,
        _kind: R,
        target: Entity,
    ) -> crate::relations::RelationIter<'_, Q> {
        self.world().query_relation::<R, Q>(_kind, target)
    }

    /// Wildcard query: find all entities with any relation of kind `R` that also
    /// have components `Q`.
    #[inline]
    pub fn query_wildcard<R: crate::relations::RelationKind, Q: WorldQuery>(
        &self,
        _kind: R,
    ) -> crate::relations::RelationIter<'_, Q> {
        self.world().query_wildcard::<R, Q>(_kind)
    }

    /// Subjects pointing at `parent` via relation `R` (see
    /// [`World::targets_of`](crate::world::World::targets_of)).
    #[inline]
    pub fn targets_of<R: crate::relations::RelationKind>(
        &self,
        kind: R,
        parent: Entity,
    ) -> impl Iterator<Item = Entity> + '_ {
        self.world().targets_of(kind, parent)
    }

    /// Check for a relation `R` between `subject` and `target`.
    #[inline]
    pub fn has_relation<R: crate::relations::RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
        target: Entity,
    ) -> bool {
        self.world().has_relation(subject, _kind, target)
    }

    /// Target of the first relation `R` from `subject` (see
    /// [`World::target_of`](crate::world::World::target_of)).
    #[inline]
    pub fn target_of<R: crate::relations::RelationKind>(
        &self,
        subject: Entity,
        kind: R,
    ) -> Option<Entity> {
        self.world().target_of(subject, kind)
    }
}

// ── ParallelWorld ──────────────────────────────────────────────

pub struct ParallelWorld<'w> {
    pub(crate) world: *const World,
    pub(crate) _marker: std::marker::PhantomData<&'w World>,
}

unsafe impl Send for ParallelWorld<'_> {}
unsafe impl Sync for ParallelWorld<'_> {}

impl<'w> ParallelWorld<'w> {
    /// # Safety
    /// The caller must ensure no `&mut World` to the same world is live for the
    /// duration of the returned `&'w World` (the scheduler guarantees this for
    /// read-only parallel access).
    #[inline]
    pub unsafe fn get(&self) -> &'w World {
        &*self.world
    }
}

// ── QueryState — per-system query state (W2-0, Bevy model) ─────

/// Owner of long-lived query state: the list of matching archetypes plus the
/// resolved ComponentIds. Grows INCREMENTALLY (archetypes are append-only); in
/// steady state `query()` is a single counter check — no locks, no hash lookups,
/// no allocations (unlike the global `QueryCache`, which pays key+hash+RwLock+
/// Arc-clone on every call).
///
/// Bound to a specific world by [`World::id`]: applying it to a different world
/// (main vs render vs isolated) transparently rebuilds the state — one world's
/// ComponentIds are not valid in another.
///
/// Carries the same `(D, F)` data/filter split as [`Query`](crate::query::Query);
/// `F` defaults to `()`.
///
/// ```ignore
/// struct ExtractMeshes {
///     q: QueryState<(Read<Mesh>, Read<GlobalTransform>)>,
/// }
/// // in the hot loop:
/// self.q.query(&world).for_each(|e, (mesh, gt)| { ... });
/// ```
pub struct QueryState<D: WorldQuery, F: WorldQuery = ()> {
    world_id: u64,
    ids: crate::query::IdBuf,
    /// Whether every component of the shape was registered at update time.
    /// Until then ids are re-read each call (registration is lazy) and the
    /// archetype scan does not start.
    ids_resolved: bool,
    arch_indices: Vec<usize>,
    seen_arch_count: usize,
    _phantom: std::marker::PhantomData<fn() -> (D, F)>,
}

impl<D: WorldQuery, F: WorldQuery> Default for QueryState<D, F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: WorldQuery, F: WorldQuery> QueryState<D, F> {
    /// Empty state; binds to a world on the first [`query`](Self::query).
    pub fn new() -> Self {
        Self {
            world_id: 0,
            ids: crate::query::IdBuf::new(),
            ids_resolved: false,
            arch_indices: Vec::new(),
            seen_arch_count: 0,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Read-only query with change-detection base `world.last_run_tick()` (as
    /// `ctx.query` in systems). Write shapes — [`query_mut`](Self::query_mut).
    #[inline]
    pub fn query<'a>(&'a mut self, world: &'a World) -> crate::query::Query<'a, 'a, D, F>
    where
        D: crate::query::ReadOnlyWorldQuery,
        F: crate::query::ReadOnlyWorldQuery,
    {
        let last_run = world.last_run_tick();
        self.query_with_tick(world, last_run)
    }

    /// Read-only query with an explicit `last_run` base (for `Changed<T>` shapes
    /// with their own frame boundary, e.g. extract systems).
    pub fn query_with_tick<'a>(
        &'a mut self,
        world: &'a World,
        last_run: Tick,
    ) -> crate::query::Query<'a, 'a, D, F>
    where
        D: crate::query::ReadOnlyWorldQuery,
        F: crate::query::ReadOnlyWorldQuery,
    {
        self.update(world);
        crate::query::Query::from_state_parts(world, &self.arch_indices, &self.ids, last_run)
    }

    /// Any-shape query under an exclusive world borrow.
    #[inline]
    pub fn query_mut<'a>(&'a mut self, world: &'a mut World) -> crate::query::Query<'a, 'a, D, F> {
        let last_run = world.last_run_tick();
        self.query_mut_with_tick(world, last_run)
    }

    /// [`query_mut`](Self::query_mut) with an explicit `last_run` base.
    pub fn query_mut_with_tick<'a>(
        &'a mut self,
        world: &'a mut World,
        last_run: Tick,
    ) -> crate::query::Query<'a, 'a, D, F> {
        crate::query::assert_no_self_alias::<(D, F)>(world);
        self.update(world);
        crate::query::Query::from_state_parts(world, &self.arch_indices, &self.ids, last_run)
    }

    /// Any-shape query through the unsafe world escape.
    ///
    /// # Safety
    /// Same contract as [`Query::new_unchecked`](crate::query::Query::new_unchecked).
    pub unsafe fn query_unchecked_with_tick<'a>(
        &'a mut self,
        world: crate::unsafe_world_cell::UnsafeWorldCell<'a>,
        last_run: Tick,
    ) -> crate::query::Query<'a, 'a, D, F> {
        let world = world.world();
        crate::query::assert_no_self_alias::<(D, F)>(world);
        self.update(world);
        crate::query::Query::from_state_parts(world, &self.arch_indices, &self.ids, last_run)
    }

    /// Incremental update for the world: rebuild on world change, resolve ids
    /// lazily (lazy component registration), scan ONLY new archetypes.
    fn update(&mut self, world: &World) {
        if self.world_id != world.world_id {
            self.world_id = world.world_id;
            self.ids = crate::query::IdBuf::new();
            self.ids_resolved = false;
            self.arch_indices.clear();
            self.seen_arch_count = 0;
        }

        if !self.ids_resolved {
            // The INVALID sentinel marks a not-yet-registered component of the
            // shape. It may be legitimate forever (Maybe/Without/dead Or branch),
            // so do not block the scan — re-read ids each call; if the resolution
            // changed (a component was registered later), rescan archetypes from
            // scratch: the matches may have changed.
            let mut fresh = crate::query::IdBuf::new();
            <(D, F)>::fill_ids(world, &mut fresh);
            if fresh != self.ids {
                self.ids = fresh;
                self.arch_indices.clear();
                self.seen_arch_count = 0;
            }
            self.ids_resolved = !self.ids.contains(&ComponentId::INVALID);
        }

        let total = world.archetypes.len();
        if self.seen_arch_count < total {
            for (i, arch) in world.archetypes[self.seen_arch_count..].iter().enumerate() {
                if <(D, F)>::matches_archetype(arch, &self.ids) {
                    self.arch_indices.push(self.seen_arch_count + i);
                }
            }
            self.seen_arch_count = total;
        }
    }
}

// ── Bundle ─────────────────────────────────────────────────────

pub trait Bundle: Sized {
    /// The bundle composition in DECLARATION order WITHOUT constructing a value
    /// (§10.10): derive/tuples know it statically from the types. This removes the
    /// `make_bundle(0)` probe footgun in `spawn_many` (the closure no longer has
    /// to be pure and is not called an extra time for the composition).
    ///
    /// The order is DECLARATION order, NOT sorted: `col_indices` for
    /// `write_into_batch` must be in traversal order (otherwise a write to the
    /// wrong column — UB). The sorted archetype key is given by [`component_ids`].
    fn static_component_ids(
        registry: &mut ComponentRegistry,
        out: &mut SmallVec<[ComponentId; 8]>,
    );

    /// The composition as a SORTED owned `SmallVec` — the archetype key. Delegates
    /// to [`static_component_ids`](Bundle::static_component_ids) + `sort_unstable`.
    fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
        let mut out = SmallVec::new();
        Self::static_component_ids(registry, &mut out);
        out.sort_unstable();
        out
    }

    /// Write the ComponentIds in DECLARATION order into `out` (without an
    /// intermediate SmallVec) — for the `write_into_batch` traversal.
    fn push_component_ids(
        &self,
        registry: &mut ComponentRegistry,
        out: &mut SmallVec<[ComponentId; 8]>,
    ) {
        Self::static_component_ids(registry, out);
    }

    fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick);

    /// The number of components in this Bundle (statically, for splitting
    /// col_indices).
    fn component_count() -> usize;

    /// Batch write of components with precomputed column indices.
    ///
    /// By default calls `write_into`. Overridden for optimization: uses the passed
    /// `col_indices` instead of repeatedly calling `get_or_register` and
    /// `column_index` for each entity.
    fn write_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        tick: Tick,
        _col_indices: &[usize],
    ) {
        self.write_into(world, archetype_id, row, tick);
    }

    /// Write component data for a batch spawn ([`spawn_many`]). OVERRIDDEN
    /// (leaf/tuple/derive) to be data-only: writes ONLY data (without change/added
    /// ticks and without `col.len`), while the caller sets ticks/`len` PER-COLUMN
    /// once per batch (much cheaper than `count×ncols` pushes). The **default**
    /// (for manual `impl Bundle`) is the full `write_into_batch` (data+ticks+len).
    /// The caller is robust to BOTH: it uses the ABSOLUTE target
    /// (`start_row+count`) for `resize`/`len`, so ticks/len already set by the
    /// default are a no-op, and for a data-only override they are filled in.
    /// `tick` is used by the default and ignored by overrides.
    fn write_data_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        tick: Tick,
        col_indices: &[usize],
    ) {
        self.write_into_batch(world, archetype_id, row, tick, col_indices);
    }

    /// Returns true if at least one component of the Bundle has Drop (needed for
    /// spawn_many).
    ///
    /// For types with Drop, bulk-copy via `copy_nonoverlapping` is unsafe, so a
    /// per-entity loop is used.
    fn needs_drop() -> bool {
        false
    }
}

// ── Blanket impl: any Component is a Bundle (of a single component) ──

impl<T: Component> Bundle for T {
    #[inline(always)]
    fn component_count() -> usize {
        1
    }

    #[inline(always)]
    fn static_component_ids(
        registry: &mut ComponentRegistry,
        out: &mut SmallVec<[ComponentId; 8]>,
    ) {
        out.push(registry.get_or_register::<T>());
    }

    #[inline(always)]
    fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick) {
        let cid = world.registry.get_or_register::<T>();
        if let Some(ci) = world.archetypes[archetype_id.0 as usize].column_index(cid) {
            unsafe {
                let col = &mut world.archetypes[archetype_id.0 as usize].columns[ci];
                if col.item_size > 0 {
                    if col.len >= col.capacity {
                        col.grow();
                    }
                    let dst = col.get_ptr(row);
                    std::ptr::copy_nonoverlapping(
                        &self as *const T as *const u8,
                        dst,
                        col.item_size,
                    );
                }
                col.change_ticks.push(TickCell::new(tick));
                col.added_ticks.push(TickCell::new(tick));
                col.len += 1;
            }
        }
        std::mem::forget(self);
    }

    #[inline(always)]
    fn write_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        tick: Tick,
        col_indices: &[usize],
    ) {
        let col_idx = col_indices[0];
        unsafe {
            let col = &mut world.archetypes[archetype_id.0 as usize].columns[col_idx];
            if col.item_size > 0 {
                if col.len >= col.capacity {
                    col.grow();
                }
                let dst = col.get_ptr(row);
                std::ptr::copy_nonoverlapping(&self as *const T as *const u8, dst, col.item_size);
            }
            col.change_ticks.push(TickCell::new(tick));
            col.added_ticks.push(TickCell::new(tick));
            col.len += 1;
        }
        std::mem::forget(self);
    }

    #[inline(always)]
    fn write_data_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        _tick: Tick,
        col_indices: &[usize],
    ) {
        let col_idx = col_indices[0];
        // SAFETY: capacity is reserved by the caller (`reserve(count)`), `row` is
        // within bounds; ticks/`len` are set by the caller per-column AFTER writing
        // the data of all rows (data-only).
        unsafe {
            let col = &mut world.archetypes[archetype_id.0 as usize].columns[col_idx];
            if col.item_size > 0 {
                let dst = col.get_ptr(row);
                std::ptr::copy_nonoverlapping(&self as *const T as *const u8, dst, col.item_size);
            }
        }
        std::mem::forget(self);
    }

    #[inline(always)]
    fn needs_drop() -> bool {
        std::mem::needs_drop::<T>()
    }
}

// ── Recursive impl_bundle! for Bundle tuples ──
//
// Tuple elements are any Bundle (components, other Bundle structs, tuples).
// The arity count is 12 (like Bevy).

macro_rules! impl_bundle {
    ($($T:ident),+) => {
        #[allow(non_snake_case)]
        impl<$($T: Bundle),+> Bundle for ($($T,)+) {
            #[inline]
            fn component_count() -> usize {
                0usize $( + $T::component_count() )+
            }

            /// IMPORTANT (batch-spawn correctness): pushes ids in the tuple's
            /// DECLARATION order — the same one in which `write_into_batch`
            /// traverses the components. `component_ids` (the trait default) SORTS
            /// (the archetype key), whereas `col_indices` for `write_into_batch`
            /// MUST be in traversal order, otherwise a component is written to the
            /// wrong column (UB: writing a 64B Matrix4 into a 12B column). See
            /// `spawn_many_inner`/`spawn_bundles_bulk` — they build `col_indices`
            /// from here.
            #[inline]
            fn static_component_ids(
                registry: &mut ComponentRegistry,
                out: &mut SmallVec<[ComponentId; 8]>,
            ) {
                $( $T::static_component_ids(registry, out); )+
            }

            #[inline]
            fn write_into(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
            ) {
                #[allow(non_snake_case)]
                let ($($T,)+) = self;
                $( $T.write_into(world, archetype_id, row, tick); )+
            }

            #[inline]
            fn write_into_batch(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
                col_indices:  &[usize],
            ) {
                #[allow(non_snake_case)]
                let ($($T,)+) = self;
                let mut _offset = 0usize;
                $(
                    let _cnt = $T::component_count();
                    $T.write_into_batch(world, archetype_id, row, tick, &col_indices[_offset.._offset + _cnt]);
                    _offset += _cnt;
                )+
            }

            #[inline]
            fn write_data_into_batch(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
                col_indices:  &[usize],
            ) {
                #[allow(non_snake_case)]
                let ($($T,)+) = self;
                let mut _offset = 0usize;
                $(
                    let _cnt = $T::component_count();
                    $T.write_data_into_batch(world, archetype_id, row, tick, &col_indices[_offset.._offset + _cnt]);
                    _offset += _cnt;
                )+
            }

            #[inline]
            fn needs_drop() -> bool {
                false $( || $T::needs_drop() )+
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
impl_bundle!(A, B, C, D, E, F, G, H, I);
impl_bundle!(A, B, C, D, E, F, G, H, I, J);
impl_bundle!(A, B, C, D, E, F, G, H, I, J, K);
impl_bundle!(A, B, C, D, E, F, G, H, I, J, K, L);
// ── impl Bundle for () ────────────────────────────────────────

impl Bundle for () {
    fn component_count() -> usize {
        0
    }

    fn static_component_ids(
        _registry: &mut ComponentRegistry,
        _out: &mut SmallVec<[ComponentId; 8]>,
    ) {
    }

    fn write_into(self, _world: &mut World, _archetype_id: ArchetypeId, _row: usize, _tick: Tick) {
        // ()
    }

    fn needs_drop() -> bool {
        false
    }
}

// ── Entity accessor ladder (R-4) ───────────────────────────────
//
// Honest names for the three access levels (Bevy-parity ladder):
//   • `EntityRef`      — read-only view over `&World`  (get / relation reads);
//   • `EntityWorldMut` — full mutable access over `&mut World`, INCLUDING
//     structural ops (insert / remove / despawn) + relations + hierarchy sugar.
// (`EntityMut` — component-mutation-only, usable as `QueryData` with disjoint
//  access — is a separate feature, not yet built.)
//
// Obtain them via `World::entity` (read), `World::entity_mut` (full), or the
// checked `get_entity` / `get_entity_mut`.

/// Read-only view over a single entity's components and relations. Obtained via
/// [`World::entity`] / [`World::get_entity`]. For mutation use
/// [`EntityWorldMut`] (via [`World::entity_mut`]).
pub struct EntityRef<'w> {
    world: &'w World,
    entity: Entity,
}

impl<'w> EntityRef<'w> {
    /// Return the entity id.
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Whether the entity is still alive.
    pub fn is_alive(&self) -> bool {
        self.world.entities.is_alive(self.entity)
    }

    /// Read component `T` (immutable), or `None` if absent.
    pub fn get<T: Component>(&self) -> Option<&T> {
        self.world.get::<T>(self.entity)
    }

    /// Whether this entity has a relation `R` to `target`.
    pub fn has_relation<R: crate::relations::RelationKind>(&self, kind: R, target: Entity) -> bool {
        self.world.has_relation(self.entity, kind, target)
    }

    /// Target of the first relation `R` from this entity (e.g. its parent for
    /// [`ChildOf`](crate::relations::ChildOf)).
    pub fn target_of<R: crate::relations::RelationKind>(&self, kind: R) -> Option<Entity> {
        self.world.target_of(self.entity, kind)
    }

    /// Entities that relate to this one by `R` (e.g. its children for
    /// [`ChildOf`](crate::relations::ChildOf)).
    pub fn targets_of<R: crate::relations::RelationKind>(
        &self,
        kind: R,
    ) -> impl Iterator<Item = Entity> + '_ {
        self.world.targets_of(kind, self.entity)
    }
}

/// Full mutable access to a single entity — structural ops (insert / remove /
/// despawn), component mutation, relations, and hierarchy sugar. Obtained via
/// [`World::entity_mut`] / [`World::get_entity_mut`]. For read-only access use
/// [`EntityRef`] (via [`World::entity`]).
pub struct EntityWorldMut<'w> {
    world: &'w mut World,
    entity: Entity,
}

impl<'w> EntityWorldMut<'w> {
    /// Return the entity id.
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Check whether the entity is alive.
    pub fn is_alive(&self) -> bool {
        self.world.entities.is_alive(self.entity)
    }

    /// Insert a component into the entity.
    pub fn insert<T: Component>(&mut self, component: T) -> &mut Self {
        self.world.insert(self.entity, component);
        self
    }

    /// Remove a component of type T from the entity.
    pub fn remove<T: Component>(&mut self) -> bool {
        self.world.remove::<T>(self.entity)
    }

    /// Despawn the entity.
    pub fn despawn(&mut self) -> bool {
        self.world.despawn(self.entity)
    }

    /// Read component T.
    pub fn get<T: Component>(&self) -> Option<&T> {
        self.world.get::<T>(self.entity)
    }

    /// Read component T mutably (lazy change-detection — [`Mut<T>`], A13).
    pub fn get_mut<T: Component>(&mut self) -> Option<crate::query::Mut<'_, T>> {
        self.world.get_mut::<T>(self.entity)
    }

    /// Add a relation between this entity and target.
    pub fn add_relation<R: crate::relations::RelationKind>(
        &mut self,
        kind: R,
        target: Entity,
    ) -> &mut Self {
        self.world.add_relation(self.entity, kind, target);
        self
    }

    /// Remove a relation.
    pub fn remove_relation<R: crate::relations::RelationKind>(
        &mut self,
        kind: R,
        target: Entity,
    ) -> &mut Self {
        self.world.remove_relation(self.entity, kind, target);
        self
    }

    /// Check for a relation.
    pub fn has_relation<R: crate::relations::RelationKind>(&self, kind: R, target: Entity) -> bool {
        self.world.has_relation(self.entity, kind, target)
    }

    // ── Hierarchy sugar (immediate mirror of EntityCommands) ──────
    //
    // `EntityWorldMut` holds `&mut World`, so these apply the `ChildOf` relation
    // immediately (no deferral). Same semantics as the deferred
    // `EntityCommands::{set_parent,add_child,…}` — first-class relations UX in
    // both the immediate and command paths (§1.9 gap: sugar was Commands-only).

    /// Make this entity a child of `parent` (immediate [`ChildOf`](crate::relations::ChildOf)
    /// relation). Immediate mirror of
    /// [`EntityCommands::set_parent`](crate::commands::EntityCommands::set_parent).
    pub fn set_parent(&mut self, parent: Entity) -> &mut Self {
        self.world
            .add_relation(self.entity, crate::relations::ChildOf, parent);
        self
    }

    /// Adopt an EXISTING `child` (immediate `ChildOf` child → self). Mirror of
    /// [`EntityCommands::add_child`](crate::commands::EntityCommands::add_child).
    pub fn add_child(&mut self, child: Entity) -> &mut Self {
        self.world
            .add_relation(child, crate::relations::ChildOf, self.entity);
        self
    }

    /// Adopt a set of existing entities as children (see [`add_child`](Self::add_child)).
    pub fn add_children(&mut self, children: &[Entity]) -> &mut Self {
        for &child in children {
            self.world
                .add_relation(child, crate::relations::ChildOf, self.entity);
        }
        self
    }

    /// Detach this entity from its parent (drop its `ChildOf` relation), if any.
    /// Mirror of [`EntityCommands::remove_parent`](crate::commands::EntityCommands::remove_parent).
    pub fn remove_parent(&mut self) -> &mut Self {
        let entity = self.entity;
        if let Some(parent) = self.world.target_of(entity, crate::relations::ChildOf) {
            self.world
                .remove_relation(entity, crate::relations::ChildOf, parent);
        }
        self
    }

    /// Detach ALL children of this entity (drop their `ChildOf` → self) without
    /// despawning them. Mirror of
    /// [`EntityCommands::clear_children`](crate::commands::EntityCommands::clear_children).
    pub fn clear_children(&mut self) -> &mut Self {
        let parent = self.entity;
        let kids: Vec<Entity> = self
            .world
            .targets_of(crate::relations::ChildOf, parent)
            .collect();
        for child in kids {
            self.world
                .remove_relation(child, crate::relations::ChildOf, parent);
        }
        self
    }

    /// Spawn children of this entity declaratively (immediate mirror of
    /// [`EntityCommands::with_children`](crate::commands::EntityCommands::with_children)).
    /// Each `c.spawn(...)` immediately gets a
    /// [`ChildOf`](crate::relations::ChildOf) → this entity relation; nesting is
    /// arbitrary (a child's [`EntityWorldMut`] also has `with_children`).
    pub fn with_children(&mut self, f: impl FnOnce(&mut WorldChildSpawner)) -> &mut Self {
        let parent = self.entity;
        let mut spawner = WorldChildSpawner {
            world: &mut *self.world,
            parent,
        };
        f(&mut spawner);
        self
    }
}

/// Immediate child-spawner for [`EntityWorldMut::with_children`]. Each
/// [`spawn`](WorldChildSpawner::spawn) attaches a `ChildOf` → parent relation.
pub struct WorldChildSpawner<'w> {
    world: &'w mut World,
    parent: Entity,
}

impl WorldChildSpawner<'_> {
    /// Spawn a child (a `ChildOf` → parent relation is attached automatically).
    /// Returns the child's [`EntityWorldMut`] — nest another `with_children` on it.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> EntityWorldMut<'_> {
        let child = self.world.spawn(bundle);
        self.world
            .add_relation(child, crate::relations::ChildOf, self.parent);
        EntityWorldMut {
            world: self.world,
            entity: child,
        }
    }
}

impl World {
    /// Read-only accessor for `entity` (see [`EntityRef`]). Does not check that
    /// the entity is alive — reads on a stale id return `None`. For a checked
    /// lookup use [`get_entity`](World::get_entity); for mutation use
    /// [`entity_mut`](World::entity_mut).
    pub fn entity(&self, entity: Entity) -> EntityRef<'_> {
        EntityRef {
            world: self,
            entity,
        }
    }

    /// Full mutable accessor for `entity` (see [`EntityWorldMut`]). For a checked
    /// lookup use [`get_entity_mut`](World::get_entity_mut).
    pub fn entity_mut(&mut self, entity: Entity) -> EntityWorldMut<'_> {
        EntityWorldMut {
            world: self,
            entity,
        }
    }

    /// Read-only accessor, or `None` if the entity is not alive.
    pub fn get_entity(&self, entity: Entity) -> Option<EntityRef<'_>> {
        self.entities
            .is_alive(entity)
            .then_some(EntityRef { world: self, entity })
    }

    /// Full mutable accessor, or `None` if the entity is not alive.
    pub fn get_entity_mut(&mut self, entity: Entity) -> Option<EntityWorldMut<'_>> {
        self.entities
            .is_alive(entity)
            .then_some(EntityWorldMut { world: self, entity })
    }
}

// ── Scripting API ──────────────────────────────────────────────────────────
//
// Public accessors for apex-scripting.
// Separated from the main impl World to make clear: this is external API,
// not the world's internal logic.

impl World {
    /// Access to the entity allocator — for obtaining an Entity by index.
    ///
    /// Used by `despawn()` from Rhai scripts.
    #[inline]
    pub fn entity_allocator(&self) -> &crate::entity::EntityAllocator {
        &self.entities
    }

    /// Get the ComponentId by the type's string name.
    ///
    /// Used by `apex-scripting` to resolve names from scripts. The search is
    /// linear (O(N) over the number of registered components), but is called only
    /// at engine initialization — not in the hot path.
    pub fn component_id_by_name(&self, name: &str) -> Option<crate::component::ComponentId> {
        self.registry
            .iter()
            .find(|info| info.name == name)
            .map(|i| i.id)
    }

    // ── EntityTemplate API ────────────────────────────────────────

    /// Register a named entity template.
    pub fn register_template(
        &mut self,
        name: &str,
        template: impl crate::template::EntityTemplate + 'static,
    ) {
        self.templates.register(name, template);
    }

    /// Create an entity from a registered template with parameters.
    ///
    /// If the template returns `Some(parent)` from [`EntityTemplate::parent()`],
    /// then after spawn `ChildOf(parent)` is set automatically. The counterpart to
    /// [`spawn_template`](Self::spawn_template) (default parameters), following the
    /// `_with` canon (see `docs/CONVENTIONS.md`).
    pub fn spawn_template_with(
        &mut self,
        name: &str,
        params: &crate::template::TemplateParams,
    ) -> Option<crate::entity::Entity> {
        // Clone the Arc and release the registry borrow — now `&mut World` is free
        // for `spawn`, and the template survives even its own re-registration (B4).
        let template = self.templates.get_arc(name)?;
        let entity = template.spawn(self, params);
        if let Some(parent) = template.parent() {
            self.add_relation(entity, crate::relations::ChildOf, parent);
        }
        Some(entity)
    }

    /// Create an entity from a template with default parameters.
    pub fn spawn_template(&mut self, name: &str) -> Option<crate::entity::Entity> {
        self.spawn_template_with(name, &crate::template::TemplateParams::new())
    }

    /// Access to the template registry (read-only).
    pub fn template_registry(&self) -> &crate::template::TemplateRegistry {
        &self.templates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Score(u32);

    // ── W2-3: tick-wrap clamp ──────────────────────────────────

    #[test]
    fn check_change_ticks_clamps_stale_rows() {
        use crate::query::{Changed, Read};

        struct P(#[allow(dead_code)] f32);
        impl crate::component::Component for P {}

        let mut world = World::new();
        let _e = world.spawn((P(0.0),)); // the row's change-tick = current tick

        // Simulate long uptime: jumps < 2³¹ with a clamp between them — in total
        // far past the overflow period.
        for _ in 0..4 {
            world.current_tick.0 = world.current_tick.0.wrapping_add(1 << 30);
            world.check_change_ticks();
        }

        // last_run is "yesterday": the row is very old and must NOT be Changed.
        let last_run = Tick(world.current_tick.0.wrapping_sub(2));
        let changed = crate::query::Query::<(Changed<P>, Read<P>)>::new_with_tick(&world, last_run)
            .iter()
            .count();
        assert_eq!(changed, 0, "the clamp kept the row as 'unchanged for a long time'");

        // Control: WITHOUT the clamp the same scenario produces a false Changed.
        let mut world2 = World::new();
        let _e2 = world2.spawn((P(0.0),));
        world2.current_tick.0 = world2.current_tick.0.wrapping_add(1 << 31).wrapping_add(8);
        let last_run2 = Tick(world2.current_tick.0.wrapping_sub(2));
        let false_changed =
            crate::query::Query::<(Changed<P>, Read<P>)>::new_with_tick(&world2, last_run2)
                .iter()
                .count();
        assert_eq!(false_changed, 1, "sanity: without the clamp, wrap gives a false Changed");
    }

    // ── R-4: entity accessor ladder + immediate hierarchy sugar ──────

    #[test]
    fn entity_accessor_ladder_and_hierarchy_sugar() {
        use crate::relations::ChildOf;

        struct Tag(u32);
        impl crate::component::Component for Tag {}

        let mut world = World::new();
        let parent = world.spawn((Tag(0),));
        let c1 = world.spawn((Tag(1),));
        let c2 = world.spawn((Tag(2),));

        // entity_mut(): full mutable — immediate hierarchy sugar.
        world.entity_mut(parent).add_child(c1);
        world.entity_mut(parent).add_children(&[c2]);

        // entity(): read-only — relation navigation + component read.
        assert_eq!(world.entity(parent).targets_of(ChildOf).count(), 2);
        assert_eq!(world.entity(c1).target_of(ChildOf), Some(parent));
        assert!(world.entity(c1).has_relation(ChildOf, parent));
        assert_eq!(world.entity(c1).get::<Tag>().unwrap().0, 1);

        // set_parent re-links c1 to a fresh parent p2.
        let p2 = world.spawn((Tag(9),));
        world.entity_mut(c1).set_parent(p2);
        assert_eq!(world.entity(c1).target_of(ChildOf), Some(p2));

        // remove_parent detaches.
        world.entity_mut(c1).remove_parent();
        assert_eq!(world.entity(c1).target_of(ChildOf), None);

        // clear_children drops all of parent's remaining children (c2).
        world.entity_mut(parent).clear_children();
        assert_eq!(world.entity(parent).targets_of(ChildOf).count(), 0);

        // with_children: immediate declarative spawn with nesting.
        let root = world.spawn((Tag(100),));
        world.entity_mut(root).with_children(|c| {
            c.spawn((Tag(101),));
            c.spawn((Tag(102),)).with_children(|gc| {
                gc.spawn((Tag(103),));
            });
        });
        let root_kids: Vec<Entity> = world.entity(root).targets_of(ChildOf).collect();
        assert_eq!(root_kids.len(), 2, "with_children spawned 2 direct children");
        // The nested grandchild exists under the second child.
        let mid = root_kids
            .iter()
            .copied()
            .find(|&e| world.entity(e).get::<Tag>().unwrap().0 == 102)
            .unwrap();
        assert_eq!(world.entity(mid).targets_of(ChildOf).count(), 1, "grandchild nested");

        // Checked accessors: None on a despawned id.
        let dead = world.spawn((Tag(7),));
        world.despawn(dead);
        assert!(world.get_entity(dead).is_none());
        assert!(world.get_entity_mut(dead).is_none());
        assert!(world.get_entity(root).is_some());
    }

    /// A6 regression: a panic in `make_bundle(i)` mid `spawn_many` must roll the
    /// archetype back to a consistent state — `entities.len() == col.len` — not
    /// leave ghost entity rows over uninitialized column memory. Pre-fix the loop
    /// pushed one entity per iteration but bumped `col.len` only after the loop,
    /// so a mid-loop panic left `entities.len() > col.len` and later queries read
    /// uninitialized rows.
    #[test]
    fn spawn_many_panic_rolls_back_batch() {
        use crate::query::Read;

        #[derive(Debug)]
        struct P(#[allow(dead_code)] u32);
        impl crate::component::Component for P {}

        let mut world = World::new();
        world.spawn((P(1),));
        world.spawn((P(2),));
        let before = crate::query::Query::<Read<P>>::new(&world).iter().count();
        assert_eq!(before, 2);

        // `make_bundle` panics on the 4th element, after rows 0..3 were pushed.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            world.spawn_many(5, |i| {
                if i == 3 {
                    panic!("boom at {i}");
                }
                (P(100 + i as u32),)
            });
        }));
        assert!(res.is_err(), "the make_bundle panic must propagate");

        // The partial batch was rolled back — the archetype is consistent again.
        let after = crate::query::Query::<Read<P>>::new(&world).iter().count();
        assert_eq!(after, before, "partial spawn_many batch rolled back on panic");

        // The world remains fully usable after the rollback.
        world.spawn((P(9),));
        let n = crate::query::Query::<Read<P>>::new(&world).iter().count();
        assert_eq!(n, 3);
    }

    // ── W2-0: QueryState ───────────────────────────────────────

    #[test]
    fn query_state_incremental_and_reusable() {
        use crate::query::Read;

        struct P(f32);
        impl crate::component::Component for P {}
        struct Tag;
        impl crate::component::Component for Tag {}

        let mut world = World::new();
        world.spawn((P(1.0),));

        let mut state = QueryState::<Read<P>>::new();
        assert_eq!(state.query(&world).iter().count(), 1);

        // A new archetype AFTER the first query — the state is extended by the tail.
        world.spawn((P(2.0), Tag));
        assert_eq!(state.query(&world).iter().count(), 2);

        // A repeat call with no changes — a pure hit (nothing is rescanned).
        assert_eq!(state.query(&world).iter().count(), 2);
        let mut sum = 0.0;
        state.query(&world).for_each(|_, p| sum += p.0);
        assert_eq!(sum, 3.0);
    }

    #[test]
    fn query_state_rebinds_to_other_world() {
        use crate::query::Read;

        struct P(#[allow(dead_code)] f32);
        impl crate::component::Component for P {}

        let mut a = World::new();
        let mut b = World::new();
        a.spawn((P(0.0),));
        b.spawn((P(0.0),));
        b.spawn((P(0.0),));

        let mut state = QueryState::<Read<P>>::new();
        assert_eq!(state.query(&a).iter().count(), 1);
        assert_eq!(state.query(&b).iter().count(), 2, "the state rebound to world B");
        assert_eq!(state.query(&a).iter().count(), 1, "and back to A");
    }

    #[test]
    fn query_state_resolves_late_registered_component() {
        use crate::query::Read;

        struct Late(#[allow(dead_code)] u32);
        impl crate::component::Component for Late {}

        let mut world = World::new();
        let mut state = QueryState::<Read<Late>>::new();
        // The component is not yet registered — the query is empty but does not panic.
        assert_eq!(state.query(&world).iter().count(), 0);

        world.spawn((Late(7),));
        assert_eq!(state.query(&world).iter().count(), 1, "ids resolved after registration");
    }

    #[test]
    fn query_state_changed_with_explicit_tick() {
        use crate::query::{Changed, Read};

        struct P(f32);
        impl crate::component::Component for P {}

        let mut world = World::new();
        let target = world.spawn((P(0.0),));
        let _other = world.spawn((P(0.0),));

        world.tick();
        let last_run = world.current_tick();
        world.tick();

        if let Some(mut p) = world.get_mut::<P>(target) {
            p.0 = 1.0;
        }

        let mut state = QueryState::<(Entity, Changed<P>, Read<P>)>::new();
        let hits: Vec<_> = state
            .query_with_tick(&world, last_run)
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert_eq!(hits, vec![target]);
    }

    #[test]
    fn system_context_try_resource_some() {
        let mut world = World::new();
        world.insert_resource(Score(42));
        // SAFETY: the test holds the only reference to `world`; nothing conflicts.
        let sw = unsafe { crate::sub_world::SubWorld::new(&world, &[]) };
        let ctx = SystemContext::from_sub_world(&sw);

        let res = ctx.try_resource::<Score>();
        assert!(res.is_some());
        assert_eq!(*res.unwrap(), Score(42));
    }

    #[test]
    fn system_context_try_resource_none() {
        let world = World::new();
        // SAFETY: the test holds the only reference to `world`; nothing conflicts.
        let sw = unsafe { crate::sub_world::SubWorld::new(&world, &[]) };
        let ctx = SystemContext::from_sub_world(&sw);

        assert!(ctx.try_resource::<Score>().is_none());
    }

    #[test]
    fn system_context_try_resource_mut_some() {
        let mut world = World::new();
        world.insert_resource(Score(10));
        // SAFETY: the test holds the only reference to `world`; nothing conflicts.
        let sw = unsafe { crate::sub_world::SubWorld::new(&world, &[]) };
        let ctx = SystemContext::from_sub_world(&sw);

        let res_mut = ctx.try_resource_mut_unchecked::<Score>();
        assert!(res_mut.is_some());
        assert_eq!(*res_mut.unwrap(), Score(10));
    }

    #[test]
    fn system_context_try_resource_mut_none() {
        let world = World::new();
        // SAFETY: the test holds the only reference to `world`; nothing conflicts.
        let sw = unsafe { crate::sub_world::SubWorld::new(&world, &[]) };
        let ctx = SystemContext::from_sub_world(&sw);

        assert!(ctx.try_resource_mut_unchecked::<Score>().is_none());
    }

    #[test]
    fn adaptive_chunk_size_small_world() {
        let cfg = ChunkConfig::default();
        // entity_count < min_per_thread * threads → serial fallback (one chunk = entity_count)
        assert_eq!(adaptive_chunk_size(50, 8, &cfg), 50); // 50 < 16*8=128 → serial
        assert_eq!(adaptive_chunk_size(50, 4, &cfg), 50); // 50 < 16*4=64 → serial
        assert_eq!(adaptive_chunk_size(1, 8, &cfg), 1); // 1 < 128 → serial
        assert_eq!(adaptive_chunk_size(99, 8, &cfg), 99); // 99 < 128 → serial
    }

    #[test]
    fn adaptive_chunk_size_medium_world() {
        let cfg = ChunkConfig::default();
        // entity_count >= threshold → ceil(ec / threads), clamped to [dynamic_min_chunk=64, max]
        assert_eq!(adaptive_chunk_size(200, 8, &cfg), 64); // ceil(200/8)=25 < 64 → 64
        assert_eq!(adaptive_chunk_size(500, 8, &cfg), 64); // ceil(500/8)=63 < 64 → 64
        assert_eq!(adaptive_chunk_size(100, 8, &cfg), 100); // 100 < 128 → serial
    }

    #[test]
    fn adaptive_chunk_size_large_world() {
        let cfg = ChunkConfig::default();
        // task_multiplier=2.0 → targets=ceil(8*2)=16 → ceil(1000/16)=63 → clamp(63,64,65536)=64
        assert_eq!(adaptive_chunk_size(1000, 8, &cfg), 64);
        // targets=16 → ceil(10000/16)=625
        assert_eq!(adaptive_chunk_size(10000, 8, &cfg), 625);
    }

    #[test]
    fn adaptive_chunk_size_single_thread() {
        let cfg = ChunkConfig::default();
        // single thread → targets=1 (multiplier only for n>1)
        assert_eq!(adaptive_chunk_size(50, 1, &cfg), 50);
        assert_eq!(adaptive_chunk_size(200, 1, &cfg), 200);
        assert_eq!(adaptive_chunk_size(1000, 1, &cfg), 1000);
    }

    #[test]
    fn adaptive_chunk_size_max_cap() {
        let cfg = ChunkConfig::default();
        // single thread → targets=1 → ceil(131072/1)=131072 → cap 65536
        assert_eq!(
            adaptive_chunk_size(DEFAULT_MAX_CHUNK_SIZE * 2, 1, &cfg),
            DEFAULT_MAX_CHUNK_SIZE
        );
        // 8 threads → targets=16 → ceil(131072/16)=8192, within bounds
        assert_eq!(
            adaptive_chunk_size(DEFAULT_MAX_CHUNK_SIZE * 2, 8, &cfg),
            8192
        );
    }

    #[test]
    fn adaptive_chunk_size_transition_points() {
        let cfg = ChunkConfig::default();
        // 99 < 128 (16*8) → serial
        assert_eq!(adaptive_chunk_size(99, 8, &cfg), 99);
        // 100 < 128 → serial
        assert_eq!(adaptive_chunk_size(100, 8, &cfg), 100);
        // 999 >= 128 → targets=ceil(8*2.0)=16 → ceil(999/16)=63 → clamp(63,64,65536)=64
        assert_eq!(adaptive_chunk_size(999, 8, &cfg), 64);
        // 1000 >= 128 → targets=16 → ceil(1000/16)=63 → clamp=64
        assert_eq!(adaptive_chunk_size(1000, 8, &cfg), 64);
    }

    #[test]
    fn chunk_config_no_serial_fallback() {
        let cfg = ChunkConfig {
            min_entities_per_thread: 16,
            dynamic_min_chunk: 1,
            max_chunk_size: 4096,
            auto_serial_fallback: false,
            task_multiplier: 1.0,
            ..Default::default()
        };
        // auto_serial_fallback = false → always split into threads chunks
        // multiplier=1.0 → targets=8 → ceil(50/8)=7
        assert_eq!(adaptive_chunk_size(50, 8, &cfg), 7);
        // ceil(1/8) = 1
        assert_eq!(adaptive_chunk_size(1, 8, &cfg), 1);
    }

    #[test]
    fn chunk_config_custom_thresholds() {
        let cfg = ChunkConfig {
            min_entities_per_thread: 8,
            dynamic_min_chunk: 1,
            max_chunk_size: 8192,
            auto_serial_fallback: true,
            task_multiplier: 1.0,
            ..Default::default()
        };
        // 8 * 8 = 64 threshold
        assert_eq!(adaptive_chunk_size(50, 8, &cfg), 50); // 50 < 64 → serial
        assert_eq!(adaptive_chunk_size(100, 8, &cfg), 13); // 100 >= 64 → ceil(100/8)=13
    }

    // ── Bundle composition tests ─────────────────────────────────

    #[derive(Debug, PartialEq)]
    struct Pos {
        x: f32,
        y: f32,
    }
    impl crate::component::Component for Pos {}

    #[derive(Debug, PartialEq)]
    struct Vel {
        x: f32,
        y: f32,
    }
    impl crate::component::Component for Vel {}

    #[derive(Debug, PartialEq)]
    struct Hp(f32);
    impl crate::component::Component for Hp {}

    #[derive(Debug, PartialEq)]
    struct Armor(f32);
    impl crate::component::Component for Armor {}

    #[derive(Debug, PartialEq)]
    struct Team(u8);
    impl crate::component::Component for Team {}

    /// TD-1: `Query::iter()` must not skip the first archetype.
    #[test]
    fn cached_query_iter_does_not_skip_first_archetype() {
        use crate::query::Read;

        // One archetype (only Pos).
        let mut world = World::new();
        let a = world.spawn((Pos { x: 1.0, y: 0.0 },));
        let b = world.spawn((Pos { x: 2.0, y: 0.0 },));
        let got: Vec<_> = world
            .query_changed::<(Entity, Read<Pos>)>(Tick::ZERO)
            .iter()
            .map(|(e, _)| e)
            .collect();
        assert_eq!(got.len(), 2, "iter() must return BOTH entities (not skip the first)");
        assert!(got.contains(&a) && got.contains(&b));

        // Several archetypes: (Pos) and (Pos, Vel).
        let c = world.spawn((Pos { x: 3.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        let got2: Vec<_> = world
            .query_changed::<(Entity, Read<Pos>)>(Tick::ZERO)
            .iter()
            .map(|(e, _)| e)
            .collect();
        assert_eq!(got2.len(), 3, "iter() must cover all archetypes, including the first");
        assert!(got2.contains(&a) && got2.contains(&b) && got2.contains(&c));

        // for_each and iter produce the same set.
        let mut fe = 0usize;
        world.query_changed::<Read<Pos>>(Tick::ZERO).for_each(|_, _| fe += 1);
        assert_eq!(fe, got2.len(), "for_each and iter must be consistent");
    }

    /// CR-M2: `(Read<A>, Read<B>)` and `(Read<A>, Without<B>)` have the same
    /// fill_ids — the cache entries must NOT poison each other.
    #[test]
    fn cached_query_without_does_not_share_entry_with_read() {
        use crate::query::{Read, Without};

        let mut world = World::new();
        let _both = world.spawn((Pos { x: 1.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        let only_pos = world.spawn((Pos { x: 2.0, y: 0.0 },));

        // First warm the cache with the shape (Read, Read)…
        let with_vel = world.query::<(Read<Pos>, Read<Vel>)>().len();
        assert_eq!(with_vel, 1);

        // …then (Read, Without) must see ITS OWN archetype list.
        let mut seen = Vec::new();
        world
            .query::<(Read<Pos>, Without<Vel>)>()
            .for_each(|e, _| seen.push(e));
        assert_eq!(seen, vec![only_pos], "the Without shape must not share a cache entry with the Read shape");
    }

    /// CR-M2: a cache entry is incrementally extended with archetypes created
    /// AFTER the list was first built.
    #[test]
    fn cached_query_picks_up_new_archetypes() {
        use crate::query::Read;

        let mut world = World::new();
        world.spawn((Pos { x: 1.0, y: 0.0 },));
        assert_eq!(world.query::<Read<Pos>>().len(), 1);

        // A new archetype (Pos, Vel) after warming the cache.
        world.spawn((Pos { x: 2.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        assert_eq!(world.query::<Read<Pos>>().len(), 2);
    }

    /// CR-M2: an entity that moved into an EMPTIED archetype is not lost by the
    /// cache (empty archetypes stay in the lists; insert/remove do not reset the
    /// cache).
    #[test]
    fn cached_query_sees_entity_in_repopulated_archetype() {
        use crate::query::Read;

        let mut world = World::new();
        let e = world.spawn((Pos { x: 1.0, y: 0.0 }, Vel { x: 3.0, y: 0.0 }));
        assert_eq!(world.query::<(Read<Pos>, Read<Vel>)>().len(), 1);

        // The (Pos, Vel) archetype empties…
        world.remove::<Vel>(e);
        assert_eq!(world.query::<(Read<Pos>, Read<Vel>)>().len(), 0);

        // …and fills up again — the cached list must see it.
        world.insert(e, Vel { x: 4.0, y: 0.0 });
        let mut seen = Vec::new();
        world
            .query::<(Read<Pos>, Read<Vel>)>()
            .for_each(|ent, _| seen.push(ent));
        assert_eq!(seen, vec![e]);
    }

    /// CR-M2 (C-4): on a world with >128 archetypes, Query::new takes candidates
    /// from component_arch_index by the RAREST required component.
    #[test]
    fn query_new_candidates_from_rarest_component_on_large_world() {
        use crate::query::{Query, Read, With};

        struct F0;
        struct F1;
        struct F2;
        struct F3;
        struct F4;
        struct F5;
        struct F6;
        struct F7;
        struct Rare(#[allow(dead_code)] u32);
        impl crate::component::Component for F0 {}
        impl crate::component::Component for F1 {}
        impl crate::component::Component for F2 {}
        impl crate::component::Component for F3 {}
        impl crate::component::Component for F4 {}
        impl crate::component::Component for F5 {}
        impl crate::component::Component for F6 {}
        impl crate::component::Component for F7 {}
        impl crate::component::Component for Rare {}

        let mut world = World::new();
        let mut rare_holder = None;
        // 200 unique compositions → >128 archetypes (the candidate path).
        for i in 0..200u32 {
            let e = world.spawn((Pos { x: i as f32, y: 0.0 },));
            if i & 1 != 0 { world.insert(e, F0); }
            if i & 2 != 0 { world.insert(e, F1); }
            if i & 4 != 0 { world.insert(e, F2); }
            if i & 8 != 0 { world.insert(e, F3); }
            if i & 16 != 0 { world.insert(e, F4); }
            if i & 32 != 0 { world.insert(e, F5); }
            if i & 64 != 0 { world.insert(e, F6); }
            if i & 128 != 0 { world.insert(e, F7); }
            if i == 137 {
                world.insert(e, Rare(7));
                rare_holder = Some(e);
            }
        }
        assert!(world.archetype_count() > 128, "the test needs the candidate path");

        // Rare component: candidates = 1 archetype, the result is correct.
        let got: Vec<_> = Query::<(Entity, Read<Pos>, Read<Rare>)>::new(&world)
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert_eq!(got, vec![rare_holder.unwrap()]);

        // The With shape via the same path.
        let cnt = Query::<(Read<Pos>, With<Rare>)>::new(&world).iter().count();
        assert_eq!(cnt, 1);

        // A wide query on the same world — all 200 rows are present.
        let all = Query::<Read<Pos>>::new(&world).iter().count();
        assert_eq!(all, 200);
    }

    // Nested Bundle — manual implementation (proc-macros do not work inside apex-core)
    struct PlayerBase {
        pos: Pos,
        hp: Hp,
    }

    impl crate::Bundle for PlayerBase {
        fn component_count() -> usize {
            2
        }

        fn static_component_ids(
            registry: &mut crate::ComponentRegistry,
            out: &mut SmallVec<[crate::ComponentId; 8]>,
        ) {
            <Pos as crate::Bundle>::static_component_ids(registry, out);
            <Hp as crate::Bundle>::static_component_ids(registry, out);
        }

        fn write_into(
            self,
            world: &mut crate::World,
            archetype_id: crate::ArchetypeId,
            row: usize,
            tick: crate::Tick,
        ) {
            crate::Bundle::write_into(self.pos, world, archetype_id, row, tick);
            crate::Bundle::write_into(self.hp, world, archetype_id, row, tick);
        }

        fn needs_drop() -> bool {
            false || <Pos as crate::Bundle>::needs_drop() || <Hp as crate::Bundle>::needs_drop()
        }
    }

    struct ArmedPlayer {
        base: PlayerBase,
        weapon: Vel,
        armor: Armor,
    }

    impl crate::Bundle for ArmedPlayer {
        fn component_count() -> usize {
            4
        }

        fn static_component_ids(
            registry: &mut crate::ComponentRegistry,
            out: &mut SmallVec<[crate::ComponentId; 8]>,
        ) {
            <PlayerBase as crate::Bundle>::static_component_ids(registry, out);
            <Vel as crate::Bundle>::static_component_ids(registry, out);
            <Armor as crate::Bundle>::static_component_ids(registry, out);
        }

        fn write_into(
            self,
            world: &mut crate::World,
            archetype_id: crate::ArchetypeId,
            row: usize,
            tick: crate::Tick,
        ) {
            crate::Bundle::write_into(self.base, world, archetype_id, row, tick);
            crate::Bundle::write_into(self.weapon, world, archetype_id, row, tick);
            crate::Bundle::write_into(self.armor, world, archetype_id, row, tick);
        }

        fn needs_drop() -> bool {
            false
                || <PlayerBase as crate::Bundle>::needs_drop()
                || <Vel as crate::Bundle>::needs_drop()
                || <Armor as crate::Bundle>::needs_drop()
        }
    }

    #[test]
    fn bundle_nested_struct_spawn() {
        let mut world = World::new();
        let e = world.spawn(ArmedPlayer {
            base: PlayerBase {
                pos: Pos { x: 10.0, y: 20.0 },
                hp: Hp(100.0),
            },
            weapon: Vel { x: 1.0, y: 0.5 },
            armor: Armor(50.0),
        });

        // All components are present
        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 10.0, y: 20.0 }));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(100.0)));
        assert_eq!(world.get::<Vel>(e), Some(&Vel { x: 1.0, y: 0.5 }));
        assert_eq!(world.get::<Armor>(e), Some(&Armor(50.0)));
        assert!(world.get::<Team>(e).is_none());
    }

    #[test]
    fn bundle_tuple_of_bundles_spawn() {
        let mut world = World::new();
        let e = world.spawn((
            PlayerBase {
                pos: Pos { x: 1.0, y: 2.0 },
                hp: Hp(75.0),
            },
            Vel { x: 3.0, y: 4.0 },
            Team(1),
        ));

        // A tuple of a Bundle struct + components works
        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 1.0, y: 2.0 }));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(75.0)));
        assert_eq!(world.get::<Vel>(e), Some(&Vel { x: 3.0, y: 4.0 }));
        assert_eq!(world.get::<Team>(e), Some(&Team(1)));
        assert!(world.get::<Armor>(e).is_none());
    }

    #[test]
    fn bundle_single_component_direct_spawn() {
        let mut world = World::new();
        // Component directly in spawn (blanket impl<T: Component> Bundle for T)
        let e = world.spawn(Pos { x: 5.0, y: 6.0 });
        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 5.0, y: 6.0 }));
    }

    #[test]
    fn bundle_mixed_tuple_of_components_and_bundles() {
        let mut world = World::new();
        // Mix: single components + a Bundle struct + one more component (without
        // duplicate components between the tuple and the nested Bundle).
        let e = world.spawn((
            PlayerBase {
                pos: Pos { x: 7.0, y: 8.0 },
                hp: Hp(80.0),
            },
            Armor(30.0),
            Team(2),
        ));

        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 7.0, y: 8.0 }));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(80.0)));
        assert_eq!(world.get::<Armor>(e), Some(&Armor(30.0)));
        assert_eq!(world.get::<Team>(e), Some(&Team(2)));
    }

    /// A1: a component appearing both in the tuple AND inside a nested Bundle is
    /// a duplicate — the old behavior silently built a phantom second column that
    /// dropped through a null pointer on despawn. It must be rejected loudly.
    #[test]
    #[should_panic(expected = "duplicate component")]
    fn bundle_duplicate_across_tuple_and_nested_bundle_panics() {
        let mut world = World::new();
        let _ = world.spawn((
            Hp(200.0), // <- also inside PlayerBase below
            PlayerBase {
                pos: Pos { x: 7.0, y: 8.0 },
                hp: Hp(80.0),
            },
            Armor(30.0),
        ));
    }

    #[test]
    fn bundle_spawn_many_with_bundle_struct() {
        let mut world = World::new();
        // spawn_many works with nested Bundle (bulk-copy when needs_drop() == false)
        let entities = world.spawn_many(10, |_| ArmedPlayer {
            base: PlayerBase {
                pos: Pos { x: 50.0, y: 50.0 },
                hp: Hp(100.0),
            },
            weapon: Vel { x: 0.1, y: 0.0 },
            armor: Armor(10.0),
        });

        assert_eq!(entities.len(), 10);
        // Check via direct get, not via query
        for &e in &entities {
            assert!(world.get::<Pos>(e).is_some(), "Entity {:?} missing Pos", e);
            assert!(world.get::<Hp>(e).is_some(), "Entity {:?} missing Hp", e);
            assert!(world.get::<Vel>(e).is_some(), "Entity {:?} missing Vel", e);
            assert!(
                world.get::<Armor>(e).is_some(),
                "Entity {:?} missing Armor",
                e
            );
        }
    }

    /// Regression: `spawn_many`/bulk-path must write components to the COLUMN BY
    /// THEIR ID, not positionally in declaration order. The bug (before the
    /// col_indices fix): `col_indices` was built from SORTED ids, but
    /// `write_into_batch` consumed them in DECLARATION order ⇒ when "declaration
    /// order ≠ id order" a component was written to the wrong column (UB: 64B into
    /// a 1B column, data corruption — manifested as a heavy_compute regression).
    /// Here the declaration order (Big, Small) is the REVERSE of the id order
    /// (Small registered first ⇒ smaller id).
    #[test]
    fn spawn_many_writes_components_by_id_not_declaration_position() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct BigComp([u64; 8]); // 64 bytes
        impl Component for BigComp {}
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct SmallComp(u8); // 1 byte
        impl Component for SmallComp {}

        let mut world = World::new();
        // Small BEFORE Big ⇒ id(Small) < id(Big). The bundle declaration order is the REVERSE.
        world.register_component::<SmallComp>();
        world.register_component::<BigComp>();

        let entities = world.spawn_many(256, |i| (BigComp([i as u64; 8]), SmallComp(0xAB)));
        assert_eq!(entities.len(), 256);

        for (i, &e) in entities.iter().enumerate() {
            let big = world.get::<BigComp>(e).expect("BigComp present");
            let small = world.get::<SmallComp>(e).expect("SmallComp present");
            assert_eq!(
                big.0, [i as u64; 8],
                "BigComp entity[{i}] corrupted — component written to the wrong column (col_indices order)"
            );
            assert_eq!(small.0, 0xAB, "SmallComp entity[{i}] corrupted");
        }
    }

    #[test]
    fn bundle_spawn_batch_heterogeneous_bundles() {
        let mut world = World::new();
        // Different spawn approaches in one test
        let boss = world.spawn(ArmedPlayer {
            base: PlayerBase {
                pos: Pos { x: 1.0, y: 1.0 },
                hp: Hp(50.0),
            },
            weapon: Vel { x: 0.0, y: 0.0 },
            armor: Armor(10.0),
        });
        let minion = world.spawn((Pos { x: 2.0, y: 2.0 }, Hp(25.0), Team(3)));
        let empty = world.spawn(());

        assert!(world.has_component::<Pos>(boss));
        assert_eq!(world.get::<Armor>(boss), Some(&Armor(10.0)));
        assert!(world.has_component::<Pos>(minion));
        assert_eq!(world.get::<Team>(minion), Some(&Team(3)));
        assert!(!world.has_component::<Pos>(empty));
    }
}

// ── W3-1: hooks/observers + Added/Removed ──────────────────────

#[cfg(test)]
mod hooks_and_added_tests {
    use super::*;
    use crate::query::{Added, Changed, Query, Read};

    #[derive(Debug, PartialEq)]
    struct Hp(u32);
    impl Component for Hp {}

    #[derive(Debug, PartialEq)]
    struct Armor(u32);
    impl Component for Armor {}

    /// Log of hook calls (a resource — the subscriber's state lives in resources).
    #[derive(Default)]
    struct HookLog {
        added: Vec<Entity>,
        removed: Vec<Entity>,
        removed_alive: Vec<bool>,
        rel_added: Vec<(Entity, Entity)>,
        rel_removed: Vec<(Entity, Entity)>,
    }

    fn log_world() -> World {
        let mut w = World::new();
        w.insert_resource(HookLog::default());
        w
    }

    // ── Added<T> ───────────────────────────────────────────────

    #[test]
    fn added_detects_fresh_spawn_and_expires_next_frame() {
        let mut world = World::new();
        world.spawn((Hp(1),));

        let lr = world.last_run_tick();
        let n = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 1, "a fresh spawn is visible to Added<T>");

        world.advance_change_tick();
        let lr = world.last_run_tick();
        let n = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 0, "on the next frame Added<T> expires");
    }

    #[test]
    fn added_survives_archetype_move_and_not_retriggered() {
        let mut world = World::new();
        let e = world.spawn((Hp(1),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        // insert Armor moves the entity into a new archetype: Added<Armor> — yes,
        // Added<Hp> — NO (the added-tick survived the move).
        world.insert(e, Armor(5));
        let added_armor = Query::<(Added<Armor>, Read<Armor>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        let added_hp = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(added_armor, 1);
        assert_eq!(added_hp, 0, "an archetype move does not 'update' Added");
    }

    #[test]
    fn reinsert_existing_is_changed_but_not_added() {
        let mut world = World::new();
        let e = world.spawn((Hp(1),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        world.insert(e, Hp(2)); // replace of an existing one
        let added = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        let changed = Query::<(Changed<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(added, 0, "replace does not re-trigger Added (like Bevy)");
        assert_eq!(changed, 1, "replace marks Changed");
        assert_eq!(world.get::<Hp>(e), Some(&Hp(2)));
    }

    #[test]
    fn added_alignment_survives_swap_remove() {
        let mut world = World::new();
        let e0 = world.spawn((Hp(0),));
        let _e1 = world.spawn((Hp(1),));
        let _e2 = world.spawn((Hp(2),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        let e3 = world.spawn((Hp(3),)); // the only "fresh" one
        world.despawn(e0); // swap_remove: e3 moves to row 0

        let fresh: Vec<Entity> = Query::<(Entity, Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert_eq!(
            fresh,
            vec![e3],
            "swap_remove preserves added-tick alignment"
        );
    }

    // ── on_add / on_remove ─────────────────────────────────────

    #[test]
    fn on_add_fires_for_spawn_insert_and_commands_burst() {
        let mut world = log_world();
        world.on_add::<Hp>(|w, e| w.resource_mut::<HookLog>().added.push(e));

        let a = world.spawn((Hp(1),)); // spawn
        let b = world.spawn((Armor(0),));
        world.insert(b, Hp(2)); // insert (archetype move)

        // Commands burst → insert_parts (the batch path W2-1).
        let c = world.spawn((Armor(0),));
        let mut cmds = Commands::new();
        cmds.insert(c, Hp(3));
        cmds.insert(c, Armor(1)); // replace — NOT on_add
        cmds.apply(&mut world);

        assert_eq!(world.resource::<HookLog>().added, vec![a, b, c]);
    }

    #[test]
    fn on_add_fires_for_spawn_many() {
        let mut world = log_world();
        world.on_add::<Hp>(|w, e| w.resource_mut::<HookLog>().added.push(e));
        let spawned = world.spawn_many(3, |i| (Hp(i as u32),));
        assert_eq!(world.resource::<HookLog>().added, spawned);
    }

    /// A7: a panic in a user hook must not leave dispatch permanently disabled —
    /// the guard resets the flag and clears the queue on unwind, so later hooks
    /// still fire.
    #[test]
    fn panicking_hook_does_not_permanently_disable_dispatch() {
        let mut world = log_world();
        world.on_add::<Hp>(|_w, _e| panic!("boom in hook"));
        world.on_add::<Armor>(|w, e| w.resource_mut::<HookLog>().added.push(e));

        // Spawning Hp fires the panicking hook.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            world.spawn((Hp(1),));
        }));
        assert!(res.is_err(), "the on_add hook panicked");
        assert!(
            !world.hook_dispatch_active,
            "dispatch flag must be reset after the panic (A7)"
        );

        // A later hook must still fire — dispatch is not permanently disabled.
        let a = world.spawn((Armor(0),));
        assert_eq!(
            world.resource::<HookLog>().added,
            vec![a],
            "hooks must still work after a prior hook panicked"
        );
    }

    #[test]
    fn on_add_hook_can_do_structural_changes() {
        // The hook pulls in Armor for everyone who got Hp (a precursor to required
        // components D2-4). The nested insert goes through the same queue.
        let mut world = World::new();
        world.on_add::<Hp>(|w, e| {
            if !w.has_component::<Armor>(e) {
                w.insert(e, Armor(100));
            }
        });
        let e = world.spawn((Hp(1),));
        assert_eq!(world.get::<Armor>(e), Some(&Armor(100)));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(1)));
    }

    #[test]
    fn on_remove_fires_for_remove_and_despawn_with_dead_entity() {
        let mut world = log_world();
        world.on_remove::<Hp>(|w, e| {
            let alive = w.is_alive(e);
            let log = w.resource_mut::<HookLog>();
            log.removed.push(e);
            log.removed_alive.push(alive);
        });

        let a = world.spawn((Hp(1),));
        world.remove::<Hp>(a); // remove: entity is alive
        let b = world.spawn((Hp(2),));
        world.despawn(b); // despawn: entity is dead

        let log = world.resource::<HookLog>();
        assert_eq!(log.removed, vec![a, b]);
        assert_eq!(log.removed_alive, vec![true, false]);
    }

    #[test]
    fn on_remove_fires_for_cascade_despawn() {
        let mut world = log_world();
        world.on_remove::<Hp>(|w, e| w.resource_mut::<HookLog>().removed.push(e));

        let parent = world.spawn((Armor(0),));
        let child = world.spawn((Hp(1),));
        world.add_relation(child, crate::relations::ChildOf, parent);

        world.despawn(parent); // the cascade removes child
        assert!(!world.is_alive(child));
        assert_eq!(world.resource::<HookLog>().removed, vec![child]);
    }

    #[test]
    #[should_panic(expected = "is already registered")]
    fn double_on_add_registration_panics() {
        let mut world = World::new();
        world.on_add::<Hp>(|_, _| {});
        world.on_add::<Hp>(|_, _| {});
    }

    // ── Relation hooks ─────────────────────────────────────────

    #[test]
    fn relation_hooks_fire_on_add_remove_and_despawn_cleanup() {
        let mut world = log_world();
        world.on_relation_add::<crate::relations::Owns>(|w, s, t| {
            w.resource_mut::<HookLog>().rel_added.push((s, t))
        });
        world.on_relation_remove::<crate::relations::Owns>(|w, s, t| {
            w.resource_mut::<HookLog>().rel_removed.push((s, t))
        });

        let owner = world.spawn((Hp(1),));
        let item = world.spawn((Armor(0),));
        world.add_relation(owner, crate::relations::Owns, item);
        world.remove_relation(owner, crate::relations::Owns, item);

        // The relation again — cleanup via despawn of the target.
        world.add_relation(owner, crate::relations::Owns, item);
        world.despawn(item);

        let log = world.resource::<HookLog>();
        assert_eq!(log.rel_added, vec![(owner, item), (owner, item)]);
        assert_eq!(
            log.rel_removed,
            vec![(owner, item), (owner, item)],
            "explicit remove + despawn cleanup"
        );
    }

    // ── track_removals / Removed<T> ────────────────────────────

    #[test]
    fn removed_events_emitted_for_remove_and_despawn() {
        let mut world = World::new();
        world.track_removals::<Hp>();

        let a = world.spawn((Hp(1),));
        let b = world.spawn((Hp(2),));
        world.remove::<Hp>(a);
        world.despawn(b);

        world.flush_all_events();
        let mut reader = world.event_reader::<crate::events::Removed<Hp>>();
        let got: Vec<Entity> = reader.read().iter().map(|r| r.entity).collect();
        assert_eq!(got, vec![a, b]);
    }

    // ── D2-4: required components ──────────────────────────────

    #[test]
    fn required_components_via_derive_attr() {
        #[derive(apex_macros::Component, Default, Debug, PartialEq)]
        struct LocalTf(u32);
        #[derive(apex_macros::Component, Default, Debug, PartialEq)]
        struct GlobalTf(u32);

        #[derive(apex_macros::Component)]
        #[require(LocalTf, GlobalTf)]
        struct Renderer;

        let mut world = World::new(); // derive registrars via linkme
        let e = world.spawn((Renderer,));
        assert_eq!(
            world.get::<LocalTf>(e),
            Some(&LocalTf(0)),
            "the missing required was pulled in via default"
        );
        assert_eq!(world.get::<GlobalTf>(e), Some(&GlobalTf(0)));

        // An explicitly given value wins over the default.
        let e2 = world.spawn((Renderer, LocalTf(7)));
        assert_eq!(world.get::<LocalTf>(e2), Some(&LocalTf(7)));
        assert_eq!(world.get::<GlobalTf>(e2), Some(&GlobalTf(0)));

        // The insert path also pulls it in.
        let e3 = world.spawn((Hp(1),));
        world.insert(e3, Renderer);
        assert_eq!(world.get::<GlobalTf>(e3), Some(&GlobalTf(0)));
    }

    #[test]
    fn required_components_transitive_and_manual_api() {
        // C requires B, B requires A — the manual API (for types with a manual
        // impl Component, as in the engine).
        #[derive(Default, Debug, PartialEq)]
        struct A(u8);
        impl Component for A {}
        #[derive(Default, Debug, PartialEq)]
        struct B(u8);
        impl Component for B {}
        struct C;
        impl Component for C {}

        let mut world = World::new();
        world.require_component::<C, B>();
        world.require_component::<B, A>();

        let e = world.spawn((C,));
        assert_eq!(world.get::<B>(e), Some(&B(0)), "direct requirement");
        assert_eq!(world.get::<A>(e), Some(&A(0)), "transitive via the queue");
    }

    #[test]
    fn required_components_user_on_add_sees_full_entity() {
        struct C;
        impl Component for C {}
        #[derive(Default)]
        struct R(#[allow(dead_code)] u8);
        impl Component for R {}

        let mut world = log_world();
        world.require_component::<C, R>();
        // The owner's on_add is called AFTER the requires are pulled in.
        world.on_add::<C>(|w, e| {
            assert!(
                w.has_component::<R>(e),
                "the required component is already present at the on_add call"
            );
            w.resource_mut::<HookLog>().added.push(e);
        });
        let e = world.spawn((C,));
        assert_eq!(world.resource::<HookLog>().added, vec![e]);
    }

    // ── W3-5: memory in archetype_stats ────────────────────────

    #[test]
    fn archetype_stats_reports_memory() {
        let mut world = World::new();
        world.spawn_many(100, |i| (Hp(i as u32),));
        let s = world.archetype_stats();
        assert!(s.component_bytes >= 100 * std::mem::size_of::<Hp>());
        assert!(s.tick_bytes >= 100 * 2 * std::mem::size_of::<Tick>()); // change + added
        assert!(s.entity_bytes >= 100 * std::mem::size_of::<Entity>());
        assert_eq!(
            s.total_bytes(),
            s.component_bytes + s.tick_bytes + s.entity_bytes
        );
    }

    #[test]
    fn untracked_component_emits_nothing() {
        let mut world = World::new();
        world.track_removals::<Hp>();
        let e = world.spawn((Armor(1),));
        world.despawn(e); // Armor is not tracked

        world.flush_all_events();
        let mut reader = world.event_reader::<crate::events::Removed<Hp>>();
        assert_eq!(reader.read().iter().count(), 0);
    }

    // ── Soundness regressions (CORE_AUDIT wave 1) ──────────────

    /// A1: a bundle listing the same component twice would build an archetype
    /// with a phantom second column (`len=0, data=null`) that drops through a
    /// null pointer on despawn. Must panic loudly at spawn instead.
    #[test]
    #[should_panic(expected = "duplicate component")]
    fn spawn_duplicate_component_in_bundle_panics() {
        let mut world = World::new();
        let _ = world.spawn((Hp(1), Hp(2)));
    }

    /// B8: `insert_raw` copies `component.size` bytes from the buffer — a short
    /// buffer would read past it (OOB). The length is validated loudly.
    #[test]
    #[should_panic(expected = "component size")]
    fn insert_raw_wrong_size_panics() {
        let mut world = World::new();
        let cid = world.register_component::<Hp>();
        let e = world.spawn((Armor(0),));
        let tick = world.current_tick();
        world.insert_raw(e, cid, vec![0u8; 1], tick); // Hp is 4 bytes
    }

    /// A9: `insert_raw` on a dead entity must run the component's drop_fn — the
    /// bytes are a moved-out `T`; dropping the raw `Vec<u8>` alone would leak
    /// `T`'s owned fields.
    #[test]
    fn insert_raw_on_dead_entity_drops_value_not_leaks() {
        use std::sync::Arc;
        struct DropComp(#[allow(dead_code)] Arc<()>);
        impl Component for DropComp {}

        let mut world = World::new();
        let cid = world.register_component::<DropComp>();

        let dead = world.spawn((Armor(0),));
        world.despawn(dead); // handle is now stale → insert_raw takes the dead path

        let probe = Arc::new(());
        let comp = DropComp(probe.clone());
        let size = std::mem::size_of::<DropComp>();
        let bytes = unsafe {
            let mut v = vec![0u8; size];
            std::ptr::copy_nonoverlapping(&comp as *const DropComp as *const u8, v.as_mut_ptr(), size);
            std::mem::forget(comp); // ownership moves into the bytes
            v
        };
        assert_eq!(Arc::strong_count(&probe), 2);
        let tick = world.current_tick();
        world.insert_raw(dead, cid, bytes, tick);
        assert_eq!(
            Arc::strong_count(&probe),
            1,
            "insert_raw on a dead entity must drop the value, not leak it"
        );
    }

    /// B4: a template that re-registers itself from inside its own `spawn` must
    /// not free the handle underfoot. The registry hands out an `Arc` clone, so
    /// the running template stays alive across the re-registration.
    #[test]
    fn template_can_reregister_itself_during_spawn() {
        use crate::template::{EntityTemplate, TemplateParams};

        struct SelfReplacing;
        impl EntityTemplate for SelfReplacing {
            fn spawn(&self, world: &mut World, _p: &TemplateParams) -> Entity {
                // Replaces the map entry mid-spawn — with the old Box+raw-pointer
                // path this dropped `self` underfoot (UAF).
                world.register_template("t", SelfReplacing);
                world.spawn((Hp(7),))
            }
        }

        let mut world = World::new();
        world.register_template("t", SelfReplacing);
        let e = world.spawn_template("t").unwrap();
        assert!(world.is_alive(e));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(7)));
    }
}

#[cfg(test)]
mod loudness_wave4 {
    //! Regression tests for the wave-4 §0.2a loudness pass: misuse paths that
    //! used to be silent no-ops now surface a throttled `warn_once!`, and the
    //! A10 re-spawn footgun refuses instead of orphaning a row.
    use super::*;
    use crate::commands::Commands;
    use std::sync::{Mutex, OnceLock};

    #[derive(Debug, PartialEq)]
    struct Comp(u32);
    impl Component for Comp {}

    // ── Capturing logger (Warn level), keyed by unique markers ─────────────
    // `warn_once!` fires once per process per call site, so tests can't rely on
    // ordering — instead each assertion greps for a marker only it emits.
    static LOG_BUF: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    fn log_buf() -> &'static Mutex<Vec<String>> {
        LOG_BUF.get_or_init(|| Mutex::new(Vec::new()))
    }
    struct CapLogger;
    impl log::Log for CapLogger {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            if r.level() == log::Level::Warn {
                log_buf().lock().unwrap().push(r.args().to_string());
            }
        }
        fn flush(&self) {}
    }
    static CAP_LOGGER: CapLogger = CapLogger;
    fn install_logger() {
        static INIT: OnceLock<bool> = OnceLock::new();
        INIT.get_or_init(|| {
            log::set_max_level(log::LevelFilter::Warn);
            log::set_logger(&CAP_LOGGER).is_ok()
        });
    }
    fn count(needle: &str) -> usize {
        log_buf()
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.contains(needle))
            .count()
    }

    /// `warn_once!` emits exactly once per call site regardless of hit count.
    #[test]
    fn warn_once_throttles_to_single_emission() {
        install_logger();
        for _ in 0..8 {
            crate::warn_once!("APEX_LOUD_MARKER_c1p7 repeated");
        }
        assert_eq!(
            count("APEX_LOUD_MARKER_c1p7"),
            1,
            "warn_once must fire at most once per call site"
        );
    }

    /// A10 (debug): spawning onto an already-live entity is rejected by the
    /// guard. Without it, the second spawn silently orphans the entity's row.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "already-live")]
    fn a10_spawn_at_on_live_entity_is_rejected() {
        let mut world = World::new();
        let e = world.spawn((Comp(1),));
        world.spawn_reserved(e, (Comp(2),));
    }

    /// A10 (release): the guard refuses without corrupting the world — the live
    /// entity keeps its original component and the entity count is unchanged.
    #[test]
    #[cfg(not(debug_assertions))]
    fn a10_spawn_at_on_live_entity_refused_release() {
        let mut world = World::new();
        let e = world.spawn((Comp(1),));
        let before = world.entity_count();
        world.spawn_reserved(e, (Comp(2),));
        assert_eq!(world.entity_count(), before);
        assert_eq!(world.get::<Comp>(e), Some(&Comp(1)));
    }

    /// B7: a double-despawn queued in Commands applies cleanly — the second
    /// despawn is a no-op and leaves the world consistent.
    #[test]
    fn b7_commands_double_despawn_is_clean_noop() {
        let mut world = World::new();
        let e = world.spawn((Comp(1),));
        let mut cmds = Commands::new();
        cmds.despawn(e);
        cmds.despawn(e);
        cmds.apply(&mut world);
        assert!(!world.is_alive(e));
        assert_eq!(world.entity_count(), 0);
    }

    /// B10: a queued spawn of an unregistered template name spawns nothing and
    /// does not disturb the world.
    #[test]
    fn b10_commands_unknown_template_spawns_nothing() {
        let mut world = World::new();
        let before = world.entity_count();
        let mut cmds = Commands::new();
        cmds.spawn_template("does-not-exist");
        cmds.apply(&mut world);
        assert_eq!(world.entity_count(), before);
    }
}

#[cfg(test)]
mod wave5_par_split {
    //! Correctness of the wave-5 adaptive-split `par_for_each_split` (§7 A/B
    //! candidate): it must visit every matched entity exactly once and produce
    //! results identical to the fixed-chunk `par_for_each`, on a SKEWED archetype
    //! distribution (one big + several small archetypes — where split's
    //! cross-archetype balancing differs from per-archetype chunking).
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct N(u32);
    impl Component for N {}
    struct MA;
    impl Component for MA {}
    struct MB;
    impl Component for MB {}
    struct MC;
    impl Component for MC {}

    fn skewed_world() -> (World, usize) {
        let mut world = World::new();
        for i in 0..4000u32 {
            world.spawn((N(i),));
        } // big archetype
        for i in 0..50u32 {
            world.spawn((N(i), MA));
        } // small
        for i in 0..13u32 {
            world.spawn((N(i), MB));
        } // tiny
        for i in 0..1u32 {
            world.spawn((N(i), MC));
        } // singleton
        (world, 4000 + 50 + 13 + 1)
    }

    #[test]
    fn par_split_visits_every_entity_exactly_once() {
        let (world, total) = skewed_world();
        let count = AtomicUsize::new(0);
        world.query::<crate::query::Read<N>>().par_for_each(|_, _| {
            count.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(count.load(Ordering::Relaxed), total);
    }

    #[test]
    fn par_split_write_matches_sequential() {
        // Parallel (adaptive-split) mutation.
        let (mut wp, _) = skewed_world();
        wp.query_mut::<crate::query::Write<N>>()
            .par_for_each_mut(|_, mut n| n.0 = n.0.wrapping_mul(2).wrapping_add(1));
        let mut par: Vec<u32> = wp
            .query::<crate::query::Read<N>>()
            .iter()
            .map(|n| n.0)
            .collect();
        par.sort_unstable();

        // Sequential reference on an identically-built world.
        let (mut ws, _) = skewed_world();
        ws.query_mut::<crate::query::Write<N>>()
            .for_each_mut(|_, mut n| n.0 = n.0.wrapping_mul(2).wrapping_add(1));
        let mut seq: Vec<u32> = ws
            .query::<crate::query::Read<N>>()
            .iter()
            .map(|n| n.0)
            .collect();
        seq.sort_unstable();

        assert_eq!(
            par, seq,
            "split par_for_each must mutate each entity exactly like sequential"
        );
    }
}

#[cfg(test)]
mod wave6_bundle {
    //! §10.10: `spawn_many` takes the bundle composition STATICALLY
    //! (`Bundle::static_component_ids`), so the `make_bundle` closure is called
    //! exactly `count` times — no extra `make_bundle(0)` probe. This removes the
    //! footgun that the closure had to be pure.
    use super::*;

    #[derive(Debug, PartialEq)]
    struct C(u32);
    impl Component for C {}

    #[test]
    fn spawn_many_calls_make_bundle_exactly_count_times() {
        let mut world = World::new();
        let mut calls = 0usize;
        let entities = world.spawn_many(5, |i| {
            calls += 1;
            C(i as u32)
        });
        assert_eq!(entities.len(), 5);
        assert_eq!(
            calls, 5,
            "make_bundle must run exactly `count` times (no make_bundle(0) probe)"
        );
        // Per-entity data is intact (probe removal did not disturb the loop).
        for (i, &e) in entities.iter().enumerate() {
            assert_eq!(world.get::<C>(e), Some(&C(i as u32)));
        }
    }

    /// E6: `map_entity_refs` rewrites the registered component's Entity fields in
    /// place through the raw `MapEntitiesFn` path (Miri-checked unsafe).
    #[test]
    fn e6_map_entity_refs_remaps_in_place() {
        #[derive(Debug, PartialEq)]
        struct Link(Entity);
        impl Component for Link {}
        impl crate::component::MapEntities for Link {
            fn map_entities(&mut self, f: &mut dyn FnMut(Entity) -> Entity) {
                self.0 = f(self.0);
            }
        }

        let mut world = World::new();
        world.register_map_entities::<Link>();
        let a = world.spawn(());
        let b = world.spawn(());
        let e = world.spawn((Link(a),));
        assert_eq!(world.get::<Link>(e).unwrap().0, a);

        let mut remap = |old: Entity| if old == a { b } else { old };
        world.map_entity_refs(e, &mut remap);
        assert_eq!(world.get::<Link>(e).unwrap().0, b, "ref remapped a -> b");
    }
}
