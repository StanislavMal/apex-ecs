use super::*;

impl Scheduler {
    // ── Compilation ─────────────────────────────────────────────

    /// Compile the schedule.
    ///
    /// Builds/updates the dependency graph and finds parallel stages.
    /// If the graph is unchanged since the last compile, only the topo
    /// sort is recomputed (added nodes are already in the graph).
    ///
    /// Also computes, per system, the archetype indices it needs
    /// (for building the SubWorld in run_hybrid_parallel).
    pub fn compile(&mut self) -> Result<(), SchedulerError> {
        // Early out: graph unchanged and the plan is already built
        if !self.graph_dirty && self.execution_plan.is_some() {
            return Ok(());
        }

        // The plan (and thus execution-stage indices) is rebuilt — reset the per-stage
        // change-detection baselines (TD-52). One extra "everything changed" next frame is safe.
        self.stage_last_run.clear();
        // Sh2: stage indices changed — drop the cost-model history (it re-learns in a
        // couple of frames; a stale index would mis-classify a different stage).
        self.stage_cost_ema_ns.clear();
        self.stage_ran_seq.clear();

        if self.type_names.is_empty() {
            log::debug!(
                "Scheduler::compile: type_names is empty. \
                 Call populate_type_names(&world.registry()) or \
                 compile_with_world(&world) to show component names \
                 in debug_plan_verbose()"
            );
        }

        if self.graph_dirty {
            // Resolve the config-declared order (`.before/.after/.chain`) into
            // id edges BEFORE building the graph — all names are now known
            // (forward references allowed). A "name not found" error is loud (§0.2a).
            self.resolve_pending_orderings()?;
            // Incremental update: add only new nodes and edges
            self.add_new_nodes_and_edges()?;
            self.graph_dirty = false;
        }

        // Topological sort of all systems → parallelism levels
        let levels = self.dependency_graph.parallel_levels().map_err(|_| {
            let cycle_info = self.find_cycle_description();
            SchedulerError::CircularDependency { cycle_info }
        })?;

        // For each topo-sort level, split system_ids by stage_label.
        // Then merge the results per label in priority order.
        use rustc_hash::FxHashMap;
        use std::collections::BTreeMap;
        let mut label_stages: BTreeMap<u8, Vec<Stage>> = BTreeMap::new();

        for level in &levels {
            let mut level_by_label: FxHashMap<StageLabel, Vec<SystemId>> = FxHashMap::default();
            for &node in level {
                if let Some(&sys_id) = self.dependency_graph.node_data(node) {
                    // O(1) lookup via system_indices instead of O(N) find()
                    if let Some(system) = self
                        .system_indices
                        .get(&sys_id)
                        .and_then(|&idx| self.systems.get(idx))
                    {
                        level_by_label
                            .entry(system.stage_label.clone())
                            .or_default()
                            .push(sys_id);
                    }
                }
            }
            // D8: determinism — several `Custom(_)` labels share priority 7, so
            // their relative order would come from the FxHashMap iteration
            // (unstable, and different on wasm32). Sort labels (StageLabel: Ord,
            // Custom ordered by name) so stage order is reproducible run-to-run.
            let mut labeled: Vec<(StageLabel, Vec<SystemId>)> =
                level_by_label.into_iter().collect();
            labeled.sort_by(|a, b| a.0.cmp(&b.0));
            for (label, ids) in labeled {
                let prio = label.priority();
                // Split into sub-stages at apply_deferred_after markers
                let sub_groups = split_at_apply_boundaries(&ids, &self.systems, &self.explicit_orderings);
                for group_ids in sub_groups {
                    let all_parallel = group_ids.iter().all(|sid| {
                        self.system_indices
                            .get(sid)
                            .and_then(|&idx| self.systems.get(idx))
                            .map(|s| s.kind.is_parallel())
                            .unwrap_or(false)
                    });
                    label_stages
                        .entry(prio)
                        .or_default()
                        .push(Stage::new(label.clone(), group_ids, all_parallel));
                }
            }
        }

        // Collect all stages in priority order or the user-defined order
        let mut stages: Vec<Stage> = Vec::new();

        if let Some(order) = &self.stage_order {
            // User-defined stage order
            let mut stage_map: FxHashMap<StageLabel, Vec<Stage>> = FxHashMap::default();
            for (_prio, mut s_stages) in label_stages {
                for stage in s_stages.drain(..) {
                    stage_map
                        .entry(stage.label.clone())
                        .or_default()
                        .push(stage);
                }
            }
            for label in order {
                if let Some(mut s_stages) = stage_map.remove(label) {
                    stages.append(&mut s_stages);
                }
            }
            // Stages not listed in the order — append them, deterministically
            // (D8: by label, otherwise FxHashMap order is unpredictable).
            let mut remaining: Vec<Stage> = stage_map.into_values().flatten().collect();
            remaining.sort_by(|a, b| a.label.cmp(&b.label));
            stages.append(&mut remaining);
        } else {
            // Standard priority order (Startup → First → ... → Last → Custom)
            for (_prio, mut s_stages) in label_stages {
                stages.append(&mut s_stages);
            }
        }

        let flat_order: Vec<SystemId> = stages
            .iter()
            .flat_map(|s| s.system_ids.iter().copied())
            .collect();

        // Collect event_writes for the per-stage flush
        for stage in &mut stages {
            let mut emit_types: FxHashSet<TypeId> = FxHashSet::default();
            for &sys_id in &stage.system_ids {
                if let Some(system) = self
                    .system_indices
                    .get(&sys_id)
                    .and_then(|&idx| self.systems.get(idx))
                {
                    if let Some(access) = system.kind.access() {
                        for &(tid, _) in &access.writes_event {
                            emit_types.insert(tid);
                        }
                    }
                }
            }
            stage.emit_event_types = emit_types.into_iter().collect();
        }

        self.execution_plan = Some(ExecutionPlan { stages, flat_order });
        Ok(())
    }

