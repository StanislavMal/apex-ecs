use super::*;

impl Scheduler {
    // ── Execution ─────────────────────────────────────────────

    /// Run one scheduler iteration.
    /// Uses Rayon for parallel execution.
    ///
    /// The Startup stage runs only on the first `run()` call.
    /// Open a stage's change-detection window (TD-52 cross-stage fix). Advances `current_tick` so this
    /// stage's writes get a fresh, higher tick, then sets the base (`last_run_tick`) to the tick at
    /// which this stage last ran. A `Changed<T>`/`Added<T>` reader in the stage therefore sees
    /// everything written since the stage **itself** last ran — including writes by later stages of the
    /// previous frame, which the old per-frame tick missed. Hot path is untouched (the per-row change
    /// comparison is unchanged); this is O(1) per stage.
    #[inline]
    fn open_stage_change_window(&mut self, stage_idx: usize, world: &mut World) {
        world.tick();
        let this_run = world.current_tick();
        if self.stage_last_run.len() <= stage_idx {
            self.stage_last_run.resize(stage_idx + 1, apex_core::Tick::ZERO);
        }
        let base = self.stage_last_run[stage_idx];
        world.set_last_run_tick(base);
        self.stage_last_run[stage_idx] = this_run;
    }

    /// Sh2: cost-model verdict — should this parallel-eligible stage run sequentially?
    /// Uses the measured dispatch-time EMA with a ±hysteresis band around
    /// `T_STAGE_SEQ_NS` to avoid flapping. `None`-history (EMA 0) → `false` (let the
    /// caller's entity heuristic decide the first frames).
    pub(crate) fn cost_model_prefers_seq(&self, stage_idx: usize) -> bool {
        let ema = self.stage_cost_ema_ns.get(stage_idx).copied().unwrap_or(0.0);
        if ema == 0.0 {
            return false;
        }
        let was_seq = self.stage_ran_seq.get(stage_idx).copied().unwrap_or(false);
        // Hysteresis: once SEQ, stay SEQ until clearly above the band; once PAR,
        // switch to SEQ only when clearly below it.
        let threshold = if was_seq {
            T_STAGE_SEQ_NS * (1.0 + STAGE_HYSTERESIS)
        } else {
            T_STAGE_SEQ_NS * (1.0 - STAGE_HYSTERESIS)
        };
        ema < threshold
    }

    /// Sh2/ADR-003: the SEQ-vs-PAR verdict for a parallel-eligible stage — a pure
    /// function of the policy, the explicit floor, and the measured/heuristic inputs,
    /// extracted from `run_hybrid_parallel` so it is unit-testable in isolation.
    /// `true` = run the stage SEQ (fall back off the ASD path).
    ///
    /// Priority:
    ///   1. explicit floor (`stage_par_min`) — a hard "never parallelize below N"
    ///      gate under BOTH policies;
    ///   2. `ParallelPolicy::Fixed` — the entity heuristic only, NEVER the cost-model
    ///      EMA (deterministic, measurement-free);
    ///   3. `ParallelPolicy::CostModel` (default) — the warm EMA fully decides; on
    ///      cold start fall back to the entity heuristic when `auto_disable` is set,
    ///      else parallelize.
    pub(crate) fn stage_prefers_seq(
        &self,
        stage_idx: usize,
        stage_entity_count: usize,
        sys_count: usize,
        stage_par_min: usize,
        auto_disable: bool,
    ) -> bool {
        // 1. Explicit floor wins under both policies.
        if stage_par_min > 0 && stage_entity_count < stage_par_min {
            return true;
        }
        // Pure entity heuristic (no EMA): SEQ when per-system work is below the
        // valley-of-death crossover. Shared by Fixed and cost-model cold-start.
        let entity_heuristic_prefers_seq = || {
            let per_system = stage_entity_count / sys_count.max(1);
            per_system < Self::min_entities_for_parallelism(sys_count)
        };
        match self.parallel_policy {
            ParallelPolicy::Fixed => entity_heuristic_prefers_seq(),
            ParallelPolicy::CostModel => {
                let has_cost_history =
                    self.stage_cost_ema_ns.get(stage_idx).copied().unwrap_or(0.0) > 0.0;
                if has_cost_history {
                    self.cost_model_prefers_seq(stage_idx)
                } else if auto_disable {
                    entity_heuristic_prefers_seq()
                } else {
                    false
                }
            }
        }
    }

    /// Sh2: fold a measured stage dispatch time into the per-stage EMA and record the
    /// mode that produced it (for hysteresis). Lazily grows the buffers.
    pub(crate) fn record_stage_cost(&mut self, stage_idx: usize, elapsed: std::time::Duration, ran_seq: bool) {
        let ns = elapsed.as_nanos() as f64;
        if self.stage_cost_ema_ns.len() <= stage_idx {
            self.stage_cost_ema_ns.resize(stage_idx + 1, 0.0);
        }
        if self.stage_ran_seq.len() <= stage_idx {
            self.stage_ran_seq.resize(stage_idx + 1, false);
        }
        let ema = &mut self.stage_cost_ema_ns[stage_idx];
        *ema = if *ema == 0.0 {
            ns
        } else {
            *ema * (1.0 - STAGE_EMA_ALPHA) + ns * STAGE_EMA_ALPHA
        };
        self.stage_ran_seq[stage_idx] = ran_seq;
    }

    pub fn run(&mut self, world: &mut World) {
        if main_prof_enabled() {
            main_prof_world_stats(world);
        }
        // Populate the component-name registry for conflict diagnostics
        self.populate_type_names(world.registry());
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        // Compute the system → archetype mapping for the SubWorld.
        self.compute_archetype_indices(world);

        self.run_hybrid_parallel(world as *mut World);

        // Frame boundary: advance the change-tick so `Changed<T>` in systems
        // next frame detects only this frame's mutations (TD-9).
        world.advance_change_tick();

        // After the first run() Startup no longer runs
        self.startup_completed = true;
    }

