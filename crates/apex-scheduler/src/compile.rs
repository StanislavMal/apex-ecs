use super::*;

impl Scheduler {
    // ── Компиляция ─────────────────────────────────────────────

    /// Скомпилировать расписание.
    ///
    /// Строит/обновляет граф зависимостей, находит параллельные Stage.
    /// Если граф не изменился с прошлого compile — только пересчитывает
    /// топосорт (добавленные узлы уже в графе).
    ///
    /// Также вычисляет для каждой системы индексы архетипов, которые ей нужны
    /// (для создания SubWorld в run_hybrid_parallel).
    pub fn compile(&mut self) -> Result<(), SchedulerError> {
        // Ранний выход: если граф не менялся и план уже построен
        if !self.graph_dirty && self.execution_plan.is_some() {
            return Ok(());
        }

        // План (а значит и индексы execution-стадий) перестраивается — сбрасываем per-stage базы
        // change-detection (TD-52). Одно лишнее «всё изменилось» на следующем кадре — безопасно.
        self.stage_last_run.clear();
        // Ш2: stage indices changed — drop the cost-model history (it re-learns in a
        // couple of frames; a stale index would mis-classify a different stage).
        self.stage_cost_ema_ns.clear();
        self.stage_ran_seq.clear();

        if self.type_names.is_empty() {
            log::debug!(
                "Scheduler::compile: type_names пуст. \
                 Вызовите populate_type_names(&world.registry()) или \
                 compile_with_world(&world) для отображения имён компонентов \
                 в debug_plan_verbose()"
            );
        }

        if self.graph_dirty {
            // Резолвим config-объявленный порядок (`.before/.after/.chain`) в
            // id-рёбра ДО построения графа — теперь все имена известны
            // (forward-ссылки разрешены). Ошибка «имя не найдено» — громко (§0.2a).
            self.resolve_pending_orderings()?;
            // Инкрементальное обновление: добавляем только новые узлы и рёбра
            self.add_new_nodes_and_edges()?;
            self.graph_dirty = false;
        }

        // Топологическая сортировка всех систем → уровни параллелизма
        let levels = self.dependency_graph.parallel_levels().map_err(|_| {
            let cycle_info = self.find_cycle_description();
            SchedulerError::CircularDependency { cycle_info }
        })?;

        // Для каждого уровня топосорта разделяем system_ids по stage_label.
        // Затем объединяем результаты по label в порядке приоритета.
        use rustc_hash::FxHashMap;
        use std::collections::BTreeMap;
        let mut label_stages: BTreeMap<u8, Vec<Stage>> = BTreeMap::new();

        for level in &levels {
            let mut level_by_label: FxHashMap<StageLabel, Vec<SystemId>> = FxHashMap::default();
            for &node in level {
                if let Some(&sys_id) = self.dependency_graph.node_data(node) {
                    // O(1) lookup через system_indices вместо O(N) find()
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
                // Разбиваем на под-Stage'и по маркерам apply_deferred_after
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

        // Собираем все Stage в порядке priority или пользовательском порядке
        let mut stages: Vec<Stage> = Vec::new();

        if let Some(order) = &self.stage_order {
            // Пользовательский порядок стадий
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
            // Стадии не указанные в порядке — добавляем в конец, детерминированно
            // (D8: по label, иначе FxHashMap-порядок непредсказуем).
            let mut remaining: Vec<Stage> = stage_map.into_values().flatten().collect();
            remaining.sort_by(|a, b| a.label.cmp(&b.label));
            stages.append(&mut remaining);
        } else {
            // Стандартный порядок по priority (Startup → First → ... → Last → Custom)
            for (_prio, mut s_stages) in label_stages {
                stages.append(&mut s_stages);
            }
        }

        let flat_order: Vec<SystemId> = stages
            .iter()
            .flat_map(|s| s.system_ids.iter().copied())
            .collect();

        // Собираем event_writes для per-Stage flush
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

    /// Скомпилировать расписание, предварительно заполнив имена компонентов.
    ///
    /// Эквивалентно вызову `populate_type_names(world.registry())` затем `compile()`.
    /// После этого `debug_plan_verbose()` будет показывать реальные имена компонентов.
    ///
    /// # Пример
    ///
    /// ```ignore
    /// let mut sched = Scheduler::new();
    /// // ... добавляем системы ...
    /// sched.compile_with_world(&world).expect("schedule error");
    /// println!("{}", sched.debug_plan_verbose()); // с именами компонентов!
    /// ```
    pub fn compile_with_world(&mut self, world: &World) -> Result<(), SchedulerError> {
        self.populate_type_names(world.registry());
        self.compile()
    }

    /// Вычислить для каждой системы индексы архетипов, которые ей нужны.
    ///
    /// Вызывается после compile() перед run(), когда World уже создан.
    /// Использует AccessDescriptor.reads/writes (TypeId) для фильтрации.
    pub(crate) fn compute_archetype_indices(&mut self, world: &apex_core::World) {
        let archetypes = world.archetypes();
        let arch_count = archetypes.len();

        // Кеш: если количество архетипов не изменилось — пропускаем пересчёт
        if arch_count == self.cached_archetype_count && !self.system_archetype_indices.is_empty() {
            return;
        }

        // Архетипы append-only: при росте мира существующие списки ДОПОЛНЯЮТСЯ
        // только хвостом archetypes[prev_count..] — полный пересчёт O(systems ×
        // archetypes) был квадратичным на спавн-бёрстах (C-8). Полный скан —
        // только при первом вызове и для систем без записи (новые после compile).
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

        // Для каждой системы находим подходящие архетипы.
        // Используем критерий `any()`: архетип подходит, если содержит
        // хотя бы один компонент из системы. Это правильно для SubWorld —
        // Query потом сам отфильтрует неподходящие архетипы через matches_archetype.
        for system in &self.systems {
            let access = match system.kind.access() {
                Some(a) => a,
                None => continue, // Sequential — не использует SubWorld
            };

            // Только компонентные TypeId определяют, какие архетипы нужны системе.
            // reads_event / writes_event — виртуальные доступы для планировщика,
            // они не соответствуют реальным данным в архетипах.
            let mut system_type_ids: Vec<std::any::TypeId> = Vec::new();
            system_type_ids.extend(access.reads.iter().copied());
            system_type_ids.extend(access.writes.iter().copied());

            if system_type_ids.is_empty() {
                // Система без компонентов (только ресурсы/события) — маркер
                // «без ограничений» вместо материализованного Vec всех индексов.
                self.system_archetype_indices
                    .insert(system.id, SystemArchetypes::All);
                continue;
            }

            // ComponentId резолвим один раз на систему, не на архетип.
            // (Незарезолвленный TypeId безвреден: архетип с компонентом не может
            // существовать раньше регистрации компонента.)
            let cids: Vec<apex_core::ComponentId> = system_type_ids
                .iter()
                .filter_map(|tid| registry.get_id_by_type(tid))
                .collect();

            // Существующий список дополняем с prev_count; новый (система,
            // появившаяся после прошлого вызова) сканируем с нуля.
            let start = match self.system_archetype_indices.get(&system.id) {
                Some(SystemArchetypes::Filtered(_)) => prev_count,
                Some(SystemArchetypes::All) => continue, // состав доступа статичен
                None => {
                    self.system_archetype_indices
                        .insert(system.id, SystemArchetypes::Filtered(Vec::new()));
                    0
                }
            };
            let Some(SystemArchetypes::Filtered(indices)) =
                self.system_archetype_indices.get_mut(&system.id)
            else {
                unreachable!("ветки выше гарантируют Filtered");
            };

            for (offset, arch) in archetypes[start..].iter().enumerate() {
                if cids.iter().any(|&cid| arch.has_component(cid)) {
                    indices.push(start + offset);
                }
            }
        }

        self.cached_archetype_count = arch_count;
    }

    /// Проверяет, существует ли ребро между двумя узлами.
    fn has_edge_between(&self, from: Index, to: Index) -> bool {
        // O(1) проверка через edge_set вместо O(N) successors()
        self.edge_set.contains(&(from, to))
    }

    /// Инкрементальное добавление новых узлов и рёбер в граф.
    ///
    /// Добавляет только системы, которых ещё нет в `graph_nodes`,
    /// и рёбра для новых/изменённых систем.
    ///
    /// ## Оптимизация
    /// - При первом compile (граф пуст) — проверки `has_path()` не нужны,
    ///   т.к. циклов в пустом графе быть не может. Это убирает O(N²) BFS-ов.
    /// - `has_path()` использует переиспользуемые буферы Graph.bfs_visited/bfs_queue
    ///   вместо аллокации на каждый вызов.
    fn add_new_nodes_and_edges(&mut self) -> Result<(), SchedulerError> {
        let n = self.systems.len();

        // ── 1. Добавляем новые узлы (системы) ──────────────────
        let mut new_system_indices = Vec::new();
        for (idx, system) in self.systems.iter().enumerate() {
            if !self.graph_nodes.contains_key(&system.id) {
                let node = self.dependency_graph.add_node(system.id);
                self.graph_nodes.insert(system.id, node);
                new_system_indices.push(idx);
            }
        }

        // Если нет новых систем, но граф помечен как dirty (например, изменились зависимости)
        // нужно пересчитать рёбра для существующих систем
        let systems_to_process = if new_system_indices.is_empty() {
            // Обрабатываем все системы (зависимости могли измениться)
            (0..n).collect::<Vec<_>>()
        } else {
            // Обрабатываем только новые системы и их связи с существующими
            new_system_indices
        };

        // Оптимизация 🅱️: при первом compile() граф ещё пуст — has_path() всегда false.
        // Пропускаем O(N²) BFS-ов, т.к. циклов в пустом графе быть не может.
        let has_existing_edges = !self.edge_set.is_empty();

        // ── 2. Явные зависимости для новых/изменённых систем ──
        for &idx in &systems_to_process {
            let system = &self.systems[idx];

            // После кого выполняется
            for &after_id in &system.after {
                if let (Some(&from), Some(&to)) = (
                    self.graph_nodes.get(&after_id),
                    self.graph_nodes.get(&system.id),
                ) {
                    // Проверяем, нет ли уже такого ребра
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

            // Перед кем выполняется
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

        // ── 3. Sequential барьеры ──
        // Используем один dummy barrier-узел вместо O(N×M) рёбер:
        //   all parallel → barrier → all sequential
        // Результат: N+M рёбер вместо N×M.
        const BARRIER_ID: u32 = u32::MAX;
        let barrier_sys_id = SystemId(BARRIER_ID);

        if !self.seq_system_indices.is_empty() && !self.par_system_indices.is_empty() {
            // Удаляем старый барьерный узел, если он был
            if let Some(old_barrier) = self.graph_nodes.remove(&barrier_sys_id) {
                self.dependency_graph.remove_node(old_barrier);
                self.edge_set
                    .retain(|&(a, b)| a != old_barrier && b != old_barrier);
                self.edge_info
                    .retain(|e| e.from_id != barrier_sys_id && e.to_id != barrier_sys_id);
            }
            // Добавляем новый барьерный узел
            let barrier_node = self.dependency_graph.add_node(barrier_sys_id);
            self.graph_nodes.insert(barrier_sys_id, barrier_node);

            // Все parallel → barrier
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

            // Barrier → все sequential
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

        // ── 4. Write/Read конфликты для новых/изменённых систем ─
        for &idx in &systems_to_process {
            let system_i = &self.systems[idx];
            let ai = match system_i.kind.access() {
                Some(a) => a,
                None => continue,
            };

            // Проверяем конфликты со всеми другими системами
            // Для Write+Write конфликтов добавляем ребро только если idx < j
            // чтобы избежать дублирования
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

                    // direction = true означает i→j, direction = false означает j→i
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

    /// Попытка найти описание цикла для сообщения об ошибке.
    fn find_cycle_description(&self) -> String {
        // Простой поиск: находим пары систем с взаимными зависимостями
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
