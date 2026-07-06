use super::*;

impl Scheduler {
    // ── Inspection ─────────────────────────────────────────────

    pub fn system_count(&self) -> usize {
        self.systems.len()
    }

    pub fn stages(&self) -> Option<&[Stage]> {
        self.execution_plan.as_ref().map(|p| p.stages.as_slice())
    }

    /// Compact execution plan.
    pub fn debug_plan(&self) -> String {
        let Some(plan) = &self.execution_plan else {
            return "(not compiled — call compile() first)".to_string();
        };
        let mut out = String::new();
        for (i, stage) in plan.stages.iter().enumerate() {
            let mode = if stage.is_parallelizable() {
                "PARALLEL"
            } else if stage.all_parallel {
                "parallel/single"
            } else {
                "sequential"
            };
            out.push_str(&format!("Stage {} [{}] ({}) :\n", i, mode, stage.label));
            for sys_id in &stage.system_ids {
                if let Some(s) = self
                    .system_indices
                    .get(sys_id)
                    .and_then(|&idx| self.systems.get(idx))
                {
                    let kind_str = match &s.kind {
                        SystemKind::Parallel { access, .. } => {
                            format!("par | R:{} W:{}", access.reads.len(), access.writes.len())
                        }
                        SystemKind::Sequential(_) => "seq | full &mut World".to_string(),
                    };
                    out.push_str(&format!("  - {} [{}]\n", s.name, kind_str));
                }
            }
        }
        out
    }

    /// Detailed plan with the reasons stages were split.
    ///
    /// Shows which component conflict caused systems to end up in
    /// different stages. Useful when debugging the schedule and
    /// tuning parallelism.
    ///
    /// # Example output
    /// ```text
    /// Stage 0 [PARALLEL]:
    ///   - physics    [par | R:1 W:2]  (reads: Mass; writes: Velocity, Position)
    ///   - health     [par | R:0 W:1]  (writes: Health)
    ///
    /// Stage 1 [sequential]:
    ///   - commands   [seq | full &mut World]
    ///
    /// ── Conflict edges ─────────────────────────────────────
    ///   physics  →  ai_system       Write+Write conflict on `Position`
    ///   physics  →  commands        sequential barrier (&mut World)
    ///   health   →  commands        sequential barrier (&mut World)
    /// ```
    pub fn debug_plan_verbose(&self) -> String {
        let Some(plan) = &self.execution_plan else {
            return "(not compiled — call compile() first)".to_string();
        };

        let mut out = String::new();

        // ── Stages ────────────────────────────────────────────
        for (i, stage) in plan.stages.iter().enumerate() {
            let mode = if stage.is_parallelizable() {
                "PARALLEL"
            } else if stage.all_parallel {
                "parallel/single"
            } else {
                "sequential"
            };
            out.push_str(&format!("Stage {} [{}] ({}):\n", i, mode, stage.label));
            for sys_id in &stage.system_ids {
                if let Some(s) = self
                    .system_indices
                    .get(sys_id)
                    .and_then(|&idx| self.systems.get(idx))
                {
                    match &s.kind {
                        SystemKind::Parallel { access, .. } => {
                            let reads: Vec<_> = access
                                .reads
                                .iter()
                                .map(|tid| component_type_name(*tid, &self.type_names))
                                .collect();
                            let writes: Vec<_> = access
                                .writes
                                .iter()
                                .map(|tid| component_type_name(*tid, &self.type_names))
                                .collect();
                            out.push_str(&format!(
                                "  - {} [par | R:{} W:{}]\n",
                                s.name,
                                access.reads.len(),
                                access.writes.len(),
                            ));
                            if !reads.is_empty() {
                                out.push_str(&format!("      reads:  {}\n", reads.join(", ")));
                            }
                            if !writes.is_empty() {
                                out.push_str(&format!("      writes: {}\n", writes.join(", ")));
                            }
                        }
                        SystemKind::Sequential(_) => {
                            out.push_str(&format!("  - {} [seq | full &mut World]\n", s.name));
                        }
                    }
                    // Show conditions
                    let cond_str = format_condition(&s.run_condition);
                    if !cond_str.is_empty() {
                        out.push_str(&format!("      run_if: {}\n", cond_str));
                    }
                }
            }
        }

        // ── SubWorld mapping (archetypes per system) ──────────
        if !self.system_archetype_indices.is_empty() {
            out.push_str("\n  ── SubWorld archetype mapping ──\n");
            for sys_id in plan.flat_order.iter() {
                if let Some(s) = self
                    .system_indices
                    .get(sys_id)
                    .and_then(|&idx| self.systems.get(idx))
                {
                    match self.system_archetype_indices.get(sys_id) {
                        Some(SystemArchetypes::All) => {
                            out.push_str(&format!("  {}: all archetypes\n", s.name));
                        }
                        Some(SystemArchetypes::Filtered(indices)) => {
                            out.push_str(&format!(
                                "  {}: {} archetypes [{}]\n",
                                s.name,
                                indices.len(),
                                if indices.len() <= 10 {
                                    indices
                                        .iter()
                                        .map(|i| i.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                } else {
                                    format!("{}..{}", indices[0], indices[indices.len() - 1])
                                }
                            ));
                        }
                        None => {}
                    }
                }
            }
        }

        // ── Conflict edges ────────────────────────────────────
        if !self.edge_info.is_empty() {
            out.push_str(
                "\n── Conflict edges ──────────────────────────────────────────────────\n",
            );
            for edge in &self.edge_info {
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
                out.push_str(&format!(
                    "  {:<20} →  {:<20}  {}\n",
                    from_name, to_name, edge.kind
                ));
            }
        }

        // ── Parallelism summary ───────────────────────────────
        let par_stages = plan.stages.iter().filter(|s| s.is_parallelizable()).count();
        let seq_stages = plan.stages.iter().filter(|s| !s.all_parallel).count();
        let max_par = plan
            .stages
            .iter()
            .map(|s| s.system_count())
            .max()
            .unwrap_or(0);
        out.push_str(&format!(
            "\n── Summary: {} stages ({} parallel, {} sequential), max parallelism: {} systems\n",
            plan.stages.len(),
            par_stages,
            seq_stages,
            max_par
        ));

        out
    }

    /// Get the conflict reasons between two specific systems.
    pub fn conflicts_between(&self, a: SystemId, b: SystemId) -> Vec<&ConflictKind> {
        self.edge_info
            .iter()
            .filter(|e| (e.from_id == a && e.to_id == b) || (e.from_id == b && e.to_id == a))
            .map(|e| &e.kind)
            .collect()
    }
}