    /// Sequential execution — for tests and non-parallel builds.
    ///
    /// Takes a safe `&mut World` (A5: no raw pointer on the public surface).
    /// Internally a raw pointer is re-derived because a `SubWorld` (read-only
    /// view) and `Commands` application (`&mut World`) are juggled at once, which
    /// cannot be expressed through a single `&mut World` without reborrow
    /// conflicts — the exclusive `&mut World` receiver makes that re-derivation
    /// sound (no other view is live).
    pub fn run_sequential(&mut self, world: &mut World) {
        // MIRI-CD: derive ONE raw pointer and route every access through it — the read
        // view (SubWorld, built from `world_ptr`) and the interleaved `&mut World` writes
        // (tick bump, Commands apply, sequential systems) are then all children of the
        // same raw tag, so no write disables the SubWorld's read pointer. Holding a
        // long-lived `&mut *world_ptr` instead would make every SubWorld read a foreign
        // access to that outstanding `&mut` (UB under Stacked/Tree Borrows).
        let world_ptr = world as *mut World;
        self.populate_type_names(unsafe { &*world_ptr }.registry());
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        let plan = self.execution_plan.as_ref().unwrap();

        for system in &self.systems {
            if let Some(access) = system.kind.access() {
                for &(type_id, cap) in &access.event_reserves {
                    unsafe { &mut *world_ptr }.event_reserve_by_type(type_id, cap);
                }
            }
        }

        // Sequential path: one shared command buffer per run, applied after each
        // stage. Systems run one-at-a-time in rank order, so both command order and
        // entity-id assignment are already deterministic (the shared reserver hands
        // ids out in rank order) — no per-system blocks needed here (D8b applies to
        // the parallel path). Single `Commands` matches the new per-system slot API.
        let mut stage_cmds = Commands::new();
        let cmds_ptr = &mut stage_cmds as *mut Commands;

        // (clone the stage metadata — the plan borrows self, while stage_steps
        // and the systems below need &mut)
        let stages_meta: Vec<(StageLabel, Vec<SystemId>, Vec<TypeId>)> = plan
            .stages
            .iter()
            .map(|s| (s.label.clone(), s.system_ids.clone(), s.emit_event_types.clone()))
            .collect();

        // Same FixedUpdate grouping as run_hybrid_parallel (D1): the contiguous
        // FixedUpdate block drains the accumulator ONCE and runs `steps` times as
        // (A;B;…)×N, so no later FixedUpdate stage is starved and the order is
        // correct. Every other stage runs once.
        let mut schedule: Vec<usize> = Vec::with_capacity(stages_meta.len());
        let mut si = 0;
        while si < stages_meta.len() {
            if stages_meta[si].0 == StageLabel::FixedUpdate {
                let mut end = si;
                while end < stages_meta.len() && stages_meta[end].0 == StageLabel::FixedUpdate {
                    end += 1;
                }
                let steps = Self::stage_steps(unsafe { &mut *world_ptr }, &StageLabel::FixedUpdate);
                for _ in 0..steps {
                    schedule.extend(si..end);
                }
                si = end;
            } else {
                schedule.push(si);
                si += 1;
            }
        }

        for &stage_idx in &schedule {
            let (label, system_ids, emit_event_types) = &stages_meta[stage_idx];
            if *label == StageLabel::Startup && self.startup_completed {
                continue;
            }
            // Evaluate each system's run_condition EXACTLY ONCE this frame and reuse
            // the result below — a stateful condition (`run_until`/`every_n_frames`,
            // whose internal counter advances on every `check`) must not be evaluated
            // twice per frame. Matches the documented contract: conditions run on the
            // main thread BEFORE the stage.
            let mut enabled: FxHashSet<SystemId> = FxHashSet::default();
            for &sys_id in system_ids {
                if let Some(&idx) = self.system_indices.get(&sys_id) {
                    if self.systems[idx].run_condition.evaluate(unsafe { &*world_ptr }) {
                        enabled.insert(sys_id);
                    }
                }
            }
            // D6: skip the stage (and the change window) when every system is
            // disabled by run_if, so stage_last_run doesn't advance past changes
            // made during the pause.
            if enabled.is_empty() {
                continue;
            }
            // D4: rebuild the SubWorld per stage so a Parallel system sees new
            // archetypes created by an earlier stage this frame (it was built once
            // before the loop and went stale). MIRI-CD: built from the RAW `world_ptr`
            // (not a `&World` reborrow) so the interleaved `&mut *world_ptr` writes below
            // do not invalidate it.
            let all_indices: Vec<usize> = (0..unsafe { &*world_ptr }.archetypes().len()).collect();
            let sub_world =
                unsafe { apex_core::SubWorld::from_raw(world_ptr as *const World, &all_indices) };
            {
                // TD-52: per-execution-stage change-detection window (cross-stage `Changed<T>`).
                self.open_stage_change_window(stage_idx, unsafe { &mut *world_ptr });
                // D6: the tick this stage runs at (see `run_hybrid_parallel`).
                let this_run = unsafe { &*world_ptr }.current_tick();
                for &sys_id in system_ids {
                    if !enabled.contains(&sys_id) {
                        continue;
                    }
                    if let Some(&index) = self.system_indices.get(&sys_id) {
                        // D6: per-system baseline (Copy — ends before `&mut systems`).
                        let window = self.system_window(sys_id);
                        let system = &mut self.systems[index];
                        let prof_t = main_prof_enabled().then(std::time::Instant::now);
                        match &mut system.kind {
                            SystemKind::Sequential(f) => f(unsafe { &mut *world_ptr }),
                            SystemKind::Parallel { system, .. } => {
                                // SAFETY: `cmds_ptr` points at `stage_cmds`, a single
                                // buffer alive until the end of the run; systems run
                                // sequentially on this thread, so no aliasing.
                                let ctx = unsafe {
                                    SystemContext::with_commands(
                                        std::slice::from_ref(&sub_world),
                                        cmds_ptr,
                                    )
                                    .with_last_run(window)
                                };
                                system.run(ctx);
                            }
                            SystemKind::NonSend { system, .. } => {
                                // Main-thread system — run_sequential is already
                                // single-threaded, so the same shared-buffer ctx is used.
                                let ctx = unsafe {
                                    SystemContext::with_commands(
                                        std::slice::from_ref(&sub_world),
                                        cmds_ptr,
                                    )
                                    .with_last_run(window)
                                };
                                system.run(ctx);
                            }
                        }
                        if let Some(t) = prof_t {
                            main_prof_record(&self.systems[index].name, t.elapsed());
                        }
                    }
                }

                // Apply deferred commands after each stage/step (rank order —
                // systems appended to `stage_cmds` in execution order).
                stage_cmds.apply(unsafe { &mut *world_ptr });

                // Per-stage flush — makes events available to the next stage
                if !emit_event_types.is_empty() {
                    unsafe { &mut *world_ptr }.flush_events_by_type(emit_event_types);
                }
                // D6: advance each enabled system's baseline to this stage's tick.
                self.advance_system_windows(&enabled, this_run);
            }
        }

        // Frame boundary: advance the change-tick (TD-9).
        unsafe { &mut *world_ptr }.advance_change_tick();

        self.startup_completed = true;
    }