    /// Compile the schedule, first populating component names.
    ///
    /// Equivalent to calling `populate_type_names(world.registry())` then `compile()`.
    /// After this, `debug_plan_verbose()` shows real component names.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut sched = Scheduler::new();
    /// // ... add systems ...
    /// sched.compile_with_world(&world).expect("schedule error");
    /// println!("{}", sched.debug_plan_verbose()); // with component names!
    /// ```
    pub fn compile_with_world(&mut self, world: &World) -> Result<(), SchedulerError> {
        self.populate_type_names(world.registry());
        self.compile()
    }

    /// Compute, per system, the archetype indices it needs.
    ///
    /// Called after compile() before run(), once the World exists.
    /// Uses AccessDescriptor.reads/writes (TypeId) to filter.
    pub(crate) fn compute_archetype_indices(&mut self, world: &apex_core::World) {
        let archetypes = world.archetypes();
        let arch_count = archetypes.len();

        // Cache: if the archetype count is unchanged, skip the recompute
        if arch_count == self.cached_archetype_count && !self.system_archetype_indices.is_empty() {
            return;
        }

        // Archetypes are append-only: as the world grows, existing lists are EXTENDED
        // only by the tail archetypes[prev_count..] — a full recompute O(systems ×
        // archetypes) was quadratic on spawn bursts (C-8). A full scan runs
        // only on the first call and for write-less systems (new after compile).
        let prev_count = if self.system_archetype_indices.is_empty() {
            0
        } else {
            self.cached_archetype_count.min(arch_count)
        };

        if arch_count == 0 {
            self.cached_archetype_count = 0;
            return;
        }

        let registry = world.registry();

        // For each system, find the matching archetypes.
        // Use the `any()` criterion: an archetype matches if it contains
        // at least one component from the system. This is correct for SubWorld —
        // the Query itself later filters out non-matching archetypes via matches_archetype.
        for system in &self.systems {
            let access = match system.kind.access() {
                Some(a) => a,
                None => continue, // Sequential — does not use SubWorld
            };

            // Only component TypeIds determine which archetypes a system needs.
            // reads_event / writes_event are virtual accesses for the scheduler,
            // they do not correspond to real data in archetypes.
            let mut system_type_ids: Vec<std::any::TypeId> = Vec::new();
            system_type_ids.extend(access.reads.iter().copied());
            system_type_ids.extend(access.writes.iter().copied());

            if system_type_ids.is_empty() {
                // A system with no components (resources/events only) — the marker
                // "unrestricted" instead of a materialized Vec of all indices.
                self.system_archetype_indices
                    .insert(system.id, SystemArchetypes::All);
                continue;
            }

            // Resolve the ComponentId once per system, not per archetype.
            // (An unresolved TypeId is harmless: an archetype with the component cannot
            // exist before the component is registered.)
            let cids: Vec<apex_core::ComponentId> = system_type_ids
                .iter()
                .filter_map(|tid| registry.get_id_by_type(tid))
                .collect();

            // Extend an existing list from prev_count; a new one (a system that
            // appeared since the last call) is scanned from scratch.
            let start = match self.system_archetype_indices.get(&system.id) {
                Some(SystemArchetypes::Filtered(_)) => prev_count,
                Some(SystemArchetypes::All) => continue, // access set is static
                None => {
                    self.system_archetype_indices
                        .insert(system.id, SystemArchetypes::Filtered(Vec::new()));
                    0
                }
            };
            let Some(SystemArchetypes::Filtered(indices)) =
                self.system_archetype_indices.get_mut(&system.id)
            else {
                unreachable!("the branches above guarantee Filtered");
            };

            for (offset, arch) in archetypes[start..].iter().enumerate() {
                if cids.iter().any(|&cid| arch.has_component(cid)) {
                    indices.push(start + offset);
                }
            }
        }

        self.cached_archetype_count = arch_count;
    }

