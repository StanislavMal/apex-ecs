use super::*;

impl Scheduler {
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            system_indices: FxHashMap::default(),
            next_id: 0,
            execution_plan: None,
            stage_last_run: Vec::new(),
            stage_cost_ema_ns: Vec::new(),
            stage_ran_seq: Vec::new(),
            dependency_graph: Graph::new(),
            graph_nodes: FxHashMap::default(),
            edge_set: FxHashSet::default(),
            edge_info: Vec::new(),
            graph_dirty: false,
            explicit_orderings: FxHashSet::default(),
            pending_orderings: Vec::new(),
            seq_system_indices: Vec::new(),
            par_system_indices: Vec::new(),
            system_archetype_indices: FxHashMap::default(),
            cached_archetype_count: 0,
            startup_completed: false,
            stage_order: None,
            default_stage_label: StageLabel::Update,
            type_names: FxHashMap::default(),
            event_ordering_enabled: true,
            last_added_system_id: None,
            scope_condition: None,
            scratch_arch_lengths: Vec::new(),
            scratch_schedule: Vec::new(),
            scratch_sys_infos: Vec::new(),
            scratch_tasks: Vec::new(),
            scratch_skipped: FxHashSet::default(),
            deterministic_spawn: false,
            system_spawn_history: FxHashMap::default(),
            deterministic_overflow_count: 0,
            system_last_run: FxHashMap::default(),
            parallel_policy: ParallelPolicy::default(),
        }
    }

    /// D6: this system's change-detection baseline — the tick at which it last ran, or
    /// `Tick::ZERO` if it has not run yet (first run sees everything as new). Read when
    /// building the system's `SystemContext`.
    #[inline]
    pub(crate) fn system_window(&self, sys_id: SystemId) -> apex_core::Tick {
        self.system_last_run
            .get(&sys_id)
            .copied()
            .unwrap_or(apex_core::Tick::ZERO)
    }

    /// D6: advance the baseline of every system that RAN this stage (the `enabled` set)
    /// to `this_run`. Skipped systems are absent from `enabled`, so their baseline is
    /// left untouched — that is the whole point: a gated system's window reaches back to
    /// when it last ran, across the frames it sat out.
    #[inline]
    pub(crate) fn advance_system_windows(
        &mut self,
        enabled: &FxHashSet<SystemId>,
        this_run: apex_core::Tick,
    ) {
        for &sys_id in enabled {
            self.system_last_run.insert(sys_id, this_run);
        }
    }

    /// D8b overflow frontier: how many times a stage-system's spawn spike exhausted its
    /// id block + escrow and fell to the shared, non-deterministic allocator path
    /// (§0.2a — the same event that fires a throttled `warn_once!`). Steady-state is 0;
    /// a non-zero count means those frames' overflow entity ids were not run-to-run
    /// deterministic. Exposed for tests/diagnostics.
    #[inline]
    pub fn deterministic_overflow_count(&self) -> u64 {
        self.deterministic_overflow_count
    }

    /// Sh2/ADR-003: select the SEQ-vs-PAR dispatch policy.
    ///
    /// [`ParallelPolicy::CostModel`] (default) lets the measured dispatch-time EMA
    /// decide once warm. [`ParallelPolicy::Fixed`] opts out of the EMA gates and
    /// decides purely from entity counts — deterministic frame-to-frame, for
    /// pathological-EMA stages or when a reproducible dispatch decision is required.
    /// The explicit floor (`ChunkConfig::stage_parallel_min_entities`) applies under
    /// both.
    pub fn set_parallel_policy(&mut self, policy: ParallelPolicy) -> &mut Self {
        self.parallel_policy = policy;
        self
    }

    /// Sh2/ADR-003: the active SEQ-vs-PAR dispatch policy.
    #[inline]
    pub fn parallel_policy(&self) -> ParallelPolicy {
        self.parallel_policy
    }

    /// D8b: enable/disable deterministic parallel-spawn entity-id assignment.
    ///
    /// When on, parallel `Commands::spawn` from stage systems assign identical
    /// entity ids run-to-run given the same start snapshot + inputs (see the
    /// per-system block scheme). This is the record/replay / rollback foundation;
    /// Bevy does not guarantee it. Off by default: each command-emitting system
    /// reserves a per-frame id block, so opting in trades a modest id-space /
    /// memory cost (and, under heavy despawn+respawn churn, id-space growth — a
    /// documented frontier) for determinism.
    pub fn set_deterministic_spawn(&mut self, on: bool) -> &mut Self {
        self.deterministic_spawn = on;
        self
    }

    /// D8b: is deterministic parallel-spawn id assignment enabled?
    #[inline]
    pub fn deterministic_spawn(&self) -> bool {
        self.deterministic_spawn
    }

    /// D8b: adaptive per-system block size — `None` (never dispatched) → a generous
    /// initial guess; `Some(0)` (observed non-spawning) → no block; else observed
    /// peak ×2 (slack so steady-state never overflows), clamped to a small floor.
    pub(crate) fn block_size_for(&self, sys_id: SystemId) -> u32 {
        const INITIAL: u32 = 256;
        const FLOOR: u32 = 8;
        match self.system_spawn_history.get(&sys_id) {
            None => INITIAL,
            Some(&0) => 0,
            Some(&v) => v.saturating_mul(2).max(FLOOR),
        }
    }

    /// D8b overflow frontier: the id-block actually SEEDED per system = the adaptive
    /// [`block_size_for`] PLUS an escrow margin. The escrow is a rank-deterministic,
    /// contention-free private tail: a spike that exceeds the adaptive block still
    /// draws DETERMINISTIC ids from the escrow instead of falling to the shared,
    /// non-deterministic path — extending the single-binary determinism guarantee
    /// (ADR-001) from "steady-state only" to "steady-state + spikes within the escrow".
    /// The unused escrow tail is reclaimed each stage ([`commit_spawn_history`]), so the
    /// id-space stays bounded. Beyond block+escrow, the reserve falls through LOUDLY
    /// (telemetry). Escrow = half the block (a modest margin — the block is already
    /// 2× the observed peak, so this covers a ~3× peak spike; kept modest so the extra
    /// reclaim churn stays small on this opt-in path). Returns 0 iff the block is 0.
    pub(crate) fn seed_size_for(&self, sys_id: SystemId) -> u32 {
        const ESCROW_FLOOR: u32 = 8;
        let block = self.block_size_for(sys_id);
        if block == 0 {
            return 0;
        }
        let escrow = (block / 2).max(ESCROW_FLOOR);
        block.saturating_add(escrow)
    }

    /// D8b: after a stage applies (`flush` grew records), for each seeded system:
    /// (1) fold its observed spawn count (block size − remaining) into the adaptive-
    /// sizing history — `reserver` shares the block cursor with the one handed to the
    /// system, so `block_remaining` reflects consumption; on overflow (remaining 0)
    /// `used == size`, so the next block doubles toward demand; (2) reclaim the block's
    /// UNUSED tail to the reuse pool — reserved-but-not-spawned indices go back to the
    /// free-list (in rank order → deterministic), keeping the id-space bounded under
    /// despawn+respawn churn. Drains `seeded`.
    pub(crate) fn commit_spawn_history(
        &mut self,
        seeded: &mut Vec<(SystemId, u32, apex_core::entity::EntityReserver)>,
        world: &mut World,
    ) {
        for (sys_id, size, reserver) in seeded.drain(..) {
            // D8b overflow telemetry (§0.2a — loud, not silent): the system exhausted
            // its block AND its escrow and fell to the shared, non-deterministic path.
            // Its overflow ids this frame are NOT run-to-run deterministic. Count every
            // occurrence (diagnostics) and warn once per process (throttled).
            if reserver.block_overflowed() {
                self.deterministic_overflow_count =
                    self.deterministic_overflow_count.saturating_add(1);
                apex_core::warn_once!(
                    "deterministic_spawn: a system exhausted its id block + escrow and \
                     fell back to the shared allocator — this frame's overflow entity ids \
                     are NOT run-to-run deterministic. The block grows next frame; if this \
                     recurs, the spawn spike exceeds the adaptive block + escrow margin."
                );
            }
            let used = size - reserver.block_remaining().unwrap_or(0);
            self.system_spawn_history.insert(sys_id, used);
            world.reclaim_entity_block_tail(&reserver.unused_block_ids());
        }
    }

    // ── Registration ────────────────────────────────────────────
    //
    // The ONLY public entry point is `add_systems(label, …)` (+ the constructors
    // `sys`/`seq`/`par`/`par_access` and bare identifiers). The methods below are
    // internal building blocks and the backbone of the internal tests; the public
    // zoo of 15 add_* variants was removed by the API revision of 2026-06-12.

    /// Register a Sequential system (full &mut World).
    /// Stage is `default_stage_label` (`Update` by default).
    #[cfg(test)]
    pub(crate) fn add_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        self.add_system_to_stage(name, func, self.default_stage_label.clone())
    }

    /// Register a Sequential system in the given stage.
    /// Test-only builder helper — production code uses `add_systems` + `seq()`.
    #[cfg(test)]
    pub(crate) fn add_system_to_stage<F>(
        &mut self,
        name: impl Into<String>,
        func: F,
        stage_label: StageLabel,
    ) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        let id = SystemId(self.next_id);
        self.next_id += 1;
        self.last_added_system_id = Some(id);
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Sequential(Box::new(func)),
            after: Vec::new(),
            before: Vec::new(),
            stage_label,
            run_condition: ConditionTree::default(),
            apply_deferred_after: false,
            has_deferred: false,
        });
        self.system_indices.insert(id, index);
        self.seq_system_indices.push(index);
        self.invalidate_plan();
        self.merge_scope_condition(id);
        SystemBuilder {
            scheduler: self,
            id,
        }
    }

    /// Register a Sequential system in the Startup stage (runs once).
    #[cfg(test)]
    pub(crate) fn add_startup_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        let name_str = name.into();
        if self.startup_completed {
            log::warn!(
                "add_startup_system: `{}` added after Startup already completed — will never run",
                name_str
            );
        }
        self.add_system_to_stage(name_str, func, StageLabel::Startup)
    }

    /// Register an AutoSystem.
    /// Stage is `default_stage_label` (`Update` by default).
    #[cfg(test)]
    pub(crate) fn add_auto_system<S>(&mut self, name: impl Into<String>, system: S) -> SystemBuilder<'_>
    where
        S: AutoSystem + 'static,
    {
        self.add_auto_system_to_stage(name, system, self.default_stage_label.clone())
    }

    /// Register an AutoSystem in the given stage.
    #[cfg(test)]
    pub(crate) fn add_auto_system_to_stage<S>(
        &mut self,
        name: impl Into<String>,
        system: S,
        stage_label: StageLabel,
    ) -> SystemBuilder<'_>
    where
        S: AutoSystem + 'static,
    {
        let id = SystemId(self.next_id);
        self.next_id += 1;
        self.last_added_system_id = Some(id);
        let mut access = S::Query::system_access()
            .merge(&S::Resources::resource_accesses())
            .merge(&S::Events::event_accesses());
        if S::NEEDS_WHOLE_WORLD {
            access.needs_whole_world = true;
        }
        // W3-4: a stateful system is not split by ASD row-split — multiple
        // tasks would call run(&mut self) on one instance concurrently.
        if std::mem::size_of::<S>() > 0 {
            access.stateful = true;
        }
        let adapter = AutoSystemAdapter { inner: system };
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel {
                system: Box::new(adapter),
                access,
            },
            after: Vec::new(),
            before: Vec::new(),
            stage_label,
            run_condition: ConditionTree::default(),
            apply_deferred_after: false,
            has_deferred: S::HAS_DEFERRED,
        });
        self.system_indices.insert(id, index);
        self.par_system_indices.push(index);
        self.invalidate_plan();
        self.merge_scope_condition(id);
        SystemBuilder {
            scheduler: self,
            id,
        }
    }

    /// Register an AutoSystem in the Startup stage.
    #[cfg(test)]
    pub(crate) fn add_startup_auto_system<S>(&mut self, name: impl Into<String>, system: S) -> SystemBuilder<'_>
    where
        S: AutoSystem + 'static,
    {
        let name_str = name.into();
        if self.startup_completed {
            log::warn!(
                "add_startup_auto_system: `{}` added after Startup already completed — will never run",
                name_str
            );
        }
        self.add_auto_system_to_stage(name_str, system, StageLabel::Startup)
    }

    /// Register a ParSystem.
    /// Stage is `default_stage_label` (`Update` by default).
    #[allow(dead_code)]
    pub(crate) fn add_par_system<S: ParSystem + 'static>(
        &mut self,
        name: impl Into<String>,
        system: S,
    ) -> SystemId {
        self.add_par_system_to_stage(name, system, self.default_stage_label.clone())
    }

    /// Register a ParSystem in the given stage.
    #[allow(dead_code)]
    pub(crate) fn add_par_system_to_stage<S: ParSystem + 'static>(
        &mut self,
        name: impl Into<String>,
        system: S,
        stage_label: StageLabel,
    ) -> SystemId {
        let id = SystemId(self.next_id);
        self.next_id += 1;
        self.last_added_system_id = Some(id);
        let mut access = S::access();
        // W3-4: a stateful system → no ASD row-split.
        if std::mem::size_of::<S>() > 0 {
            access.stateful = true;
        }
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel {
                system: Box::new(system),
                access,
            },
            after: Vec::new(),
            before: Vec::new(),
            stage_label,
            run_condition: ConditionTree::default(),
            apply_deferred_after: false,
            has_deferred: false,
        });
        self.system_indices.insert(id, index);
        self.par_system_indices.push(index);
        self.invalidate_plan();
        self.merge_scope_condition(id);
        id
    }

    /// Register a ParSystem in the Startup stage.
    #[allow(dead_code)]
    pub(crate) fn add_startup_par_system<S: ParSystem + 'static>(
        &mut self,
        name: impl Into<String>,
        system: S,
    ) -> SystemId {
        self.add_par_system_to_stage(name, system, StageLabel::Startup)
    }


    /// Register a parallel closure system with explicit access.
    ///
    /// `access` describes which components/resources/events the system reads
    /// and writes — the scheduler uses this to resolve conflicts.
    ///
    /// Stage is `default_stage_label` (`Update` by default).
    ///
    /// ```
    /// # use apex_core::prelude::*;
    /// # use apex_core::access_desc;
    /// # use apex_scheduler::{Scheduler, StageLabel};
    /// # let mut sched = Scheduler::new();
    /// # #[derive(Component)] struct Pos { x: f32, y: f32 }
    /// # #[derive(Component)] struct Vel { x: f32, y: f32 }
    /// sched.add_par_access(
    ///     "physics",
    ///     access_desc!(read<Vel>, write<Pos>),
    ///     |ctx| {
    ///         ctx.query_unchecked::<(Read<Vel>, Write<Pos>)>().for_each(|_, (v, mut p)| {
    ///             p.x += v.x;
    ///         });
    ///     },
    /// );
    /// ```
    #[cfg(test)]
    pub(crate) fn add_par_access<F>(
        &mut self,
        name: impl Into<String>,
        access: AccessDescriptor,
        func: F,
    ) -> SystemBuilder<'_>
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        self.add_par_access_to_stage(name, access, func, self.default_stage_label.clone())
    }

    /// Register a parallel closure system with explicit access
    /// in the given stage.
    #[cfg(test)]
    fn add_par_access_to_stage<F>(
        &mut self,
        name: impl Into<String>,
        access: AccessDescriptor,
        func: F,
        stage_label: StageLabel,
    ) -> SystemBuilder<'_>
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let id = SystemId(self.next_id);
        self.next_id += 1;
        self.last_added_system_id = Some(id);
        let mut access = access;
        // W3-4: a capturing closure = state → no ASD row-split.
        if std::mem::size_of::<F>() > 0 {
            access.stateful = true;
        }
        let system = FnParSystem {
            func: Box::new(func),
            access: access.clone(),
        };
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel {
                system: Box::new(system),
                access,
            },
            after: Vec::new(),
            before: Vec::new(),
            stage_label,
            run_condition: ConditionTree::default(),
            apply_deferred_after: false,
            has_deferred: false,
        });
        self.system_indices.insert(id, index);
        self.par_system_indices.push(index);
        self.invalidate_plan();
        self.merge_scope_condition(id);
        SystemBuilder {
            scheduler: self,
            id,
        }
    }

    // Stage-parallelism gating is configured on the World, not the scheduler
    // (wave 3, §1.7): `world.set_chunk_config(ChunkConfig {
    // stage_parallel_min_entities, auto_disable_stage_parallel, ..default() })`.
    // The scheduler reads these from `world.chunk_config()` at run time — one
    // config object for all parallelism tuning. (Former setters
    // `set_parallel_min_entities` / `set_parallel_auto_disable` removed.)

    /// Heuristic: the minimum entity count per system for parallelism to pay
    /// off (rayon overhead < gain).
    ///
    /// Thresholds were found empirically via the `parallel_diagnostics` scaling benchmark
    /// (12 cores, release). The "valley of death" (PAR 2-3x slower than SEQ) sits
    /// in the 5,000-10,000 entity range for multi-system and up to 50,000 for single-system.
    pub(crate) fn min_entities_for_parallelism(num_systems: usize) -> usize {
        if num_systems >= 3 {
            15_000 // 3+ systems — amortizes rayon overhead at 25K+ entities
        } else if num_systems >= 2 {
            25_000 // 2 systems — PAR/SEQ crossover around 25K
        } else {
            80_000 // 1 system — row-level chunking only, crossover ~100K
        }
    }

    /// Register systems in the given stage via a closure.
    ///
    /// Inside the closure `default_stage_label` is temporarily set to `label`,
    /// so every `add_*_system` (without `_to_stage`) inside lands in that stage.
    ///
    /// After the closure the previous stage is restored.
    ///
    /// Handy for grouping a plugin's systems:
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel};
    /// # use apex_core::world::SystemContext;
    /// let mut sched = Scheduler::new();
    ///
    /// sched.set_default_stage(StageLabel::tag("update"));
    ///
    /// sched.staged(StageLabel::tag("input"), |s| {
    ///     s.add_par("read_keys", |_: SystemContext<'_>| {});
    /// });
    ///
    /// sched.staged(StageLabel::tag("render"), |s| {
    ///     s.add_par("draw", |_: SystemContext<'_>| {});
    /// });
    ///
    /// sched.add_par("particles", |_: SystemContext<'_>| {});
    /// ```
    #[cfg(test)]
    pub(crate) fn staged<F>(&mut self, label: StageLabel, f: F) -> &mut Self
    where
        F: FnOnce(&mut Self),
    {
        let previous = std::mem::replace(&mut self.default_stage_label, label);
        let saved_condition = self.scope_condition.clone();
        f(self);
        self.default_stage_label = previous;
        // The scope condition applies only INSIDE the block (it used to "stick" to
        // all subsequent registrations — a latent bug found by the audit
        // 2026-06-12).
        self.scope_condition = saved_condition;
        self
    }

    /// Condition scope: every system registered inside the closure —
    /// including via [`add_systems`](Self::add_systems) — gets the conditions
    /// set by [`run_condition`](Self::run_condition) (AND-ed with their own).
    /// Scopes nest (conditions combine with AND); on leaving the block
    /// the previous scope is restored.
    ///
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel, seq};
    /// # let mut sched = Scheduler::new();
    /// sched.scoped(|s| {
    ///     s.run_condition(|w| !w.has_resource::<bool>());
    ///     s.add_systems(StageLabel::Update, (
    ///         seq("movement", |_w: &mut apex_core::world::World| {}),
    ///         seq("ai", |_w: &mut apex_core::world::World| {}),
    ///     ));
    ///     // both systems inherit the pause condition
    /// });
    /// ```
    pub fn scoped<F>(&mut self, f: F) -> &mut Self
    where
        F: FnOnce(&mut Self),
    {
        let saved_condition = self.scope_condition.clone();
        f(self);
        self.scope_condition = saved_condition;
        self
    }

    /// Set a scope condition — every system registered inside
    /// the current [`scoped`](Self::scoped) block automatically gets this
    /// condition (AND-ed with their own). Repeated calls inside
    /// the block combine with AND.
    ///
    /// # Example
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel, seq};
    /// # let mut sched = Scheduler::new();
    /// sched.scoped(|s| {
    ///     s.run_condition(|w| !w.has_resource::<bool>());
    ///     s.add_systems(StageLabel::Update, (
    ///         seq("movement", |_w: &mut apex_core::world::World| {}),
    ///         seq("ai", |_w: &mut apex_core::world::World| {}),
    ///     ));
    ///     // both systems inherit the pause condition
    /// });
    /// ```
    pub fn run_condition<F>(&mut self, condition: F) -> &mut Self
    where
        F: Fn(&World) -> bool + Send + Sync + 'static,
    {
        let leaf: std::sync::Arc<dyn Fn(&World) -> bool + Send + Sync> = std::sync::Arc::new(condition);
        self.scope_condition = match self.scope_condition.take() {
            None => Some(leaf),
            Some(existing) => Some(std::sync::Arc::new(move |w: &World| existing(w) && leaf(w))),
        };
        self
    }

    fn merge_scope_condition(&mut self, sys_id: SystemId) {
        if let Some(ref scope_fn) = self.scope_condition {
            let scope_clone = std::sync::Arc::clone(scope_fn);
            if let Some(sys) = self.system_by_id_mut(sys_id) {
                let leaf = ConditionTree::leaf(move |w: &World| (*scope_clone)(w));
                match &mut sys.run_condition {
                    ConditionTree::And(ref mut conds) => conds.push(leaf),
                    _ => {
                        let old = std::mem::replace(&mut sys.run_condition, ConditionTree::And(Vec::new()));
                        if let ConditionTree::And(ref mut conds) = sys.run_condition {
                            conds.push(old);
                            conds.push(leaf);
                        }
                    }
                }
            }
        }
    }

    /// Add an explicit dependency: `system` runs after `after_id`.
    pub(crate) fn add_dependency(&mut self, system: SystemId, after_id: SystemId) {
        if let Some(s) = self.systems.iter_mut().find(|s| s.id == system) {
            if !s.after.contains(&after_id) {
                s.after.push(after_id);
            }
            self.explicit_orderings.insert((after_id, system));
            self.invalidate_plan();
        }
    }

    /// Drain config-declared orderings (`.before()`/`.after()`/`.chain()`),
    /// resolving name endpoints to ids and recording each as a dependency edge.
    /// Called once at the start of `compile()` while the graph is dirty; a name
    /// that resolves to no system surfaces as `SystemNotFound` (loud, §0.2a).
    pub(crate) fn resolve_pending_orderings(&mut self) -> Result<(), SchedulerError> {
        if self.pending_orderings.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_orderings);
        for (before, after) in pending {
            let before_id = self.resolve_order_endpoint(&before)?;
            let after_id = self.resolve_order_endpoint(&after)?;
            // `after` runs after `before` ⇒ dependency edge before → after.
            self.add_dependency(after_id, before_id);
        }
        Ok(())
    }

    fn resolve_order_endpoint(&self, ep: &OrderEndpoint) -> Result<SystemId, SchedulerError> {
        match ep {
            OrderEndpoint::Id(id) => Ok(*id),
            OrderEndpoint::Name(name) => self.find_id_by_name(name),
        }
    }

    /// `a` runs before `b`. Equivalent to `add_dependency(b, a)`.
    ///
    /// An explicit order takes precedence over automatically detected read-write
    /// conflicts: if the scheduler sees `BidirectionalWriteRead` between `a` and `b`,
    /// the edge contradicting the explicit order is suppressed (no cycle).
    ///
    /// Systems are named as passed to `add_auto_system` / `add_system`.
    pub fn before(&mut self, a_name: &str, b_name: &str) -> Result<(), SchedulerError> {
        let a_id = self.find_id_by_name(a_name)?;
        let b_id = self.find_id_by_name(b_name)?;
        self.add_dependency(b_id, a_id);
        Ok(())
    }

    /// `a` runs after `b`. Equivalent to `add_dependency(a, b)`.
    ///
    /// An explicit order takes precedence over automatically detected read-write
    /// conflicts: if the scheduler sees `BidirectionalWriteRead` between `a` and `b`,
    /// the edge contradicting the explicit order is suppressed (no cycle).
    ///
    /// Systems are named as passed to `add_auto_system` / `add_system`.
    pub fn after(&mut self, a_name: &str, b_name: &str) -> Result<(), SchedulerError> {
        let a_id = self.find_id_by_name(a_name)?;
        let b_id = self.find_id_by_name(b_name)?;
        self.add_dependency(a_id, b_id);
        Ok(())
    }

    pub(crate) fn find_id_by_name(&self, name: &str) -> Result<SystemId, SchedulerError> {
        // Exact match first.
        if let Some(s) = self.systems.iter().find(|s| s.name == name) {
            return Ok(s.id);
        }
        // Fallback: match the last `::` path segment, so a fn-based exclusive system registered
        // under its `type_name` (e.g. "my_crate::init_metrics") can be referenced by its short name
        // ("init_metrics") in `chain`/`after`. Only when exactly one system matches (no ambiguity).
        let mut matches = self
            .systems
            .iter()
            .filter(|s| s.name.rsplit("::").next() == Some(name));
        if let Some(first) = matches.next() {
            if matches.next().is_none() {
                return Ok(first.id);
            }
        }
        Err(SchedulerError::SystemNotFound(name.to_string()))
    }

    /// A chain of systems: each runs after the previous one.
    ///
    /// `sched.chain(&["grav", "phys"])` is equivalent to `sched.before("grav", "phys")`.
    ///
    /// For N names it creates N-1 dependencies:
    /// `names[0] → names[1] → ... → names[N-1]`.
    pub fn chain(&mut self, names: &[&str]) -> Result<(), SchedulerError> {
        for w in names.windows(2) {
            self.before(w[0], w[1])?;
        }
        Ok(())
    }

    /// Declare systems mutually **order-independent**: on `BidirectionalWriteRead` between them
    /// the scheduler does NOT fail compilation (`CircularDependency`) but serializes them in
    /// **deterministic registration order** (reproducible across builds).
    ///
    /// When to use: two systems have a cross read-write conflict (A writes T read by
    /// B; B writes U read by A), but their execution order **does not matter** for the logic — and you
    /// don't want to invent an artificial `before`/`after`. The data conflict still prevents
    /// parallel execution (no races) — `independent` only drops the requirement to *explicitly pick*
    /// a direction, fixing it deterministically (system registration order).
    ///
    /// Difference from Bevy `ambiguous_with`: that allows an ARBITRARY order (non-determinism); we
    /// keep determinism (important for replay/netcode — see the guide §6.3).
    ///
    /// For N names it covers all pairs. Systems are named (as in `before`/`after`/`chain`).
    pub fn independent(&mut self, names: &[&str]) -> Result<(), SchedulerError> {
        let mut ids = Vec::with_capacity(names.len());
        for n in names {
            ids.push(self.find_id_by_name(n)?);
        }
        // For each pair — an edge in REGISTRATION order (position in `self.systems`); collect the pairs
        // up front, so we don't hold an immutable borrow of `self.systems` during `add_dependency`.
        let mut ordered: Vec<(SystemId, SystemId)> = Vec::new();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (a, b) = (ids[i], ids[j]);
                let pa = self.systems.iter().position(|s| s.id == a);
                let pb = self.systems.iter().position(|s| s.id == b);
                let (first, second) = match (pa, pb) {
                    (Some(pa), Some(pb)) if pb < pa => (b, a),
                    _ => (a, b),
                };
                ordered.push((first, second));
            }
        }
        for (first, second) in ordered {
            // `first` → `second`: explicit ordering suppresses the opposing BidirectionalWriteRead edge.
            self.add_dependency(second, first);
        }
        Ok(())
    }

    /// Set a custom StageLabel order for compile().
    ///
    /// By default stages are ordered by priority:
    /// `Startup(0) → First(1) → PreUpdate(2) → Update(3) → PostUpdate(4) → Last(5) → Custom(6)`.
    ///
    /// To change the order (e.g. `First` after `Update`), use this method:
    ///
    /// ```ignore
    /// # use apex_scheduler::stage::StageLabel::*;
    /// let mut scheduler = Scheduler::new();
    /// scheduler.configure_stages(vec![Startup, Update, First, PreUpdate, PostUpdate, Last]);
    /// ```
    ///
    /// Stages not listed in `order` are appended at the end in ascending priority order.
    pub fn configure_stages(&mut self, order: Vec<StageLabel>) {
        self.stage_order = Some(order);
        self.invalidate_plan();
    }

    // ── Run Conditions ─────────────────────────────────────────

    /// Attach a run condition to a system by name (AND composition).
    #[cfg(test)]
    pub(crate) fn set_run_if<F>(&mut self, name: &str, condition: F) -> Result<(), SchedulerError>
    where
        F: Fn(&World) -> bool + Send + Sync + 'static,
    {
        let id = self.find_id_by_name(name)?;
        let leaf = ConditionTree::leaf(condition);
        if let Some(sys) = self.system_by_id_mut(id) {
            match &mut sys.run_condition {
                ConditionTree::And(ref mut conds) => conds.push(leaf),
                _ => {
                    let old = std::mem::replace(&mut sys.run_condition, ConditionTree::And(Vec::new()));
                    if let ConditionTree::And(ref mut conds) = sys.run_condition {
                        conds.push(old);
                        conds.push(leaf);
                    }
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_run_if_cond<C: Condition>(
        &mut self,
        name: &str,
        condition: C,
    ) -> Result<(), SchedulerError> {
        let id = self.find_id_by_name(name)?;
        let acc = condition.access();
        let leaf = ConditionTree::Leaf(condition.into_check_fn());
        if let Some(sys) = self.system_by_id_mut(id) {
            if let SystemKind::Parallel { ref mut access, .. } = &mut sys.kind {
                *access = std::mem::take(access).merge(&acc);
            }
            match &mut sys.run_condition {
                ConditionTree::And(ref mut conds) => conds.push(leaf),
                _ => {
                    let old = std::mem::replace(&mut sys.run_condition, ConditionTree::And(Vec::new()));
                    if let ConditionTree::And(ref mut conds) = sys.run_condition {
                        conds.push(old);
                        conds.push(leaf);
                    }
                }
            }
        }
        Ok(())
    }

    // ── Apply Deferred ─────────────────────────────────────────

    /// Insert a sync point after the last registered system.
    ///
    /// At `compile()` the stage is split into sub-stages, and between them all
    /// accumulated `Commands` are applied and events are flushed.
    ///
    /// Typical use — between `add_systems` calls:
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel, seq};
    /// # let mut sched = Scheduler::new();
    /// let combat = StageLabel::tag("combat");
    /// sched.add_systems(combat.clone(), seq("spawner", |_w: &mut apex_core::world::World| {}));
    /// sched.apply_deferred(); // ← spawner commands applied before the next systems
    /// sched.add_systems(combat, seq("camera", |_w: &mut apex_core::world::World| {}));
    /// ```
    pub fn apply_deferred(&mut self) -> &mut Self {
        if let Some(last_id) = self.last_added_system_id {
            if let Some(sys) = self.system_by_id_mut(last_id) {
                sys.apply_deferred_after = true;
            }
        }
        self
    }

    // ── add_systems — tuple API (the professional way) ──────────

    /// Register several systems at once via the tuple API.
    ///
    /// This is the recommended way to register. Systems are configured
    /// with conditions BEFORE registration, which avoids a double-borrow conflict.
    ///
    /// # Example
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel, sys, seq, IntoScheduleConfigs};
    /// # let mut sched = Scheduler::new();
    /// sched.add_systems(StageLabel::Update, (
    ///     sys("a", MoveSys).run_if(|_: &apex_core::world::World| true),
    ///     seq("cleanup", |_: &mut apex_core::world::World| {}),
    /// ));
    /// # struct MoveSys;
    /// # impl apex_scheduler::AutoSystem for MoveSys {
    /// #     type Query = (); type Resources = (); type Events = ();
    /// #     fn run(&mut self, _: apex_core::world::SystemContext<'_>) {}
    /// # }
    /// ```
    ///
    /// # Ordering (the golden path)
    ///
    /// Dependencies are declared right on the configs via `.before()`/`.after()`
    /// (by system name) and `.chain()` (positionally, by tuple order). Names
    /// are resolved at `compile()`, so forward references are allowed:
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel, seq, IntoScheduleConfigs};
    /// # let mut sched = Scheduler::new();
    /// # let s = |_: &mut apex_core::world::World| {};
    /// sched.add_systems(StageLabel::Update, (
    ///     seq("input", s).before("physics"),      // input → physics
    ///     (seq("gravity", s), seq("physics", s)).chain(), // gravity → physics
    ///     seq("render", s).after("physics"),       // physics → render
    /// ));
    /// sched.compile().unwrap();
    /// ```
    /// For DYNAMIC ordering (by string name, from the editor/scripts)
    /// use `Scheduler::before`/`after`/`chain`.
    pub fn add_systems<M>(
        &mut self,
        stage_label: StageLabel,
        systems: impl IntoScheduleConfigs<M>,
    ) -> &mut Self {
        let ScheduleConfigs { configs, edges } = systems.into_configs();
        let mut ids = Vec::with_capacity(configs.len());
        for mut cfg in configs {
            // Config-declared name orderings (`.before("x")`/`.after("x")`) are
            // deferred: the referenced system may be registered later. Positional
            // ids for chain edges are captured below.
            let before_names = std::mem::take(&mut cfg.before_names);
            let after_names = std::mem::take(&mut cfg.after_names);
            let id = self.register_system_config(cfg, stage_label.clone());
            for b in before_names {
                // self runs BEFORE b.
                self.pending_orderings
                    .push((OrderEndpoint::Id(id), OrderEndpoint::Name(b)));
            }
            for a in after_names {
                // a runs BEFORE self.
                self.pending_orderings
                    .push((OrderEndpoint::Name(a), OrderEndpoint::Id(id)));
            }
            ids.push(id);
        }
        // Positional chain edges: configs[bi] runs before configs[ai].
        for (bi, ai) in edges {
            self.pending_orderings.push((
                OrderEndpoint::Id(ids[bi]),
                OrderEndpoint::Id(ids[ai]),
            ));
        }
        self
    }

    /// Internal registration of one SystemConfig.
    fn register_system_config(&mut self, cfg: SystemConfig, stage_label: StageLabel) -> SystemId {
        if stage_label == StageLabel::Startup && self.startup_completed {
            log::warn!(
                "add_systems(Startup, …): `{}` added after Startup finished — it will not run",
                cfg.name
            );
        }
        let id = SystemId(self.next_id);
        self.next_id += 1;
        self.last_added_system_id = Some(id);
        let index = self.systems.len();

        match cfg.kind {
            SystemConfigKind::Auto(system, mut access) => {
                access = access.merge(&cfg.condition_access);
                self.systems.push(SystemDescriptor {
                    id,
                    name: cfg.name,
                    kind: SystemKind::Parallel { system, access },
                    after: Vec::new(),
                    before: Vec::new(),
                    stage_label,
                    run_condition: cfg.condition,
                    apply_deferred_after: false,
                    has_deferred: cfg.has_deferred,
                });
                self.par_system_indices.push(index);
            }
            SystemConfigKind::Sequential(f) => {
                self.systems.push(SystemDescriptor {
                    id,
                    name: cfg.name,
                    kind: SystemKind::Sequential(f),
                    after: Vec::new(),
                    before: Vec::new(),
                    stage_label,
                    run_condition: cfg.condition,
                    apply_deferred_after: false,
                    has_deferred: cfg.has_deferred,
                });
                self.seq_system_indices.push(index);
            }
            SystemConfigKind::ParClosure { access, func } => {
                let access = access.merge(&cfg.condition_access);
                self.systems.push(SystemDescriptor {
                    id,
                    name: cfg.name,
                    kind: SystemKind::Parallel {
                        system: Box::new(FnParSystem {
                            func,
                            access: access.clone(),
                        }),
                        access,
                    },
                    after: Vec::new(),
                    before: Vec::new(),
                    stage_label,
                    run_condition: cfg.condition,
                    apply_deferred_after: false,
                    has_deferred: cfg.has_deferred,
                });
                self.par_system_indices.push(index);
            }
        }

        self.system_indices.insert(id, index);
        // Scope conditions (scoped/run_condition) apply to the add_systems path too
        // (previously only to the internal registration methods: the documented
        // "scoped + add_systems" pattern silently dropped the condition — a latent bug
        // found by the audit of 2026-06-12).
        self.merge_scope_condition(id);
        self.invalidate_plan();
        id
    }

    /// Get `&mut SystemDescriptor` by `SystemId` (O(1)).
    pub(crate) fn system_by_id_mut(&mut self, id: SystemId) -> Option<&mut SystemDescriptor> {
        self.system_indices
            .get(&id)
            .and_then(|&idx| self.systems.get_mut(idx))
    }

    fn invalidate_plan(&mut self) {
        // NOTE: do NOT reset stage_order — it must persist
        // across recompiles (see the configure_stages_persists_across_compiles test).
        self.execution_plan = None;
        self.graph_dirty = true;
    }

    /// Control automatic event-based ordering.
    ///
    /// When `true` (the default): every system with `Emit<E>` is guaranteed to
    /// run before systems with `Listen<E>` within a single frame.
    ///
    /// When `false`: the Emit/Listen order is not defined by the scheduler.
    /// Use only for backward compatibility or if
    /// the order does not matter and you want to maximize parallelism.
    pub fn enable_event_ordering(&mut self, enabled: bool) -> &mut Self {
        self.event_ordering_enabled = enabled;
        self.invalidate_plan();
        self
    }

    /// Create an event pipeline for type E.
    ///
    /// # Example
    /// ```ignore
    /// let physics_id = sched.add_auto_system("physics", PhysicsSystem);
    /// let armor_id   = sched.add_auto_system("armor",   ArmorSystem);
    /// let health_id  = sched.add_auto_system("health",  HealthSystem);
    ///
    /// Scheduler::event_pipeline::<DamageEvent>()
    ///     .produced_by("physics")
    ///     .transformed_by("armor")
    ///     .consumed_by("health")
    ///     .build(&mut sched);
    /// ```
    pub fn event_pipeline<E: Send + Sync + 'static>() -> EventPipelineBuilder<E> {
        EventPipelineBuilder::new()
    }

    /// Get a system's AccessDescriptor by its SystemId.
    ///
    /// Returns `None` if the system is not found or is a sequential system.
    pub(crate) fn system_access(&self, id: SystemId) -> Option<&AccessDescriptor> {
        self.system_indices
            .get(&id)
            .and_then(|&idx| self.systems.get(idx)?.kind.access())
    }

    /// Populate `type_names` from the World's ComponentRegistry.
    ///
    /// After calling this, `component_type_name()` returns
    /// real component names instead of `"<component>"`.
    ///
    /// Called automatically in `run()` / `run_sequential()`.
    /// Can be called manually before `compile()` if you need real names
    /// in `debug_plan_verbose()`.
    pub(crate) fn populate_type_names(&mut self, registry: &ComponentRegistry) {
        if !self.type_names.is_empty() {
            return; // Already populated — all worlds have the same components
        }
        for info in registry.iter() {
            self.type_names.insert(info.type_id, info.name);
        }
    }

}