    /// Run a stage via ASD (Adaptive Scope Distribution).
    ///
    /// ASD dynamically splits the entities of all the stage's systems into
    /// adaptively sized chunks, sorts them by archetype_id for cache locality,
    /// and distributes them across all Rayon workers.
    ///
    /// # Algorithm
    ///
    /// 1. Compute `total_entity_count` — the sum of entities of all systems in the stage.
    /// 2. `target_chunk = max(total_entity_count / num_workers / 2, MIN_CHUNK)`.
    /// 3. For each system:
    ///    - If entity_count <= target_chunk → 1 task (the whole archetype).
    ///    - If entity_count > target_chunk → split into chunks of ~target_chunk.
    /// 4. All chunks are collected into a Vec and sorted by archetype_id.
    /// 5. The chunks are run via `rayon::scope`.
    ///
    /// # Adaptivity
    ///
    /// - Few entities → small target_chunk → each system = 1 task (per-system scope).
    /// - Many entities → large target_chunk → tasks fill all workers evenly.
    /// - No if/else — one mechanism for every scenario.
    ///
    /// # Safety
    ///
    /// Each spawned task works on a DIFFERENT `SystemDescriptor`
    /// (different `AsdTask.ptr`), so concurrent `&mut` access
    /// to different systems is safe. The SubWorld is created locally inside spawn
    /// with the `(arch_idx, start, end)` restriction, which guarantees that
    /// different tasks do NOT overlap in the data of a single archetype.
    /// `cmds_base` — raw pointer (as `usize`, Copy+Send) to the base of the
    /// stage's per-system `Commands` slots (`stage_cmds` in `run_hybrid_parallel`),
    /// one per system in rank order. `slot_of` maps a `SystemId` to its slot index.
    /// Command-emitting systems are single-task (per-system scope), so each writes a
    /// unique slot; row-split (query-only) tasks use an inline buffer (D8b).
    fn run_stage_parallel(
        &mut self,
        stage_ids: &[SystemId],
        world: &World,
        cmds_base: usize,
        slot_of: &FxHashMap<SystemId, usize>,
        enabled: &FxHashSet<SystemId>,
    ) {
        let archetypes = world.archetypes();
        let num_workers = rayon::current_num_threads();

        // 0. Run conditions were already evaluated ONCE this frame by the caller
        //    (`enabled` set) — do NOT re-evaluate here, or a stateful condition
        //    (`run_until`/`every_n_frames`) would be double-counted. Systems absent
        //    from `enabled` are skipped.
        //    Sh1: buffers pooled in Scheduler scratch fields (detached via mem::take
        //    so `&mut self` method calls below don't alias them; restored at the end
        //    to retain capacity — zero steady-state allocation).
        let mut skipped_systems = std::mem::take(&mut self.scratch_skipped);
        skipped_systems.clear();
        for &sys_id in stage_ids {
            if !enabled.contains(&sys_id) {
                skipped_systems.insert(sys_id);
            }
        }

        // 1. Collect per-system info (SysInfo — module-level, pooled).
        let mut sys_infos = std::mem::take(&mut self.scratch_sys_infos);
        sys_infos.clear();
        let mut total_entity_count: usize = 0;

        // B2: NonSend (main-thread) systems of this stage — run on the calling thread
        // inside the scope below, concurrently with the worker tasks. Raw `!Send`
        // pointers (never worker-dispatched): captured by the `in_place_scope` op,
        // which runs on the current thread and carries no `Send` bound.
        let mut nonsend_infos: Vec<(SystemId, *mut dyn NonSendSystem, apex_core::Tick)> =
            Vec::new();

        for &sys_id in stage_ids {
            if skipped_systems.contains(&sys_id) {
                continue;
            }
            if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                // B2: a NonSend system is never chunked or worker-dispatched — collect
                // its box pointer + change-detection baseline and run it on the main
                // thread inside the scope.
                if matches!(self.systems[sys_idx].kind, SystemKind::NonSend { .. }) {
                    let last_run = self.system_window(sys_id);
                    if let SystemKind::NonSend { system, .. } = &mut self.systems[sys_idx].kind {
                        nonsend_infos.push((
                            sys_id,
                            &mut **system as *mut dyn NonSendSystem,
                            last_run,
                        ));
                    }
                    continue;
                }
                // Only systems with a concrete component filter (Filtered) have a
                // per-entity partition to split by rows. A system with `All` / no
                // record declares no component access (only resources / events /
                // whole-world), so it has nothing to partition — chunking it would
                // run its whole body (and any side effects) once per chunk (D2).
                // Such systems run inline as a single task.
                let is_filtered = matches!(
                    self.system_archetype_indices.get(&sys_id),
                    Some(SystemArchetypes::Filtered(_))
                );
                // Sh1: entity_count without materializing an arch-index Vec — the
                // slice is fetched from `system_archetype_indices` at task-build time.
                let entity_count: usize = match self.system_archetype_indices.get(&sys_id) {
                    Some(SystemArchetypes::Filtered(v)) => v
                        .iter()
                        .filter(|&&ai| ai < archetypes.len())
                        .map(|&ai| archetypes[ai].len())
                        .sum(),
                    // All / no record — system sees all archetypes.
                    _ => archetypes.iter().map(|a| a.len()).sum(),
                };

                if entity_count > 0 && is_filtered {
                    let access = self.systems[sys_idx].kind.access();
                    let has_events = access
                        .map(|a| !a.reads_event.is_empty() || !a.writes_event.is_empty())
                        .unwrap_or(false);
                    let uses_par_for_each = access.map(|a| a.uses_par_for_each).unwrap_or(false);
                    let needs_whole_world = access.map(|a| a.needs_whole_world).unwrap_or(false);
                    // No access (dynamic system) — state is unknown,
                    // conservatively treat it as stateful (no row-split).
                    let stateful = access.map(|a| a.stateful).unwrap_or(true);
                    // Resource mutation / Commands (TD-37): a side effect not local to the query
                    // partition ⇒ row-split would duplicate it. No access ⇒ conservatively true.
                    // `has_deferred` covers `system!`-macro command systems (`Cmd` sets
                    // `HAS_DEFERRED` but NOT `access.uses_commands` — only the `Commands`
                    // SystemParam sets the latter); both must forbid row-split, else the
                    // body (and its spawns) run once per chunk AND, post-D8b-layer-1, a
                    // row-split task's commands are dropped (it carries no per-system slot).
                    let non_query_side_effects = self.systems[sys_idx].has_deferred
                        || access
                            .map(|a| a.writes_resource || a.uses_commands)
                            .unwrap_or(true);
                    // The `access` borrow ends here; take a raw pointer to the
                    // `dyn ParSystem` itself (D3). A filtered system always
                    // declares component access, i.e. is `Parallel`; a
                    // `Sequential` system has nothing to run on this path.
                    let par_ptr: *mut dyn ParSystem = match &mut self.systems[sys_idx].kind {
                        SystemKind::Parallel { system, .. } => &mut **system,
                        // NonSend systems were already collected into `nonsend_infos`
                        // above (they `continue` before reaching here); a Sequential
                        // system cannot share a parallelizable stage. Defensive.
                        SystemKind::Sequential(_) | SystemKind::NonSend { .. } => continue,
                    };
                    total_entity_count += entity_count;
                    sys_infos.push(SysInfo {
                        ptr: SendPtr(par_ptr),
                        sys_id,
                        entity_count,
                        has_events,
                        uses_par_for_each,
                        needs_whole_world,
                        stateful,
                        non_query_side_effects,
                    });
                } else {
                    // A system with no entities (resources/events only) — run it immediately.
                    let slot = slot_of[&sys_id];
                    // D6: per-system baseline (Copy — borrow ends before `&mut systems`).
                    let window = self.system_window(sys_id);
                    let system = &mut self.systems[sys_idx];
                    if let SystemKind::Parallel { system: sys, .. } = &mut system.kind {
                        let all_indices: Vec<usize> = (0..archetypes.len()).collect();
                        let sw = unsafe {
                            apex_core::SubWorld::from_raw(world as *const World, &all_indices)
                        };
                        let sws = [sw];
                        // SAFETY: this system's private per-system slot (D8b), run
                        // single-task on the main thread here → unique `&mut`.
                        let ctx = unsafe {
                            SystemContext::with_commands(
                                &sws,
                                (cmds_base as *mut Commands).add(slot),
                            )
                            .with_last_run(window)
                        };
                        sys.run(ctx);
                    }
                }
            }
        }

