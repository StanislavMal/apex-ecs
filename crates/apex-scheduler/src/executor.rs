use super::*;

impl Scheduler {
    // ── Выполнение ─────────────────────────────────────────────

    /// Запустить одну итерацию планировщика.
    /// Использует Rayon для параллельного выполнения.
    ///
    /// Startup этап выполняется только при первом вызове `run()`.
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

    /// Ш2: cost-model verdict — should this parallel-eligible stage run sequentially?
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

    /// Ш2: fold a measured stage dispatch time into the per-stage EMA and record the
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
        // Заполняем реестр имён компонентов для диагностики конфликтов
        self.populate_type_names(world.registry());
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        // Вычисляем маппинг систем → архетипы для SubWorld.
        self.compute_archetype_indices(world);

        self.run_hybrid_parallel(world as *mut World);

        // Граница кадра: продвигаем change-tick, чтобы `Changed<T>` в системах
        // на следующем кадре детектировал только мутации этого кадра (TD-9).
        world.advance_change_tick();

        // После первого run() Startup больше не выполняется
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
        let world_ptr = world as *mut World;
        let w = unsafe { &mut *world_ptr };
        self.populate_type_names(w.registry());
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        let plan = self.execution_plan.as_ref().unwrap();

        for system in &self.systems {
            if let Some(access) = system.kind.access() {
                for &(type_id, cap) in &access.event_reserves {
                    w.event_reserve_by_type(type_id, cap);
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

        // (клонируем метаданные стадий — plan заимствует self, а stage_steps
        // и системы ниже требуют &mut)
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
                let steps = Self::stage_steps(w, &StageLabel::FixedUpdate);
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
                    if self.systems[idx].run_condition.evaluate(w) {
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
            // before the loop and went stale).
            let all_indices: Vec<usize> = (0..w.archetypes().len()).collect();
            let sub_world = unsafe { apex_core::SubWorld::from_raw(w, &all_indices) };
            {
                // TD-52: per-execution-stage change-detection window (cross-stage `Changed<T>`).
                self.open_stage_change_window(stage_idx, w);
                for &sys_id in system_ids {
                    if !enabled.contains(&sys_id) {
                        continue;
                    }
                    if let Some(&index) = self.system_indices.get(&sys_id) {
                        let system = &mut self.systems[index];
                        let prof_t = main_prof_enabled().then(std::time::Instant::now);
                        match &mut system.kind {
                            SystemKind::Sequential(f) => f(w),
                            SystemKind::Parallel { system, .. } => {
                                // SAFETY: `cmds_ptr` points at `stage_cmds`, a single
                                // buffer alive until the end of the run; systems run
                                // sequentially on this thread, so no aliasing.
                                let ctx = unsafe {
                                    SystemContext::with_commands(
                                        std::slice::from_ref(&sub_world),
                                        cmds_ptr,
                                    )
                                };
                                system.run(ctx);
                            }
                        }
                        if let Some(t) = prof_t {
                            main_prof_record(&self.systems[index].name, t.elapsed());
                        }
                    }
                }

                // Применяем отложенные команды после каждой стадии/шага (rank order —
                // systems appended to `stage_cmds` in execution order).
                stage_cmds.apply(w);

                // Per-stage flush — делает события доступными для следующего этапа
                if !emit_event_types.is_empty() {
                    w.flush_events_by_type(emit_event_types);
                }
            }
        }

        // Граница кадра: продвигаем change-tick (TD-9).
        w.advance_change_tick();

        self.startup_completed = true;
    }

    /// Запустить stage через ASD (Adaptive Scope Distribution).
    ///
    /// ASD динамически разбивает entity всех систем stage на чанки
    /// адаптивного размера, сортирует их по archetype_id для cache locality
    /// и распределяет по всем воркерам Rayon.
    ///
    /// # Алгоритм
    ///
    /// 1. Вычисляется `total_entity_count` — сумма entity всех систем в stage.
    /// 2. `target_chunk = max(total_entity_count / num_workers / 2, MIN_CHUNK)`.
    /// 3. Для каждой системы:
    ///    - Если entity_count <= target_chunk → 1 задача (весь архетип целиком).
    ///    - Если entity_count > target_chunk → разбивка на чанки размером ~target_chunk.
    /// 4. Все чанки собираются в Vec, сортируются по archetype_id.
    /// 5. Чанки запускаются через `rayon::scope`.
    ///
    /// # Адаптивность
    ///
    /// - Мало entity → target_chunk мал → каждая система = 1 задача (per-system scope).
    /// - Много entity → target_chunk велик → задачи равномерно заполняют всех воркеров.
    /// - Без if/else — один механизм на все сценарии.
    ///
    /// # Безопасность
    ///
    /// Каждая spawn-задача работает с РАЗНЫМ `SystemDescriptor`
    /// (разные `AsdTask.ptr`), поэтому одновременный `&mut` доступ
    /// к разным системам безопасен. SubWorld создаётся локально внутри spawn
    /// с ограничением `(arch_idx, start, end)`, что гарантирует что
    /// разные задачи НЕ пересекаются по данным одного архетипа.
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
        //    Ш1: buffers pooled in Scheduler scratch fields (detached via mem::take
        //    so `&mut self` method calls below don't alias them; restored at the end
        //    to retain capacity — zero steady-state allocation).
        let mut skipped_systems = std::mem::take(&mut self.scratch_skipped);
        skipped_systems.clear();
        for &sys_id in stage_ids {
            if !enabled.contains(&sys_id) {
                skipped_systems.insert(sys_id);
            }
        }

        // 1. Собираем per-system информацию (SysInfo — module-level, pooled).
        let mut sys_infos = std::mem::take(&mut self.scratch_sys_infos);
        sys_infos.clear();
        let mut total_entity_count: usize = 0;

        for &sys_id in stage_ids {
            if skipped_systems.contains(&sys_id) {
                continue;
            }
            if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
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
                // Ш1: entity_count without materializing an arch-index Vec — the
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
                    // Нет access (динамическая система) — состояние неизвестно,
                    // консервативно считаем stateful (без row-split).
                    let stateful = access.map(|a| a.stateful).unwrap_or(true);
                    // Мутация ресурса / Commands (TD-37): сайд-эффект, не локальный для партиции
                    // запроса ⇒ row-split дублировал бы его. Нет access ⇒ консервативно true.
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
                        SystemKind::Sequential(_) => continue,
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
                    // Система без entity (только ресурсы/события) — запускаем сразу.
                    let slot = slot_of[&sys_id];
                    let system = &mut self.systems[sys_idx];
                    if let SystemKind::Parallel { system: sys, .. } = &mut system.kind {
                        let all_indices: Vec<usize> = (0..archetypes.len()).collect();
                        let sw = unsafe { apex_core::SubWorld::from_raw(world, &all_indices) };
                        let sws = [sw];
                        // SAFETY: this system's private per-system slot (D8b), run
                        // single-task on the main thread here → unique `&mut`.
                        let ctx = unsafe {
                            SystemContext::with_commands(
                                &sws,
                                (cmds_base as *mut Commands).add(slot),
                            )
                        };
                        sys.run(ctx);
                    }
                }
            }
        }

