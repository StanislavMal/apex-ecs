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
        }
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
            let used = size - reserver.block_remaining().unwrap_or(0);
            self.system_spawn_history.insert(sys_id, used);
            world.reclaim_entity_block_tail(&reserver.unused_block_ids());
        }
    }

    // ── Регистрация ────────────────────────────────────────────
    //
    // ЕДИНСТВЕННЫЙ публичный вход — `add_systems(label, …)` (+ конструкторы
    // `sys`/`seq`/`par`/`par_access` и bare-идентификаторы). Методы ниже —
    // внутренние строительные блоки и опора внутренних тестов; публичный
    // зоопарк из 15 add_*-вариантов удалён ревизией API 2026-06-12.

    /// Регистрировать Sequential систему (полный &mut World).
    /// Этап — `default_stage_label` (по умолчанию `Update`).
    #[cfg(test)]
    pub(crate) fn add_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        self.add_system_to_stage(name, func, self.default_stage_label.clone())
    }

    /// Регистрировать Sequential систему в указанном этапе.
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

    /// Регистрировать Sequential систему в Startup этапе (запускается один раз).
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

    /// Регистрировать AutoSystem.
    /// Этап — `default_stage_label` (по умолчанию `Update`).
    #[cfg(test)]
    pub(crate) fn add_auto_system<S>(&mut self, name: impl Into<String>, system: S) -> SystemBuilder<'_>
    where
        S: AutoSystem + 'static,
    {
        self.add_auto_system_to_stage(name, system, self.default_stage_label.clone())
    }

    /// Регистрировать AutoSystem в указанном этапе.
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
        // W3-4: система с состоянием не делится ASD row-split'ом — несколько
        // задач звали бы run(&mut self) одного экземпляра конкурентно.
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

    /// Регистрировать AutoSystem в Startup этапе.
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

    /// Регистрировать ParSystem.
    /// Этап — `default_stage_label` (по умолчанию `Update`).
    #[allow(dead_code)]
    pub(crate) fn add_par_system<S: ParSystem + 'static>(
        &mut self,
        name: impl Into<String>,
        system: S,
    ) -> SystemId {
        self.add_par_system_to_stage(name, system, self.default_stage_label.clone())
    }

    /// Регистрировать ParSystem в указанном этапе.
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
        // W3-4: система с состоянием → без ASD row-split.
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

    /// Регистрировать ParSystem в Startup этапе.
    #[allow(dead_code)]
    pub(crate) fn add_startup_par_system<S: ParSystem + 'static>(
        &mut self,
        name: impl Into<String>,
        system: S,
    ) -> SystemId {
        self.add_par_system_to_stage(name, system, StageLabel::Startup)
    }


    /// Регистрировать параллельную систему-замыкание с явным доступом.
    ///
    /// `access` описывает, какие компоненты/ресурсы/события система читает
    /// и пишет — планировщик использует это для разрешения конфликтов.
    ///
    /// Этап — `default_stage_label` (по умолчанию `Update`).
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

    /// Регистрировать параллельную систему-замыкание с явным доступом
    /// в указанном этапе.
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
        // W3-4: замыкание с захватами = состояние → без ASD row-split.
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

    /// Эвристика: минимальное количество entity на одну систему для окупаемости
    /// параллелизма (rayon overhead < выигрыш).
    ///
    /// Пороги определены эмпирически через `parallel_diagnostics` scaling benchmark
    /// (12 ядер, release). «Valley of death» (PAR медленнее SEQ в 2-3x) находится
    /// в диапазоне 5,000-10,000 entity для multi-system и до 50,000 для single-system.
    pub(crate) fn min_entities_for_parallelism(num_systems: usize) -> usize {
        if num_systems >= 3 {
            15_000 // 3+ систем — амортизация rayon overhead на 25K+ entity
        } else if num_systems >= 2 {
            25_000 // 2 системы — пересечение PAR/SEQ около 25K
        } else {
            80_000 // 1 система — только row-level chunking, пересечение ~100K
        }
    }

    /// Зарегистрировать системы в указанном этапе через замыкание.
    ///
    /// Внутри замыкания `default_stage_label` временно подменяется на `label`,
    /// поэтому все `add_*_system` (без `_to_stage`) внутри попадают в этот этап.
    ///
    /// После замыкания предыдущий этап восстанавливается.
    ///
    /// Удобно для группировки систем плагина:
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
        // Скоуп-условие действует только ВНУТРИ блока (раньше «прилипало» ко
        // всем последующим регистрациям — латентный баг, найден аудитом
        // 2026-06-12).
        self.scope_condition = saved_condition;
        self
    }

    /// Скоуп условий: все системы, зарегистрированные внутри замыкания —
    /// включая через [`add_systems`](Self::add_systems) — получают условия,
    /// заданные [`run_condition`](Self::run_condition) (AND с их собственными).
    /// Скоупы вкладываются (условия комбинируются по AND); по выходе из блока
    /// прежний скоуп восстанавливается.
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
    ///     // обе системы наследуют условие паузы
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

    /// Установить scope condition — все системы, зарегистрированные внутри
    /// текущего [`scoped`](Self::scoped)-блока, автоматически получат это
    /// условие (AND с их собственными condition'ами). Повторные вызовы внутри
    /// блока комбинируются по AND.
    ///
    /// # Пример
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel, seq};
    /// # let mut sched = Scheduler::new();
    /// sched.scoped(|s| {
    ///     s.run_condition(|w| !w.has_resource::<bool>());
    ///     s.add_systems(StageLabel::Update, (
    ///         seq("movement", |_w: &mut apex_core::world::World| {}),
    ///         seq("ai", |_w: &mut apex_core::world::World| {}),
    ///     ));
    ///     // обе системы наследуют условие паузы
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

    /// Добавить явную зависимость: `system` выполняется после `after_id`.
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

    /// `a` выполняется до `b`. Эквивалентно `add_dependency(b, a)`.
    ///
    /// Явный порядок имеет приоритет над автоматически обнаруженными конфликтами
    /// чтения-записи: если планировщик видит `BidirectionalWriteRead` между `a` и `b`,
    /// ребро, противоречащее явному порядку, будет подавлено (цикла не будет).
    ///
    /// Системы указываются по именам, переданным в `add_auto_system` / `add_system`.
    pub fn before(&mut self, a_name: &str, b_name: &str) -> Result<(), SchedulerError> {
        let a_id = self.find_id_by_name(a_name)?;
        let b_id = self.find_id_by_name(b_name)?;
        self.add_dependency(b_id, a_id);
        Ok(())
    }

    /// `a` выполняется после `b`. Эквивалентно `add_dependency(a, b)`.
    ///
    /// Явный порядок имеет приоритет над автоматически обнаруженными конфликтами
    /// чтения-записи: если планировщик видит `BidirectionalWriteRead` между `a` и `b`,
    /// ребро, противоречащее явному порядку, будет подавлено (цикла не будет).
    ///
    /// Системы указываются по именам, переданным в `add_auto_system` / `add_system`.
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

    /// Цепочка систем: каждая запускается после предыдущей.
    ///
    /// `sched.chain(&["grav", "phys"])` — эквивалент `sched.before("grav", "phys")`.
    ///
    /// Для N имён создаёт N-1 зависимостей:
    /// `names[0] → names[1] → ... → names[N-1]`.
    pub fn chain(&mut self, names: &[&str]) -> Result<(), SchedulerError> {
        for w in names.windows(2) {
            self.before(w[0], w[1])?;
        }
        Ok(())
    }

    /// Объявить системы взаимно **порядко-независимыми**: при `BidirectionalWriteRead` между ними
    /// планировщик НЕ роняет компиляцию (`CircularDependency`), а сериализует их в
    /// **детерминированном порядке регистрации** (порядок воспроизводим между сборками).
    ///
    /// Когда применять: две системы имеют перекрёстный конфликт чтения-записи (A пишет T, читаемый
    /// B; B пишет U, читаемый A), но порядок их выполнения для логики **не важен** — и не хочется
    /// выдумывать искусственный `before`/`after`. Конфликт по данным всё равно исключает
    /// параллельный запуск (гонок нет) — `independent` лишь снимает требование *явно выбрать*
    /// направление, фиксируя его детерминированно (порядок добавления систем).
    ///
    /// Отличие от Bevy `ambiguous_with`: тот допускает ПРОИЗВОЛЬНЫЙ порядок (недетерминизм); мы
    /// сохраняем детерминизм (важно для replay/netcode — см. руководство §6.3).
    ///
    /// Для N имён покрывает все пары. Системы указываются по именам (как в `before`/`after`/`chain`).
    pub fn independent(&mut self, names: &[&str]) -> Result<(), SchedulerError> {
        let mut ids = Vec::with_capacity(names.len());
        for n in names {
            ids.push(self.find_id_by_name(n)?);
        }
        // Для каждой пары — ребро в порядке РЕГИСТРАЦИИ (позиция в `self.systems`); собираем пары
        // заранее, чтобы не держать иммутабельный борроу `self.systems` во время `add_dependency`.
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
            // `first` → `second`: explicit ordering подавляет встречное ребро BidirectionalWriteRead.
            self.add_dependency(second, first);
        }
        Ok(())
    }

    /// Установить пользовательский порядок StageLabel для compile().
    ///
    /// По умолчанию стадии упорядочиваются по приоритету:
    /// `Startup(0) → First(1) → PreUpdate(2) → Update(3) → PostUpdate(4) → Last(5) → Custom(6)`.
    ///
    /// Если нужно изменить порядок (например, `First` после `Update`), используй этот метод:
    ///
    /// ```ignore
    /// # use apex_scheduler::stage::StageLabel::*;
    /// let mut scheduler = Scheduler::new();
    /// scheduler.configure_stages(vec![Startup, Update, First, PreUpdate, PostUpdate, Last]);
    /// ```
    ///
    /// Стадии, не указанные в `order`, добавляются в конец в порядке возрастания приоритета.
    pub fn configure_stages(&mut self, order: Vec<StageLabel>) {
        self.stage_order = Some(order);
        self.invalidate_plan();
    }

    // ── Run Conditions ─────────────────────────────────────────

    /// Прикрепить run condition к системе по имени (AND-композиция).
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

    /// Вставить точку синхронизации после последней зарегистрированной системы.
    ///
    /// При `compile()` Stage будет разбит на под-Stage, и между ними будут
    /// применены все накопленные `Commands` и сброшены события.
    ///
    /// Типичное использование — между `add_systems`-вызовами:
    /// ```
    /// # use apex_scheduler::{Scheduler, StageLabel, seq};
    /// # let mut sched = Scheduler::new();
    /// let combat = StageLabel::tag("combat");
    /// sched.add_systems(combat.clone(), seq("spawner", |_w: &mut apex_core::world::World| {}));
    /// sched.apply_deferred(); // ← команды spawner'а применены до следующих систем
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

    // ── add_systems — кортежный API (профессиональный) ──────────

    /// Зарегистрировать несколько систем одновременно через кортежный API.
    ///
    /// Это рекомендованный способ регистрации. Системы конфигурируются
    /// с условиями ДО регистрации, что исключает double-borrow conflict.
    ///
    /// # Пример
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
    /// # Порядок (золотой путь)
    ///
    /// Зависимости объявляются прямо на конфигах через `.before()`/`.after()`
    /// (по имени системы) и `.chain()` (позиционно, по порядку в кортеже). Имена
    /// резолвятся при `compile()`, поэтому forward-ссылки допустимы:
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
    /// Для ДИНАМИЧЕСКОГО порядка (по строке-имени, из редактора/скриптов)
    /// остаётся `Scheduler::before`/`after`/`chain`.
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

    /// Внутренняя регистрация одной SystemConfig.
    fn register_system_config(&mut self, cfg: SystemConfig, stage_label: StageLabel) -> SystemId {
        if stage_label == StageLabel::Startup && self.startup_completed {
            log::warn!(
                "add_systems(Startup, …): `{}` добавлена после завершения Startup — не выполнится",
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
        // Scope-условия (scoped/run_condition) применяются и к add_systems-пути
        // (раньше — только к внутренним методам регистрации: документированный
        // паттерн «scoped + add_systems» молча терял условие — латентный баг,
        // найден аудитом 2026-06-12).
        self.merge_scope_condition(id);
        self.invalidate_plan();
        id
    }

    /// Получить `&mut SystemDescriptor` по `SystemId` (O(1)).
    pub(crate) fn system_by_id_mut(&mut self, id: SystemId) -> Option<&mut SystemDescriptor> {
        self.system_indices
            .get(&id)
            .and_then(|&idx| self.systems.get_mut(idx))
    }

    fn invalidate_plan(&mut self) {
        // ВНИМАНИЕ: НЕ сбрасываем stage_order — он должен сохраняться
        // между перекомпиляциями (см. тест configure_stages_persists_across_compiles).
        self.execution_plan = None;
        self.graph_dirty = true;
    }

    /// Управлять автоматическим упорядочиванием по событиям.
    ///
    /// При `true` (по умолчанию): все системы с `Emit<E>` гарантированно
    /// выполняются до систем с `Listen<E>` в пределах одного кадра.
    ///
    /// При `false`: порядок Emit/Listen не определён планировщиком.
    /// Используйте только для совместимости со старым кодом или если
    /// порядок не важен и вы хотите максимизировать параллелизм.
    pub fn enable_event_ordering(&mut self, enabled: bool) -> &mut Self {
        self.event_ordering_enabled = enabled;
        self.invalidate_plan();
        self
    }

    /// Создать конвейер событий для типа E.
    ///
    /// # Пример
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

    /// Получить AccessDescriptor системы по её SystemId.
    ///
    /// Возвращает `None` если система не найдена или это sequential система.
    pub(crate) fn system_access(&self, id: SystemId) -> Option<&AccessDescriptor> {
        self.system_indices
            .get(&id)
            .and_then(|&idx| self.systems.get(idx)?.kind.access())
    }

    /// Заполнить `type_names` из ComponentRegistry World'а.
    ///
    /// После вызова этой функции `component_type_name()` будет возвращать
    /// реальные имена компонентов вместо `"<component>"`.
    ///
    /// Вызывается автоматически в `run()` / `run_sequential()`.
    /// Можно вызвать вручную перед `compile()`, если нужны реальные имена
    /// в `debug_plan_verbose()`.
    pub(crate) fn populate_type_names(&mut self, registry: &ComponentRegistry) {
        if !self.type_names.is_empty() {
            return; // Уже заполнены — все миры имеют одинаковые компоненты
        }
        for info in registry.iter() {
            self.type_names.insert(info.type_id, info.name);
        }
    }

}