        if sys_infos.is_empty() && nonsend_infos.is_empty() {
            self.scratch_skipped = skipped_systems;
            self.scratch_sys_infos = sys_infos;
            return;
        }

        // 2. Compute the target chunk size via adaptive_chunk_size
        //    from apex-core (used in Query::par_for_each).
        let per_system_entity = total_entity_count / sys_infos.len().max(1);
        let target_chunk = apex_core::world::adaptive_chunk_size(
            per_system_entity,
            num_workers,
            world.chunk_config(),
        );

        // Align the chunk size to 8 entities to avoid false sharing of
        // cache lines between adjacent chunks of one archetype (8 × sizeof(Position) = 96 bytes > 64).
        const CACHE_ALIGN_ENTITIES: usize = 8;
        let aligned_chunk = (target_chunk / CACHE_ALIGN_ENTITIES) * CACHE_ALIGN_ENTITIES;
        let effective_chunk = aligned_chunk.max(CACHE_ALIGN_ENTITIES);

        // 3. Create chunks for all systems (Sh1: pooled task buffer)
        let mut tasks = std::mem::take(&mut self.scratch_tasks);
        tasks.clear();

        for info in &sys_infos {
            // Sh1: fetch the arch-index slice from the cached map (no per-system clone).
            let arch_slice: &[usize] = match self.system_archetype_indices.get(&info.sys_id) {
                Some(SystemArchetypes::Filtered(v)) => v.as_slice(),
                _ => &[],
            };
            // Per-system scope (NO row-split — the system runs exactly once) for:
            //   a) systems with a small entity_count
            //   b) systems with events (Emit/Listen)
            //   c) systems with par_for_each — to avoid oversubscribing rayon
            //   d) systems with needs_whole_world — global access
            //   e) stateful systems (W3-4) — row-split would call
            //      run(&mut self) on one instance concurrently
            //   f) systems mutating a resource / Commands (TD-37) — the plain-fn body runs once
            //      per chunk, so a non-query-local side effect (a ResMut write, Commands)
            //      would be multiplied by the chunk count (+ a race across parallel chunks). Decomposition
            //      is allowed ONLY when a system's effects are confined to its per-entity query
            //      partition (component writes over disjoint row ranges).
            if info.has_events
                || info.uses_par_for_each
                || info.needs_whole_world
                || info.stateful
                || info.non_query_side_effects
                || info.entity_count <= effective_chunk
            {
                // Per-system scope: one task, all entities at once. D8b: single-task
                // → private per-system command slot (unique writer).
                let slot = slot_of[&info.sys_id];
                tasks.push(AsdTask {
                    ptr: info.ptr,
                    arch_indices: SmallVec::from_slice(arch_slice),
                    chunk_ranges: SmallVec::new(), // empty = the whole SubWorld
                    cmds: Some(SendPtr((cmds_base as *mut Commands).wrapping_add(slot))),
                    // D6: this system's per-system change-detection baseline, captured
                    // now on the main thread and carried into the rayon task.
                    last_run: self.system_window(info.sys_id),
                });
            } else {
                // A large multi-archetype system — ASD split
                let mut remaining = info.entity_count;
                let mut arch_iter = arch_slice.iter().copied();
                let mut current_arch = arch_iter.next();
                let mut arch_offset: usize = 0;

                while remaining > 0 {
                    let mut chunk_ranges: SmallVec<[(usize, usize, usize); 4]> = SmallVec::new();
                    // The set of archetype_ids in this chunk — to narrow arch_indices
                    let mut chunk_arch_set: SmallVec<[usize; 8]> = SmallVec::new();
                    let mut chunk_remaining = effective_chunk.min(remaining);

                    while chunk_remaining > 0 {
                        let Some(arch_idx) = current_arch else {
                            break;
                        };
                        if arch_idx >= archetypes.len() {
                            current_arch = arch_iter.next();
                            arch_offset = 0;
                            continue;
                        }
                        let arch_len = archetypes[arch_idx].len();
                        if arch_len == 0 || arch_offset >= arch_len {
                            current_arch = arch_iter.next();
                            arch_offset = 0;
                            continue;
                        }

                        let available = arch_len - arch_offset;
                        let take = chunk_remaining.min(available);

                        chunk_ranges.push((arch_idx, arch_offset, arch_offset + take));
                        // Add archetype_id to the set if not already present
                        if !chunk_arch_set.contains(&arch_idx) {
                            chunk_arch_set.push(arch_idx);
                        }
                        arch_offset += take;
                        chunk_remaining -= take;
                        remaining -= take;

                        if arch_offset >= arch_len {
                            current_arch = arch_iter.next();
                            arch_offset = 0;
                        }
                    }

                    if !chunk_ranges.is_empty() {
                        // Row-split (query-only) task: no command slot (D8b) — such a
                        // system declares no side effects, so it emits no commands.
                        tasks.push(AsdTask {
                            ptr: info.ptr,
                            arch_indices: chunk_arch_set,
                            chunk_ranges,
                            cmds: None,
                            // D6: all row-split chunks of one system share its baseline.
                            last_run: self.system_window(info.sys_id),
                        });
                    }
                }
            }
        }