        if sys_infos.is_empty() {
            self.scratch_skipped = skipped_systems;
            self.scratch_sys_infos = sys_infos;
            return;
        }

        // 2. Вычисляем target chunk size через adaptive_chunk_size
        //    из apex-core (используется в Query::par_for_each и CachedQuery::par_for_each).
        let per_system_entity = total_entity_count / sys_infos.len().max(1);
        let target_chunk = apex_core::world::adaptive_chunk_size(
            per_system_entity,
            num_workers,
            world.chunk_config(),
        );

        // Выравниваем размер чанка до 8 entity, чтобы избежать false sharing
        // кэш-линий между соседними чанками одного архетипа (8 × sizeof(Position) = 96 байт > 64).
        const CACHE_ALIGN_ENTITIES: usize = 8;
        let aligned_chunk = (target_chunk / CACHE_ALIGN_ENTITIES) * CACHE_ALIGN_ENTITIES;
        let effective_chunk = aligned_chunk.max(CACHE_ALIGN_ENTITIES);

        // 3. Создаём чанки для всех систем (Ш1: pooled task buffer)
        let mut tasks = std::mem::take(&mut self.scratch_tasks);
        tasks.clear();

        for info in &sys_infos {
            // Ш1: fetch the arch-index slice from the cached map (no per-system clone).
            let arch_slice: &[usize] = match self.system_archetype_indices.get(&info.sys_id) {
                Some(SystemArchetypes::Filtered(v)) => v.as_slice(),
                _ => &[],
            };
            // Per-system scope (БЕЗ row-split — система выполняется ровно один раз) для:
            //   a) Систем с малым entity_count
            //   b) Систем с событиями (Emit/Listen)
            //   c) Систем с par_for_each — избегаем oversubscribe rayon
            //   d) Систем с needs_whole_world — глобальный доступ
            //   e) Систем с состоянием (W3-4) — row-split звал бы
            //      run(&mut self) одного экземпляра конкурентно
            //   f) Систем с мутацией ресурса / Commands (TD-37) — тело plain-fn выполняется раз
            //      на чанк, поэтому не-query-локальный сайд-эффект (ResMut-запись, Commands)
            //      умножился бы на число чанков (+ гонка при параллельных чанках). Декомпозиция
            //      допустима ТОЛЬКО когда эффекты системы ограничены её per-entity партицией
            //      запроса (записи компонентов по непересекающимся диапазонам строк).
            if info.has_events
                || info.uses_par_for_each
                || info.needs_whole_world
                || info.stateful
                || info.non_query_side_effects
                || info.entity_count <= effective_chunk
            {
                // Per-system scope: одна задача, все entity целиком. D8b: single-task
                // → private per-system command slot (unique writer).
                let slot = slot_of[&info.sys_id];
                tasks.push(AsdTask {
                    ptr: info.ptr,
                    arch_indices: SmallVec::from_slice(arch_slice),
                    chunk_ranges: SmallVec::new(), // пусто = весь SubWorld
                    cmds: Some(SendPtr((cmds_base as *mut Commands).wrapping_add(slot))),
                });
            } else {
                // Multi-архетипная крупная система — ASD разбивка
                let mut remaining = info.entity_count;
                let mut arch_iter = arch_slice.iter().copied();
                let mut current_arch = arch_iter.next();
                let mut arch_offset: usize = 0;

                while remaining > 0 {
                    let mut chunk_ranges: SmallVec<[(usize, usize, usize); 4]> = SmallVec::new();
                    // Множество archetype_id в этом чанке — для сужения arch_indices
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
                        // Добавляем archetype_id в set если его там нет
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
                        });
                    }
                }
            }
        }

        // 4. Сортируем чанки по archetype_id для cache locality
        tasks.sort_unstable_by_key(|t| t.chunk_ranges.first().map(|&(a, _, _)| a).unwrap_or(0));

        // 5. Запускаем через rayon::scope. `task.cmds` (Option<SendPtr<Commands>>)
        //    is Copy+Send — each single-task system carries the pointer to its own
        //    per-system slot; row-split tasks carry `None`.
        rayon::scope(|s| {
            for task in &tasks {
                s.spawn(move |_| {
                    // SAFETY (W3-4 + D3): несколько задач одной системы существуют
                    // ТОЛЬКО для систем без состояния (`stateful`/`has_events`/
                    // `uses_par_for_each`/`needs_whole_world` получают единый
                    // per-system scope). Для них `run(&mut self)` не читает и
                    // не пишет байтов self (ZST/без захватов). `task.ptr` целит
                    // ПРЯМО в `dyn ParSystem` (ZST-цель), поэтому конкурентные
                    // `&mut *task.ptr.0` не алиасят реальных байт — в отличие от
                    // прежнего `&mut SystemDescriptor` (владеет `String` name и
                    // т.п.). Диапазоны строк задач дизъюнктны по построению.
                    unsafe {
                        let sys: &mut dyn ParSystem = &mut *task.ptr.0;
                        let sub = if task.chunk_ranges.is_empty() {
                            // Полный SubWorld — все архетипы системы без ограничений
                            apex_core::SubWorld::from_raw(world, &task.arch_indices)
                        } else {
                            // SubWorld с range-ограничениями и суженными arch_indices
                            apex_core::SubWorld::from_raw_with_ranges(
                                world,
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
                        let ctx = match task.cmds {
                            Some(slot) => SystemContext::with_commands(sws, slot.0),
                            None => SystemContext::new(sws),
                        };
                        sys.run(ctx);
                    }
                });
            }
        });

        // Ш1: return pooled buffers to the Scheduler to retain capacity next frame.
        // `sys_infos` holds raw `SendPtr`s into `self.systems`; they are cleared and
        // rebuilt each frame before any use, never dereferenced while stale.
        self.scratch_skipped = skipped_systems;
        self.scratch_sys_infos = sys_infos;
        self.scratch_tasks = tasks;
    }

    /// Параллельное выполнение через ASD (Adaptive Scope Distribution).
    ///
    /// Для stage с исключительно параллельными системами используется
    /// [`run_stage_parallel`] (ASD). Stage с sequential системами или
    /// смешанные выполняются последовательно, каждая система получает
    /// полный `SubWorld` над всеми архетипами.
    ///
    fn run_hybrid_parallel(&mut self, world_ptr: *mut World) {
        // Ш1: move the plan OUT of `self` (mem::take) so its stages can be borrowed
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

        // Ш1: pooled arch-length snapshot (cleared + refilled, capacity retained).
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
                        let size = self.block_size_for(sys_id);
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
                    let w = unsafe { &*const_ptr };
                    let all_indices: Vec<usize> = (0..w.archetypes().len()).collect();
                    let sub_world = unsafe { apex_core::SubWorld::from_raw(w, &all_indices) };
                    for &sys_id in stage_ids {
                        if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                            if !enabled.contains(&sys_id) {
                                continue;
                            }
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
                continue;
            }

            // Ş2: SEQ/PAR decision. Priority:
            //   1. explicit user floor (`parallel_min_entities`) always wins;
            //   2. else, once the cost-model has measured this stage (EMA>0), it
            //      FULLY decides — measured work supersedes entity count. This is the
            //      point of the model: it parallelizes heavy-low-entity stages the
            //      entity heuristic would serialize, and serializes light-high-entity
            //      stages it would parallelize (Д1/Д2);
            //   3. else (cold start, no history yet) fall back to the entity heuristic.
            let stage_entity_count: usize = if stage_par_min > 0
                || auto_disable_stage
            {
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
            let below_hard_limit = stage_par_min > 0
                && stage_entity_count < stage_par_min;
            let has_cost_history =
                self.stage_cost_ema_ns.get(stage_idx).copied().unwrap_or(0.0) > 0.0;
            let should_fallback = if below_hard_limit {
                true
            } else if has_cost_history {
                self.cost_model_prefers_seq(stage_idx)
            } else if auto_disable_stage {
                let sys_count = stage_ids.len();
                let per_system = stage_entity_count / sys_count.max(1);
                per_system < Self::min_entities_for_parallelism(sys_count)
            } else {
                false
            };
            let stage_t0 = std::time::Instant::now();

            if should_fallback {
                {
                    let w = unsafe { &*const_ptr };
                    let all_indices: Vec<usize> = (0..w.archetypes().len()).collect();
                    let sub_world = unsafe { apex_core::SubWorld::from_raw(w, &all_indices) };
                    for &sys_id in stage_ids {
                        if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                            if !enabled.contains(&sys_id) {
                                continue;
                            }
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
            // Ш2: fold the measured stage time into the per-stage cost EMA (drives
            // the SEQ/PAR decision next frame). `should_fallback` = the mode we ran.
            self.record_stage_cost(stage_idx, stage_t0.elapsed(), should_fallback);
        }

        // Ш1: return the plan and pooled buffers to `self` (capacity retained).
        self.execution_plan = Some(plan);
        self.scratch_arch_lengths = arch_lengths;
        self.scratch_schedule = schedule;
    }

    /// Число исполнений стадии в этом кадре (D2-5): FixedUpdate — по
    /// аккумулятору [`FixedTime`] (без ресурса — 1, обычная стадия);
    /// остальные стадии — всегда 1.
    fn stage_steps(world: &mut World, label: &StageLabel) -> usize {
        if *label != StageLabel::FixedUpdate {
            return 1;
        }
        world
            .try_resource_mut::<crate::fixed::FixedTime>()
            .map(|ft| ft.drain_steps())
            .unwrap_or(1)
    }

}