    /// Checks whether an edge exists between two nodes.
    fn has_edge_between(&self, from: Index, to: Index) -> bool {
        // O(1) check via edge_set instead of O(N) successors()
        self.edge_set.contains(&(from, to))
    }

    /// Incrementally add new nodes and edges to the graph.
    ///
    /// Adds only systems not yet in `graph_nodes`,
    /// and edges for new/changed systems.
    ///
    /// ## Optimization
    /// - On the first compile (empty graph), `has_path()` checks are unnecessary,
    ///   since an empty graph cannot have cycles. This removes O(N²) BFS runs.
    /// - `has_path()` uses the reusable Graph.bfs_visited/bfs_queue buffers
    ///   instead of allocating on every call.
    fn add_new_nodes_and_edges(&mut self) -> Result<(), SchedulerError> {
        let n = self.systems.len();

        // ── 1. Add new nodes (systems) ──────────────────
        let mut new_system_indices = Vec::new();
        for (idx, system) in self.systems.iter().enumerate() {
            if !self.graph_nodes.contains_key(&system.id) {
                let node = self.dependency_graph.add_node(system.id);
                self.graph_nodes.insert(system.id, node);
                new_system_indices.push(idx);
            }
        }

        // If there are no new systems but the graph is marked dirty (e.g. dependencies changed)
        // we must recompute edges for existing systems
        let systems_to_process = if new_system_indices.is_empty() {
            // Process all systems (dependencies may have changed)
            (0..n).collect::<Vec<_>>()
        } else {
            // Process only new systems and their links to existing ones
            new_system_indices
        };

        // Optimization: on the first compile() the graph is still empty — has_path() is always false.
        // Skip the O(N²) BFS runs, since an empty graph cannot have cycles.
        let has_existing_edges = !self.edge_set.is_empty();

        // ── 2. Explicit dependencies for new/changed systems ──
        for &idx in &systems_to_process {
            let system = &self.systems[idx];

            // Runs after
            for &after_id in &system.after {
                if let (Some(&from), Some(&to)) = (
                    self.graph_nodes.get(&after_id),
                    self.graph_nodes.get(&system.id),
                ) {
                    // Check the edge does not already exist
                    if !self.has_edge_between(from, to) {
                        self.dependency_graph
                            .add_edge(from, to, ConflictKind::Explicit);
                        self.edge_set.insert((from, to));
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: after_id,
                            to_id: system.id,
                            kind: ConflictKind::Explicit,
                        });
                    }
                }
            }