        // 4. Sort chunks by archetype_id for cache locality
        tasks.sort_unstable_by_key(|t| t.chunk_ranges.first().map(|&(a, _, _)| a).unwrap_or(0));

        // B2: SubWorld index set (all archetypes) for the main-thread NonSend
        // systems, materialized once. Empty when there are none (no cost).
        let all_arch_indices: Vec<usize> = if nonsend_infos.is_empty() {
            Vec::new()
        } else {
            (0..archetypes.len()).collect()
        };

        // 5. Run via rayon::in_place_scope — the op runs on THIS (main) thread, so it
        //    carries no `Send` bound and may run `!Send` NonSend systems in place while
        //    the spawned worker tasks run in parallel. `task.cmds`
        //    (Option<SendPtr<Commands>>) is Copy+Send — each single-task system carries
        //    the pointer to its own per-system slot; row-split tasks carry `None`.
        rayon::in_place_scope(|s| {
            for task in &tasks {
                s.spawn(move |_| {
                    // SAFETY (W3-4 + D3): multiple tasks of one system exist
                    // ONLY for stateless systems (`stateful`/`has_events`/
                    // `uses_par_for_each`/`needs_whole_world` get a single
                    // per-system scope). For them `run(&mut self)` neither reads
                    // nor writes any bytes of self (ZST/no captures). `task.ptr` points
                    // DIRECTLY at the `dyn ParSystem` (a ZST target), so concurrent
                    // `&mut *task.ptr.0` do not alias any real bytes — unlike
                    // the former `&mut SystemDescriptor` (owns a `String` name etc.).
                    // Task row ranges are disjoint by construction.
                    unsafe {
                        let sys: &mut dyn ParSystem = &mut *task.ptr.0;
                        let sub = if task.chunk_ranges.is_empty() {
                            // Full SubWorld — all of the system's archetypes, unrestricted
                            apex_core::SubWorld::from_raw(world as *const World, &task.arch_indices)
                        } else {
                            // SubWorld with range restrictions and narrowed arch_indices
                            apex_core::SubWorld::from_raw_with_ranges(
                                world as *const World,
                                &task.arch_indices,
                                &task.chunk_ranges,
                            )
                        };
                        let sws = std::slice::from_ref(&sub);
                        // D8b: single-task systems carry a private per-system command
                        // slot (`Some`), applied in rank order for deterministic
                        // command ordering; row-split (query-only) tasks carry `None`
                        // and use an inline buffer (they emit no commands).
                        // SAFETY: a command-emitting system is single-task (per-system
                        // scope), so exactly one task ever forms `&mut *slot.0`.
                        // D6: apply this system's per-system change-detection baseline.
                        let ctx = match task.cmds {
                            Some(slot) => SystemContext::with_commands(sws, slot.0),
                            None => SystemContext::new(sws),
                        }
                        .with_last_run(task.last_run);
                        sys.run(ctx);
                    }
                });
            }

            // B2: run this stage's NonSend systems on the current (main) thread,
            // concurrently with the worker tasks spawned above. Their access is
            // disjoint from every worker task's (compile placed conflicting systems in
            // different stages), so this is the same disjoint-data argument that makes
            // the worker tasks sound — extended to the main thread as one more actor.
            for &(sys_id, ptr, last_run) in &nonsend_infos {
                // SAFETY: `ptr` targets this system's `dyn NonSendSystem` box — distinct
                // from every worker task's `AsdTask.ptr` (those target `Parallel`
                // boxes), and only the main thread ever forms `&mut *ptr`. The full
                // SubWorld read + this system's private per-system command slot are
                // disjoint from every concurrent worker task (stage conflict-freedom).
                // `!Send` is sound: `in_place_scope` runs this op on the main thread.
                unsafe {
                    let sys: &mut dyn NonSendSystem = &mut *ptr;
                    let sub = apex_core::SubWorld::from_raw(
                        world as *const World,
                        &all_arch_indices,
                    );
                    let sws = std::slice::from_ref(&sub);
                    let slot = slot_of[&sys_id];
                    let ctx = SystemContext::with_commands(
                        sws,
                        (cmds_base as *mut Commands).add(slot),
                    )
                    .with_last_run(last_run);
                    sys.run(ctx);
                }
            }
        });