            // Runs before
            for &before_id in &system.before {
                if let (Some(&from), Some(&to)) = (
                    self.graph_nodes.get(&system.id),
                    self.graph_nodes.get(&before_id),
                ) {
                    if !self.has_edge_between(from, to) {
                        self.dependency_graph
                            .add_edge(from, to, ConflictKind::Explicit);
                        self.edge_set.insert((from, to));
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: system.id,
                            to_id: before_id,
                            kind: ConflictKind::Explicit,
                        });
                    }
                }
            }
        }

        // ── 3. Sequential barriers ──
        // Use one dummy barrier node instead of O(N×M) edges:
        //   all parallel → barrier → all sequential
        // Result: N+M edges instead of N×M.
        const BARRIER_ID: u32 = u32::MAX;
        let barrier_sys_id = SystemId(BARRIER_ID);

        if !self.seq_system_indices.is_empty() && !self.par_system_indices.is_empty() {
            // Remove the old barrier node, if any
            if let Some(old_barrier) = self.graph_nodes.remove(&barrier_sys_id) {
                self.dependency_graph.remove_node(old_barrier);
                self.edge_set
                    .retain(|&(a, b)| a != old_barrier && b != old_barrier);
                self.edge_info
                    .retain(|e| e.from_id != barrier_sys_id && e.to_id != barrier_sys_id);
            }
            // Add a new barrier node
            let barrier_node = self.dependency_graph.add_node(barrier_sys_id);
            self.graph_nodes.insert(barrier_sys_id, barrier_node);

            // All parallel → barrier
            for &par_idx in &self.par_system_indices {
                let par_id = self.systems[par_idx].id;
                if let Some(&par_node) = self.graph_nodes.get(&par_id) {
                    if !self.has_edge_between(par_node, barrier_node)
                        && (has_existing_edges
                            && !self.dependency_graph.has_path(barrier_node, par_node)
                            || !has_existing_edges)
                    {
                        self.dependency_graph.add_edge(
                            par_node,
                            barrier_node,
                            ConflictKind::SequentialBarrier,
                        );
                        self.edge_set.insert((par_node, barrier_node));
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: par_id,
                            to_id: barrier_sys_id,
                            kind: ConflictKind::SequentialBarrier,
                        });
                    }
                }
            }

            // Barrier → all sequential
            for &seq_idx in &self.seq_system_indices {
                let seq_id = self.systems[seq_idx].id;
                if let Some(&seq_node) = self.graph_nodes.get(&seq_id) {
                    if !self.has_edge_between(barrier_node, seq_node)
                        && (has_existing_edges
                            && !self.dependency_graph.has_path(seq_node, barrier_node)
                            || !has_existing_edges)
                    {
                        self.dependency_graph.add_edge(
                            barrier_node,
                            seq_node,
                            ConflictKind::SequentialBarrier,
                        );
                        self.edge_set.insert((barrier_node, seq_node));
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: barrier_sys_id,
                            to_id: seq_id,
                            kind: ConflictKind::SequentialBarrier,
                        });
                    }
                }
            }
        }

        // ── 4. Write/Read conflicts for new/changed systems ─
        for &idx in &systems_to_process {
            let system_i = &self.systems[idx];
            let ai = match system_i.kind.access() {
                Some(a) => a,
                None => continue,
            };

            // Check conflicts against all other systems
            // For Write+Write conflicts, add an edge only if idx < j
            // to avoid duplication
            for j in 0..n {
                if j == idx {
                    continue;
                }

                let system_j = &self.systems[j];
                let aj = match system_j.kind.access() {
                    Some(a) => a,
                    None => continue,
                };

                if let Some((conflict_kind, direction)) = detect_conflict_kind(
                    ai,
                    aj,
                    system_i.id,
                    system_j.id,
                    &self.type_names,
                    self.event_ordering_enabled,
                ) {
                    let is_symmetric = matches!(conflict_kind, ConflictKind::WriteWrite { .. })
                        || matches!(conflict_kind, ConflictKind::EventWriteWrite { .. })
                        || matches!(conflict_kind, ConflictKind::SharedEventReaders { .. })
                        || matches!(conflict_kind, ConflictKind::BidirectionalWriteRead { .. });
                    let is_bidirectional =
                        matches!(conflict_kind, ConflictKind::BidirectionalWriteRead { .. });

                    // For BidirectionalWriteRead we add edges in both directions
                    // to create a real cycle that will be detected as CircularDependency.
                    // If the user has declared an explicit ordering (via add_dependency,
                    // .before(), or .after()), we respect it: edges that contradict the
                    // explicit direction are suppressed, preventing a false cycle.
                    if is_bidirectional {
                        if idx > j {
                            continue; // process only once per pair
                        }
                        let explicit_fwd = self
                            .explicit_orderings
                            .contains(&(system_i.id, system_j.id));
                        let explicit_rev = self
                            .explicit_orderings
                            .contains(&(system_j.id, system_i.id));

                        // Add A→B edge — unless explicitly reversed
                        if let (Some(&from_a), Some(&to_a)) = (
                            self.graph_nodes.get(&system_i.id),
                            self.graph_nodes.get(&system_j.id),
                        ) {
                            if !self.has_edge_between(from_a, to_a) && !explicit_rev {
                                self.dependency_graph
                                    .add_edge(from_a, to_a, conflict_kind.clone());
                                self.edge_set.insert((from_a, to_a));
                                self.edge_info.push(GraphEdgeInfo {
                                    from_id: system_i.id,
                                    to_id: system_j.id,
                                    kind: conflict_kind.clone(),
                                });
                            }
                        }
                        // Add B→A edge — unless explicitly reversed
                        if let (Some(&from_b), Some(&to_b)) = (
                            self.graph_nodes.get(&system_j.id),
                            self.graph_nodes.get(&system_i.id),
                        ) {
                            if !self.has_edge_between(from_b, to_b) && !explicit_fwd {
                                self.dependency_graph
                                    .add_edge(from_b, to_b, conflict_kind.clone());
                                self.edge_set.insert((from_b, to_b));
                                self.edge_info.push(GraphEdgeInfo {
                                    from_id: system_j.id,
                                    to_id: system_i.id,
                                    kind: conflict_kind,
                                });
                            }
                        }
                        continue;
                    }

                    // direction = true means i→j, direction = false means j→i
                    let (from_idx, to_idx, from_id, to_id) = if direction {
                        (idx, j, system_i.id, system_j.id)
                    } else {
                        if j > idx {
                            // process only once per pair for WriteRead
                            continue;
                        }
                        (j, idx, system_j.id, system_i.id)
                    };

                    if is_symmetric && from_idx > to_idx {
                        continue;
                    }

                    // D5: for a symmetric conflict (WriteWrite / EventWriteWrite),
                    // an explicit ordering in EITHER direction already serializes
                    // the two systems, satisfying the conflict. Adding a
                    // registration-oriented edge on top could contradict it and
                    // fabricate a false CircularDependency (e.g. `before("b","a")`
                    // while `a` registered first). Respect the explicit ordering —
                    // its edge is already in the graph — and skip the conflict edge.
                    if is_symmetric
                        && (self.explicit_orderings.contains(&(from_id, to_id))
                            || self.explicit_orderings.contains(&(to_id, from_id)))
                    {
                        continue;
                    }

                    if let (Some(&from), Some(&to)) =
                        (self.graph_nodes.get(&from_id), self.graph_nodes.get(&to_id))
                    {
                        let need_cycle_check = !is_symmetric;
                        if !self.has_edge_between(from, to)
                            && (!need_cycle_check || !self.dependency_graph.has_path(to, from))
                        {
                            self.dependency_graph
                                .add_edge(from, to, conflict_kind.clone());
                            self.edge_set.insert((from, to));
                            self.edge_info.push(GraphEdgeInfo {
                                from_id,
                                to_id,
                                kind: conflict_kind,
                            });
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Try to find a cycle description for the error message.
    fn find_cycle_description(&self) -> String {
        // Simple search: find pairs of systems with mutual dependencies
        let mut pairs = Vec::new();
        for edge in &self.edge_info {
            let reverse = self
                .edge_info
                .iter()
                .any(|e| e.from_id == edge.to_id && e.to_id == edge.from_id);
            if reverse {
                let from_name = self
                    .systems
                    .iter()
                    .find(|s| s.id == edge.from_id)
                    .map(|s| s.name.as_str())
                    .unwrap_or("?");
                let to_name = self
                    .systems
                    .iter()
                    .find(|s| s.id == edge.to_id)
                    .map(|s| s.name.as_str())
                    .unwrap_or("?");
                pairs.push(format!("{} <-> {}", from_name, to_name));
            }
        }
        if pairs.is_empty() {
            "check add_dependency() calls for circular references".to_string()
        } else {
            pairs.dedup();
            let mut msg = pairs.join(", ");
            msg.push_str(
                "\n  Hint: resolve with scheduler.chain(&[\"a\", \"b\"]), \
                 scheduler.before(\"a\", \"b\"), or scheduler.after(\"b\", \"a\")",
            );
            msg
        }
    }

}