        // Sh1: return pooled buffers to the Scheduler to retain capacity next frame.
        // `sys_infos` holds raw `SendPtr`s into `self.systems`; they are cleared and
        // rebuilt each frame before any use, never dereferenced while stale.
        self.scratch_skipped = skipped_systems;
        self.scratch_sys_infos = sys_infos;
        self.scratch_tasks = tasks;
    }

    /// Parallel execution via ASD (Adaptive Scope Distribution).
    ///
    /// A stage with exclusively parallel systems uses
    /// [`run_stage_parallel`] (ASD). A stage with sequential systems or
    /// a mix runs sequentially, and each system gets
    /// a full `SubWorld` over all archetypes.
    ///
    fn run_hybrid_parallel(&mut self, world_ptr: *mut World) {
        // Sh1: move the plan OUT of `self` (mem::take) so its stages can be borrowed
        // while `&mut self` methods run in the loop — no per-frame clone of the stage
        // metadata (labels, `system_ids`, `emit_event_types`). Restored at the end.
        // If a system panics mid-run the plan is lost → recompiled on the next run
        // (a panicking system is already a bug; correctness is preserved).
        // Stages are NOT pre-filtered for Startup here: startup stages are skipped
        // in-loop, so `stage_pos` equals the plan-stage index (the stable TD-52
        // change-detection key), exactly as the old `i` did.
        let plan = self
            .execution_plan
            .take()
            .expect("execution_plan present before run");

        {
            let w = unsafe { &mut *world_ptr };
            for system in &self.systems {
                if let Some(access) = system.kind.access() {
                    for &(type_id, cap) in &access.event_reserves {
                        w.event_reserve_by_type(type_id, cap);
                    }
                }
            }
        }

        let const_ptr = world_ptr as *const World;
        let mut prev_arch_count = unsafe { &*const_ptr }.archetypes().len();

        // Stage-parallelism gating knobs live on the World's `ChunkConfig` (wave 3
        // §1.7). Config is stable during a run, so read once here.
        let (stage_par_min, auto_disable_stage) = {
            let cc = unsafe { &*const_ptr }.chunk_config();
            (cc.stage_parallel_min_entities, cc.auto_disable_stage_parallel)
        };

        // D8b: per-system command buffers — one slot per system in rank (stage_ids)
        // order, rebuilt per stage below and applied in rank order. This makes both
        // command ordering AND (via per-system block reservers) entity-id assignment
        // deterministic under parallel spawning — a guarantee Bevy does not give.
        // A local, not a Scheduler field: `Commands` is `!Send`, so a field would
        // make `Scheduler: !Send` (breaks `ThreadPool::install`; see the scratch
        // note). Reused across stages via `clear` + `resize_with`.
        let mut stage_cmds: Vec<Commands> = Vec::new();
        let mut slot_of: FxHashMap<SystemId, usize> = FxHashMap::default();
        // D8b: systems seeded with an id block this stage (sys_id, block size, a
        // clone of the reserver sharing the block cursor) — drained after apply to
        // update adaptive-sizing history. Empty unless `deterministic_spawn`.
        let mut seeded: Vec<(SystemId, u32, apex_core::entity::EntityReserver)> = Vec::new();

        // Sh1: pooled arch-length snapshot (cleared + refilled, capacity retained).
        let mut arch_lengths = std::mem::take(&mut self.scratch_arch_lengths);
        arch_lengths.clear();
        arch_lengths.extend(unsafe { &*const_ptr }.archetypes().iter().map(|a| a.len()));

        // Build the execution order (D2-5). Stages sharing a StageLabel are
        // contiguous, so the FixedUpdate stages form one block. That block steps
        // as a GROUP: the FixedTime accumulator is drained ONCE and the whole
        // block runs `steps` times as (A; B; …) × N. This fixes D1 — draining
        // per stage let the first FixedUpdate stage consume all the steps, so any
        // later FixedUpdate stage (a conflict split the label across several
        // execution stages) ran zero times, and the order was A×N then B×N rather
        // than the correct (A;B)×N. Every non-FixedUpdate stage runs exactly once.
        let mut schedule = std::mem::take(&mut self.scratch_schedule);
        schedule.clear();
        let mut si = 0;
        while si < plan.stages.len() {
            if plan.stages[si].label == StageLabel::FixedUpdate {
                let mut end = si;
                while end < plan.stages.len() && plan.stages[end].label == StageLabel::FixedUpdate {
                    end += 1;
                }
                let steps = Self::stage_steps(unsafe { &mut *world_ptr }, &StageLabel::FixedUpdate);
                for _ in 0..steps {
                    schedule.extend(si..end);
                }
                si = end;
            } else {
                schedule.push(si);
                si += 1;
            }
        }

        for &stage_pos in &schedule {
            let stage = &plan.stages[stage_pos];
            // Startup runs only on the first frame (was a pre-loop filter on `stages`).
            if stage.label == StageLabel::Startup && self.startup_completed {
                continue;
            }
            // D4: an earlier stage may have spawned entities into NEW archetypes.
            // Refresh the cached per-system archetype indices before this stage so
            // a Filtered parallel system sees them. Previously only the parallel
            // branch refreshed (in its tail), so new archetypes created by a
            // non-parallel / fallback stage were missed until the next frame.
            {
                let w = unsafe { &*const_ptr };
                let cur = w.archetypes().len();
                if cur != prev_arch_count {
                    prev_arch_count = cur;
                    self.compute_archetype_indices(w);
                }
            }
            // `stage_idx` (plan-stage index) is the stable TD-52 change key.
            let stage_idx = stage_pos;
            let stage_ids = &stage.system_ids;
            let all_parallel = stage.all_parallel;
            let emit_event_types = &stage.emit_event_types;
            // Evaluate each system's run_condition EXACTLY ONCE this frame into
            // `enabled`, then reuse it in every branch below (SEQ-fallback, per-stage
            // sequential, and `run_stage_parallel`). A stateful condition
            // (`run_until`/`every_n_frames`) must not be evaluated more than once per
            // frame. Contract: conditions run on the main thread BEFORE the stage.
            let mut enabled: FxHashSet<SystemId> = FxHashSet::default();
            for &sys_id in stage_ids {
                if let Some(&idx) = self.system_indices.get(&sys_id) {
                    if self.systems[idx]
                        .run_condition
                        .evaluate(unsafe { &*const_ptr })
                    {
                        enabled.insert(sys_id);
                    }
                }
            }
            // D6: skip the whole stage — including opening the change window — when
            // every system is disabled by its run_condition. Otherwise the window
            // would advance stage_last_run past changes made during a run_if pause,
            // and a system resuming later would miss Changed<T> from those frames.
            if enabled.is_empty() {
                continue;
            }
            // TD-52: per-execution-stage change-detection window (cross-stage `Changed<T>`).
            self.open_stage_change_window(stage_idx, unsafe { &mut *world_ptr });
            // D6: the tick this stage runs at. Each enabled system reads its OWN
            // `system_window` baseline when building its context, and its baseline is
            // advanced to `this_run` after it runs (below) — so a gated system sees
            // changes since IT last ran, not since the stage last ran.
            let this_run = unsafe { &*const_ptr }.current_tick();

            // D8b: (re)build per-system command slots for this stage in rank order
            // (position in `stage_ids`). One slot per system; command-emitting
            // single-task systems write their slot, applied in rank order below.
            stage_cmds.clear();
            stage_cmds.resize_with(stage_ids.len(), Commands::new);
            slot_of.clear();
            seeded.clear();
            for (slot, &sys_id) in stage_ids.iter().enumerate() {
                slot_of.insert(sys_id, slot);
                // D8b: seed a private, rank-deterministic id block for each command-
                // emitting system so parallel spawns assign identical ids run-to-run.
                // Seeded for ALL command systems regardless of the SEQ/PAR path taken
                // below → id assignment is independent of the cost-model's timing.
                // `reserve_entity_block` runs here on the main thread in rank
                // (stage_ids) order, so the block bases are deterministic.
                if self.deterministic_spawn {
                    // "Uses Commands" = `has_deferred` (system!-macro path) OR
                    // `access.uses_commands` (Commands SystemParam path) — see the
                    // non_query_side_effects note; only these systems spawn/defer.
                    let uses_cmds = self
                        .system_indices
                        .get(&sys_id)
                        .map(|&idx| {
                            self.systems[idx].has_deferred
                                || self.systems[idx]
                                    .kind
                                    .access()
                                    .map(|a| a.uses_commands)
                                    .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if uses_cmds {
                        // Seed block + escrow (overflow frontier): a spike beyond the
                        // adaptive block still draws deterministic ids from the escrow
                        // tail. `size` tracked here is the SEEDED size, so
                        // `commit_spawn_history` measures actual usage as size−remaining.
                        let size = self.seed_size_for(sys_id);
                        if size > 0 {
                            let reserver = unsafe { &*const_ptr }.reserve_entity_block(size);
                            stage_cmds[slot].set_reserver(reserver.clone());
                            seeded.push((sys_id, size, reserver));
                        }
                    }
                }
            }
            let cmds_base: *mut Commands = stage_cmds.as_mut_ptr();

            if !all_parallel {
                {
                    // MIRI-CD: build the SubWorld from the RAW `const_ptr` (= `world_ptr`),
                    // not a `&*const_ptr` reborrow — the interleaved `&mut *world_ptr`
                    // writes below are children of that same raw tag, so they never
                    // invalidate the SubWorld's read pointer.
                    let all_indices: Vec<usize> =
                        (0..unsafe { &*const_ptr }.archetypes().len()).collect();
                    let sub_world = unsafe { apex_core::SubWorld::from_raw(const_ptr, &all_indices) };
                    for &sys_id in stage_ids {
                        if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                            if !enabled.contains(&sys_id) {
                                continue;
                            }
                            // D6: this system's per-system change-detection baseline
                            // (Copy, so the immutable borrow ends before `&mut systems`).
                            let window = self.system_window(sys_id);
                            let system = &mut self.systems[sys_idx];
                            let prof_t = main_prof_enabled().then(std::time::Instant::now);
                            match &mut system.kind {
                                SystemKind::Sequential(f) => f(unsafe { &mut *world_ptr }),
                                SystemKind::Parallel { system, .. } => {
                                    // SAFETY: this system's private per-system slot
                                    // (D8b); it runs single-task on the main thread
                                    // here, so the `&mut` is unique.
                                    let slot = slot_of[&sys_id];
                                    let ctx = unsafe {
                                        SystemContext::with_commands(
                                            std::slice::from_ref(&sub_world),
                                            cmds_base.add(slot),
                                        )
                                        .with_last_run(window)
                                    };
                                    system.run(ctx);
                                }
                                SystemKind::NonSend { system, .. } => {
                                    // Main-thread system (!Send state). Same private
                                    // per-system slot + single-task guarantee as the
                                    // Parallel arm — here it simply runs on the main
                                    // thread by construction (this whole branch does).
                                    let slot = slot_of[&sys_id];
                                    let ctx = unsafe {
                                        SystemContext::with_commands(
                                            std::slice::from_ref(&sub_world),
                                            cmds_base.add(slot),
                                        )
                                        .with_last_run(window)
                                    };
                                    system.run(ctx);
                                }
                            }
                            if let Some(t) = prof_t {
                                main_prof_record(&self.systems[sys_idx].name, t.elapsed());
                            }
                        }
                    }
                }
                {
                    let w = unsafe { &mut *world_ptr };
                    // D8b: apply per-system command buffers in rank order (slot =
                    // position in stage_ids) — deterministic command application.
                    for cmds in &mut stage_cmds {
                        cmds.apply(w);
                    }
                }
                // No reset needed: `stage_cmds` is cleared + rebuilt each stage.
                // D8b: adaptive block sizing + reclaim unused block tails (bounds
                // id-space under churn). After apply (records grown by flush).
                self.commit_spawn_history(&mut seeded, unsafe { &mut *world_ptr });
                if !emit_event_types.is_empty() {
                    unsafe { &mut *world_ptr }.flush_events_by_type(emit_event_types);
                }
                // D6: advance each enabled system's baseline to this stage's tick.
                self.advance_system_windows(&enabled, this_run);
                continue;
            }

            // Sh2: SEQ/PAR decision (verdict extracted into `stage_prefers_seq`).
            // `stage_entity_count` is only needed when a path consults it: an explicit
            // floor, the auto-disable heuristic, or ParallelPolicy::Fixed. Otherwise
            // (CostModel with no floor) it is not read, so skip the O(archetypes) sum.
            let needs_entity_count = stage_par_min > 0
                || auto_disable_stage
                || self.parallel_policy == ParallelPolicy::Fixed;
            let stage_entity_count: usize = if needs_entity_count {
                let total_entities: usize = arch_lengths.iter().sum();
                stage_ids
                    .iter()
                    .filter_map(|&sys_id| self.system_archetype_indices.get(&sys_id))
                    .map(|indices| match indices {
                        SystemArchetypes::All => total_entities,
                        SystemArchetypes::Filtered(v) => v
                            .iter()
                            .copied()
                            .filter(|&ai| ai < arch_lengths.len())
                            .map(|ai| arch_lengths[ai])
                            .sum(),
                    })
                    .sum()
            } else {
                0
            };
            let should_fallback = self.stage_prefers_seq(
                stage_idx,
                stage_entity_count,
                stage_ids.len(),
                stage_par_min,
                auto_disable_stage,
            );
            let stage_t0 = std::time::Instant::now();

            if should_fallback {
                {
                    // MIRI-CD: SubWorld from the raw `const_ptr` (see the `!all_parallel`
                    // branch) so the interleaved `&mut *world_ptr` writes do not
                    // invalidate it.
                    let all_indices: Vec<usize> =
                        (0..unsafe { &*const_ptr }.archetypes().len()).collect();
                    let sub_world = unsafe { apex_core::SubWorld::from_raw(const_ptr, &all_indices) };
                    for &sys_id in stage_ids {
                        if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                            if !enabled.contains(&sys_id) {
                                continue;
                            }
                            // D6: per-system change-detection baseline (see branch above).
                            let window = self.system_window(sys_id);
                            let system = &mut self.systems[sys_idx];
                            let prof_t = main_prof_enabled().then(std::time::Instant::now);
                            match &mut system.kind {
                                SystemKind::Sequential(f) => f(unsafe { &mut *world_ptr }),
                                SystemKind::Parallel { system, .. } => {
                                    // SAFETY: this system's private per-system slot
                                    // (D8b); it runs single-task on the main thread
                                    // here, so the `&mut` is unique.
                                    let slot = slot_of[&sys_id];
                                    let ctx = unsafe {
                                        SystemContext::with_commands(
                                            std::slice::from_ref(&sub_world),
                                            cmds_base.add(slot),
                                        )
                                        .with_last_run(window)
                                    };
                                    system.run(ctx);
                                }
                                SystemKind::NonSend { system, .. } => {
                                    // Main-thread system (!Send state). Same private
                                    // per-system slot + single-task guarantee as the
                                    // Parallel arm — here it simply runs on the main
                                    // thread by construction (this whole branch does).
                                    let slot = slot_of[&sys_id];
                                    let ctx = unsafe {
                                        SystemContext::with_commands(
                                            std::slice::from_ref(&sub_world),
                                            cmds_base.add(slot),
                                        )
                                        .with_last_run(window)
                                    };
                                    system.run(ctx);
                                }
                            }
                            if let Some(t) = prof_t {
                                main_prof_record(&self.systems[sys_idx].name, t.elapsed());
                            }
                        }
                    }
                }
                {
                    let w = unsafe { &mut *world_ptr };
                    // D8b: apply per-system command buffers in rank order (slot =
                    // position in stage_ids) — deterministic command application.
                    for cmds in &mut stage_cmds {
                        cmds.apply(w);
                    }
                }
                // No reset needed: `stage_cmds` is cleared + rebuilt each stage.
            } else {
                self.run_stage_parallel(
                    stage_ids,
                    unsafe { &*const_ptr },
                    cmds_base as usize,
                    &slot_of,
                    &enabled,
                );

                {
                    let w = unsafe { &mut *world_ptr };
                    // D8b: apply per-system command buffers in rank order (slot =
                    // position in stage_ids) — deterministic command application.
                    for cmds in &mut stage_cmds {
                        cmds.apply(w);
                    }
                }
                {
                    let w = unsafe { &*const_ptr };
                    let cur = w.archetypes().len();
                    if cur != prev_arch_count {
                        prev_arch_count = cur;
                        self.compute_archetype_indices(w);
                    }
                }
            }
            // D8b: adaptive block sizing + reclaim unused block tails (covers both the
            // SEQ-fallback and PAR branches). After apply (records grown by flush).
            self.commit_spawn_history(&mut seeded, unsafe { &mut *world_ptr });
            if !emit_event_types.is_empty() {
                unsafe { &mut *world_ptr }.flush_events_by_type(emit_event_types);
            }
            // D6: advance each enabled system's baseline to this stage's tick (covers the
            // SEQ-fallback and PAR branches).
            self.advance_system_windows(&enabled, this_run);
            // Sh2: fold the measured stage time into the per-stage cost EMA (drives
            // the SEQ/PAR decision next frame). `should_fallback` = the mode we ran.
            self.record_stage_cost(stage_idx, stage_t0.elapsed(), should_fallback);
        }

        // Sh1: return the plan and pooled buffers to `self` (capacity retained).
        self.execution_plan = Some(plan);
        self.scratch_arch_lengths = arch_lengths;
        self.scratch_schedule = schedule;
    }

    /// The stage's execution count this frame (D2-5): FixedUpdate — from
    /// the [`FixedTime`] accumulator (no resource — 1, a normal stage);
    /// all other stages — always 1.
    fn stage_steps(world: &mut World, label: &StageLabel) -> usize {
        if *label != StageLabel::FixedUpdate {
            return 1;
        }
        world
            .try_resource_mut::<crate::fixed::FixedTime>()
            .map(|mut ft| {
                // PE-C6: `ResMut` stamps lazily on mutable deref — do not touch
                // the accumulator when no step is due, or FixedTime reads as
                // "changed" every frame forever.
                if ft.dt <= 0.0 || ft.overstep() < ft.dt {
                    0
                } else {
                    ft.drain_steps()
                }
            })
            .unwrap_or(1)
    }

}
