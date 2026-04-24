//! apex-scheduler — гибридный планировщик систем.
//!
//! # Улучшения по сравнению с предыдущей версией
//!
//! ## 1. `add_auto_system` — регистрация AutoSystem без ручного access
//!
//! ```ignore
//! struct MovementSystem;
//! impl AutoSystem for MovementSystem {
//!     type Query = (Read<Velocity>, Write<Position>);
//!     fn run(&mut self, ctx: SystemContext<'_>) {
//!         ctx.for_each_component::<Self::Query, _>(|(vel, pos)| {
//!             pos.x += vel.x;
//!         });
//!     }
//! }
//! sched.add_auto_system("movement", MovementSystem);
//! // AccessDescriptor выводится статически — нельзя забыть компонент
//! ```
//!
//! ## 2. `ConflictKind` в рёбрах графа — verbose диагностика
//!
//! `debug_plan_verbose()` показывает ПОЧЕМУ системы в разных Stage:
//! ```text
//! Stage 0 [PARALLEL]:
//!   - physics    [par | R:1 W:2]
//!   - health     [par | R:0 W:1]
//!   Conflict: physics -> ai  WriteWrite(Position)
//!
//! Stage 1 [sequential]:
//!   - commands   [seq | full &mut World]
//! ```
//!
//! ## 3. Инкрементальный граф — добавление систем без полного пересчёта
//!
//! При `add_*_system` граф не пересчитывается сразу.
//! Топосорт выполняется лениво при первом `run()` или явном `compile()`.
//! При добавлении новой системы добавляются только новые узлы/рёбра.
//!
//! ## 4. `par_for_each_component` в SystemContext (в apex-core/world.rs)
//!
//! Параллелизм внутри одной системы по архетипам через Rayon.
//!
//! # Типы систем
//!
//! | Тип | Access | Использование |
//! |-----|--------|---------------|
//! | AutoSystem | автовывод из Query | рекомендуется |
//! | ParSystem | явный AccessDescriptor | сложные системы |
//! | FnParSystem | явный + замыкание | быстрые прототипы |
//! | Sequential | полный &mut World | structural changes |

pub mod stage;

use rustc_hash::FxHashMap;
use thiserror::Error;
use apex_graph::Graph;
use thunderdome::Index;
use apex_core::{
    AccessDescriptor,
    world::{World, ParallelWorld, SystemContext},
    system_param::{AutoSystem, WorldQuerySystemAccess},
};

pub use stage::{Stage, StageLabel};
pub use apex_core::AccessDescriptor as Access;

// ── ConflictKind ───────────────────────────────────────────────

/// Причина зависимости между системами в графе.
///
/// Хранится в рёбрах `dependency_graph` для verbose диагностики.
/// Позволяет `debug_plan_verbose()` объяснять ПОЧЕМУ системы
/// оказались в разных Stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Явная зависимость через `add_dependency()`
    Explicit,
    /// Оба пишут в один компонент — Write+Write конфликт
    WriteWrite {
        component_name: &'static str,
    },
    /// Один пишет, другой читает — Write+Read конфликт
    WriteRead {
        component_name: &'static str,
        writer_id: u32,
        reader_id: u32,
    },
    /// Sequential барьер — система с полным &mut World
    SequentialBarrier,
    /// Два EventWriter одного типа событий
    EventWriteWrite {
        event_name: &'static str,
    },
    /// EventWriter и EventReader одного типа событий
    EventWriteRead {
        event_name: &'static str,
        writer_id: u32,
        reader_id: u32,
    },
}

impl std::fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConflictKind::Explicit =>
                write!(f, "explicit dependency"),
            ConflictKind::WriteWrite { component_name } =>
                write!(f, "Write+Write conflict on `{}`", component_name),
            ConflictKind::WriteRead { component_name, .. } =>
                write!(f, "Write+Read conflict on `{}`", component_name),
            ConflictKind::SequentialBarrier =>
                write!(f, "sequential barrier (&mut World)"),
            ConflictKind::EventWriteWrite { event_name } =>
                write!(f, "Event Write+Write conflict on `{}`", event_name),
            ConflictKind::EventWriteRead { event_name, .. } =>
                write!(f, "Event Write+Read conflict on `{}`", event_name),
        }
    }
}

// ── SchedulerError ─────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("Circular dependency between systems: {cycle_info}")]
    CircularDependency { cycle_info: String },
    #[error("System '{0}' not found")]
    SystemNotFound(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SystemId(pub u32);

pub type SystemFn = Box<dyn FnMut(&mut World) + Send>;

// ── SendPtr ────────────────────────────────────────────────────

#[derive(Copy, Clone)]
struct SendPtr<T>(*mut T);

// SAFETY: использование строго ограничено run_hybrid_parallel где
// уникальность ptr гарантирована — каждый ptr из уникального индекса.
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

impl<T> SendPtr<T> {
    #[inline]
    unsafe fn as_mut(&self) -> &mut T { &mut *self.0 }
}

// ── ParSystem trait ────────────────────────────────────────────

/// Параллельная система с явным AccessDescriptor.
///
/// Используй `AutoSystem` если access полностью покрывается одним Query.
pub trait ParSystem: Send + Sync {
    fn access() -> AccessDescriptor where Self: Sized;
    fn run(&mut self, ctx: SystemContext<'_>);
    fn name() -> &'static str where Self: Sized { std::any::type_name::<Self>() }
}

// ── Адаптер AutoSystem → ParSystem ────────────────────────────

/// Обёртка которая позволяет регистрировать AutoSystem как ParSystem.
///
/// Access берётся из `S::Query::system_access()` — статически,
/// без возможности ошибиться.
struct AutoSystemAdapter<S: AutoSystem> {
    inner: S,
}

impl<S: AutoSystem + 'static> ParSystem for AutoSystemAdapter<S> {
    fn access() -> AccessDescriptor where Self: Sized {
        // Ключевой момент: access выводится из типа Query, не из ручного кода
        S::Query::system_access()
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        self.inner.run(ctx);
    }

    fn name() -> &'static str where Self: Sized {
        S::name()
    }
}

// ── FnParSystem ────────────────────────────────────────────────

struct FnParSystem {
    func:   Box<dyn FnMut(SystemContext<'_>) + Send + Sync>,
    access: AccessDescriptor,
}

impl ParSystem for FnParSystem {
    fn access() -> AccessDescriptor where Self: Sized { AccessDescriptor::new() }
    fn run(&mut self, ctx: SystemContext<'_>) { (self.func)(ctx); }
}

// ── SystemKind ─────────────────────────────────────────────────

enum SystemKind {
    Sequential(SystemFn),
    Parallel {
        system: Box<dyn ParSystem>,
        access: AccessDescriptor,
    },
}

impl SystemKind {
    fn is_parallel(&self) -> bool { matches!(self, SystemKind::Parallel { .. }) }

    fn access(&self) -> Option<&AccessDescriptor> {
        match self {
            SystemKind::Parallel { access, .. } => Some(access),
            SystemKind::Sequential(_)           => None,
        }
    }
}

// ── SystemDescriptor ───────────────────────────────────────────

struct SystemDescriptor {
    id:     SystemId,
    name:   String,
    kind:   SystemKind,
    after:  Vec<SystemId>,
    before: Vec<SystemId>,
    /// Этап выполнения (по умолчанию Update).
    stage_label: StageLabel,
}

// ── SystemBuilder ──────────────────────────────────────────────

pub struct SystemBuilder<'a> {
    scheduler: &'a mut Scheduler,
    id:        SystemId,
}

impl<'a> SystemBuilder<'a> {
    pub fn id(self) -> SystemId { self.id }
}

// ── ExecutionPlan ──────────────────────────────────────────────

struct ExecutionPlan {
    stages:     Vec<Stage>,
    flat_order: Vec<SystemId>,
}

// ── GraphEdgeInfo ──────────────────────────────────────────────

/// Метаданные ребра в dependency_graph для verbose диагностики.
#[derive(Clone, Debug)]
struct GraphEdgeInfo {
    from_id: SystemId,
    to_id:   SystemId,
    kind:    ConflictKind,
}

// ── Scheduler ─────────────────────────────────────────────────

/// Гибридный планировщик с граф-ориентированным компилятором.
///
/// # Жизненный цикл
///
/// ```text
/// add_*_system()    →  systems Vec обновлён, план инвалидирован
/// compile()         →  граф пересчитан, план готов
/// run()             →  compile() лениво если нужно, затем выполнение
/// ```
///
/// # Инкрементальность
///
/// Граф зависимостей хранится между `compile()` вызовами.
/// `dirty_systems` отслеживает системы добавленные после последнего compile —
/// при следующем compile добавляются только новые узлы/рёбра.
pub struct Scheduler {
    systems:         Vec<SystemDescriptor>,
    /// Быстрый поиск системы по SystemId: O(1) вместо O(n)
    system_indices:  FxHashMap<SystemId, usize>,
    next_id:         u32,
    execution_plan:  Option<ExecutionPlan>,

    // ── Конфигурация параллелизма ───────────────────────────────
    /// Минимальное количество систем в Stage для параллельного выполнения
    /// Если систем меньше этого значения — выполняется последовательно
    parallel_threshold: usize,

    // ── Инкрементальный граф ────────────────────────────────────
    /// Граф зависимостей: узлы = SystemId, рёбра = ConflictKind.
    /// Хранится между compile() для инкрементального обновления.
    dependency_graph: Graph<SystemId, ConflictKind>,
    /// Map SystemId → Index в dependency_graph (для быстрого lookup).
    graph_nodes:      FxHashMap<SystemId, Index>,
    /// Рёбра с полными метаданными — для verbose диагностики.
    edge_info:        Vec<GraphEdgeInfo>,
    /// True если после последнего compile() добавлялись системы/зависимости.
    graph_dirty:      bool,

    // ── SubWorld маппинг ────────────────────────────────────────
    /// Для каждой системы — индексы архетипов, которые ей нужны.
    /// Заполняется в compile() и используется в run_hybrid_parallel().
    system_archetype_indices: FxHashMap<SystemId, Vec<usize>>,
    /// Owned storage для Vec<usize>, используемых SubWorld.
    /// Позволяет избежать Box::leak — данные живут в Scheduler.
    archetype_indices_storage: Vec<Vec<usize>>,
    /// Количество архетипов в World на момент последнего compute_archetype_indices().
    /// Используется для кеширования — пересчёт только при изменении.
    cached_archetype_count: usize,

    /// Флаг: был ли уже выполнен Startup этап.
    startup_completed: bool,

    /// Пользовательский порядок StageLabel для compile().
    /// Если Some — compile() использует этот порядок вместо hardcoded standard_order().
    /// Если None — используется StageLabel::standard_order().
    stage_order: Option<Vec<StageLabel>>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            systems:          Vec::new(),
            system_indices:   FxHashMap::default(),
            next_id:          0,
            execution_plan:   None,
            parallel_threshold: 2, // Минимум 2 системы для параллельного выполнения
            dependency_graph: Graph::new(),
            graph_nodes:      FxHashMap::default(),
            edge_info:        Vec::new(),
            graph_dirty:      false,
            system_archetype_indices: FxHashMap::default(),
            archetype_indices_storage: Vec::new(),
            cached_archetype_count: 0,
            startup_completed: false,
            stage_order:      None,
        }
    }

    // ── Регистрация ────────────────────────────────────────────

    /// Регистрировать Sequential систему (полный &mut World) в указанном этапе.
    pub fn add_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        self.add_system_to_stage(name, func, StageLabel::Update)
    }

    /// Регистрировать Sequential систему в указанном этапе.
    pub fn add_system_to_stage<F>(
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
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Sequential(Box::new(func)),
            after:  Vec::new(),
            before: Vec::new(),
            stage_label,
        });
        self.system_indices.insert(id, index);
        self.invalidate_plan();
        SystemBuilder { scheduler: self, id }
    }

    /// Регистрировать Sequential систему в Startup этапе (запускается один раз).
    pub fn add_startup_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        self.add_system_to_stage(name, func, StageLabel::Startup)
    }

    /// Регистрировать AutoSystem в этапе Update (по умолчанию).
    pub fn add_auto_system<S>(&mut self, name: impl Into<String>, system: S) -> SystemId
    where
        S: AutoSystem + 'static,
    {
        self.add_auto_system_to_stage(name, system, StageLabel::Update)
    }

    /// Регистрировать AutoSystem в указанном этапе.
    pub fn add_auto_system_to_stage<S>(
        &mut self,
        name: impl Into<String>,
        system: S,
        stage_label: StageLabel,
    ) -> SystemId
    where
        S: AutoSystem + 'static,
    {
        let id     = SystemId(self.next_id);
        self.next_id += 1;
        let access = S::Query::system_access();
        let adapter = AutoSystemAdapter { inner: system };
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel { system: Box::new(adapter), access },
            after:  Vec::new(),
            before: Vec::new(),
            stage_label,
        });
        self.system_indices.insert(id, index);
        self.invalidate_plan();
        id
    }

    /// Регистрировать AutoSystem в Startup этапе.
    pub fn add_startup_auto_system<S>(&mut self, name: impl Into<String>, system: S) -> SystemId
    where
        S: AutoSystem + 'static,
    {
        self.add_auto_system_to_stage(name, system, StageLabel::Startup)
    }

    /// Регистрировать ParSystem в этапе Update (по умолчанию).
    pub fn add_par_system<S: ParSystem + 'static>(
        &mut self,
        name:   impl Into<String>,
        system: S,
    ) -> SystemId {
        self.add_par_system_to_stage(name, system, StageLabel::Update)
    }

    /// Регистрировать ParSystem в указанном этапе.
    pub fn add_par_system_to_stage<S: ParSystem + 'static>(
        &mut self,
        name:   impl Into<String>,
        system: S,
        stage_label: StageLabel,
    ) -> SystemId {
        let id     = SystemId(self.next_id);
        self.next_id += 1;
        let access = S::access();
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel { system: Box::new(system), access },
            after:  Vec::new(),
            before: Vec::new(),
            stage_label,
        });
        self.system_indices.insert(id, index);
        self.invalidate_plan();
        id
    }

    /// Регистрировать ParSystem в Startup этапе.
    pub fn add_startup_par_system<S: ParSystem + 'static>(
        &mut self,
        name:   impl Into<String>,
        system: S,
    ) -> SystemId {
        self.add_par_system_to_stage(name, system, StageLabel::Startup)
    }

    /// Регистрировать FnParSystem в этапе Update (по умолчанию).
    pub fn add_fn_par_system<F>(
        &mut self,
        name:   impl Into<String>,
        func:   F,
        access: AccessDescriptor,
    ) -> SystemId
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        self.add_fn_par_system_to_stage(name, func, access, StageLabel::Update)
    }

    /// Регистрировать FnParSystem в указанном этапе.
    pub fn add_fn_par_system_to_stage<F>(
        &mut self,
        name:   impl Into<String>,
        func:   F,
        access: AccessDescriptor,
        stage_label: StageLabel,
    ) -> SystemId
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let id       = SystemId(self.next_id);
        self.next_id += 1;
        let system   = FnParSystem { func: Box::new(func), access: access.clone() };
        let index = self.systems.len();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel { system: Box::new(system), access },
            after:  Vec::new(),
            before: Vec::new(),
            stage_label,
        });
        self.system_indices.insert(id, index);
        self.invalidate_plan();
        id
    }

    /// Регистрировать FnParSystem в Startup этапе.
    pub fn add_startup_fn_par_system<F>(
        &mut self,
        name:   impl Into<String>,
        func:   F,
        access: AccessDescriptor,
    ) -> SystemId
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        self.add_fn_par_system_to_stage(name, func, access, StageLabel::Startup)
    }

    /// Добавить явную зависимость: `system` выполняется после `after_id`.
    pub fn add_dependency(&mut self, system: SystemId, after_id: SystemId) {
        if let Some(s) = self.systems.iter_mut().find(|s| s.id == system) {
            if !s.after.contains(&after_id) { s.after.push(after_id); }
            self.invalidate_plan();
        }
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

    fn invalidate_plan(&mut self) {
        self.execution_plan = None;
        self.graph_dirty    = true;
    }

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
        if self.graph_dirty {
            // Инкрементальное обновление: добавляем только новые узлы и рёбра
            self.add_new_nodes_and_edges()?;
            self.graph_dirty = false;
        }

        // Топологическая сортировка всех систем → уровни параллелизма
        let levels = self.dependency_graph
            .parallel_levels()
            .map_err(|_| {
                let cycle_info = self.find_cycle_description();
                SchedulerError::CircularDependency { cycle_info }
            })?;

        // Для каждого уровня топосорта разделяем system_ids по stage_label.
        // Затем объединяем результаты по label в порядке приоритета.
        use std::collections::BTreeMap;
        use rustc_hash::FxHashMap;
        let mut label_stages: BTreeMap<u8, Vec<Stage>> = BTreeMap::new();

        for level in &levels {
            let mut level_by_label: FxHashMap<StageLabel, Vec<SystemId>> = FxHashMap::default();
            for &node in level {
                if let Some(&sys_id) = self.dependency_graph.node_data(node) {
                    if let Some(system) = self.systems.iter().find(|s| s.id == sys_id) {
                        level_by_label
                            .entry(system.stage_label.clone())
                            .or_default()
                            .push(sys_id);
                    }
                }
            }
            for (label, ids) in level_by_label {
                let all_parallel = ids.iter().all(|sid| {
                    self.systems.iter()
                        .find(|s| s.id == *sid)
                        .map(|s| s.kind.is_parallel())
                        .unwrap_or(false)
                });
                let prio = label.priority();
                label_stages
                    .entry(prio)
                    .or_default()
                    .push(Stage::new(label, ids, all_parallel));
            }
        }

        // Собираем все Stage в порядке priority или пользовательском порядке
        let mut stages: Vec<Stage> = Vec::new();

        if let Some(order) = &self.stage_order {
            // Пользовательский порядок стадий
            let mut stage_map: FxHashMap<StageLabel, Vec<Stage>> = FxHashMap::default();
            for (_prio, mut s_stages) in label_stages {
                for stage in s_stages.drain(..) {
                    stage_map.entry(stage.label.clone()).or_default().push(stage);
                }
            }
            for label in order {
                if let Some(mut s_stages) = stage_map.remove(label) {
                    stages.append(&mut s_stages);
                }
            }
            // Стадии не указанные в порядке — добавляем в конец
            let mut remaining: Vec<Stage> = stage_map.into_values().flat_map(|v| v).collect();
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

        self.execution_plan = Some(ExecutionPlan { stages, flat_order });
        Ok(())
    }

    /// Вычислить для каждой системы индексы архетипов, которые ей нужны.
    ///
    /// Вызывается после compile() перед run(), когда World уже создан.
    /// Использует AccessDescriptor.reads/writes (TypeId) для фильтрации.
    pub fn compute_archetype_indices(&mut self, world: &apex_core::World) {
        let archetypes = world.archetypes();
        let arch_count = archetypes.len();

        // Кеш: если количество архетипов не изменилось — пропускаем пересчёт
        if arch_count == self.cached_archetype_count && !self.system_archetype_indices.is_empty() {
            return;
        }

        self.system_archetype_indices.clear();
        self.archetype_indices_storage.clear();

        if arch_count == 0 {
            self.cached_archetype_count = 0;
            return;
        }

        // Для каждой системы находим подходящие архетипы
        for system in &self.systems {
            let access = match system.kind.access() {
                Some(a) => a,
                None => continue, // Sequential — не использует SubWorld
            };

            // Собираем все TypeId, которые система читает или пишет
            let mut system_type_ids: Vec<std::any::TypeId> = Vec::new();
            system_type_ids.extend(access.reads.iter().copied());
            system_type_ids.extend(access.writes.iter().copied());

            if system_type_ids.is_empty() {
                // Система без компонентов (только ресурсы/события) — все архетипы
                let all: Vec<usize> = (0..arch_count).collect();
                self.system_archetype_indices.insert(system.id, all);
                continue;
            }

            let mut indices = Vec::new();
            for (arch_idx, arch) in archetypes.iter().enumerate() {
                // Проверяем, есть ли у архетипа хотя бы один компонент из списка системы
                let registry = world.registry();
                let has_match = system_type_ids.iter().any(|tid| {
                    if let Some(cid) = registry.get_id_by_type(tid) {
                        arch.has_component(cid)
                    } else {
                        false
                    }
                });
                if has_match {
                    indices.push(arch_idx);
                }
            }

            self.system_archetype_indices.insert(system.id, indices);
        }

        self.cached_archetype_count = arch_count;
    }

    /// Полная перестройка графа зависимостей.
    ///
    /// Вызывается при `graph_dirty = true`. Добавляет все системы как узлы,
    /// затем все рёбра (явные + sequential барьеры + Write/Read конфликты).
    fn rebuild_graph(&mut self) -> Result<(), SchedulerError> {
        // Очищаем граф и метаданные рёбер
        self.dependency_graph = Graph::new();
        self.graph_nodes.clear();
        self.edge_info.clear();

        // Добавляем все системы как узлы
        for system in &self.systems {
            let node = self.dependency_graph.add_node(system.id);
            self.graph_nodes.insert(system.id, node);
        }

        let n = self.systems.len();

        // ── 1. Явные зависимости ───────────────────────────────
        for system in &self.systems {
            for &after_id in &system.after {
                if let (Some(&from), Some(&to)) =
                    (self.graph_nodes.get(&after_id), self.graph_nodes.get(&system.id))
                {
                    self.dependency_graph.add_edge(from, to, ConflictKind::Explicit);
                    self.edge_info.push(GraphEdgeInfo {
                        from_id: after_id,
                        to_id:   system.id,
                        kind:    ConflictKind::Explicit,
                    });
                }
            }
            for &before_id in &system.before {
                if let (Some(&from), Some(&to)) =
                    (self.graph_nodes.get(&system.id), self.graph_nodes.get(&before_id))
                {
                    self.dependency_graph.add_edge(from, to, ConflictKind::Explicit);
                    self.edge_info.push(GraphEdgeInfo {
                        from_id: system.id,
                        to_id:   before_id,
                        kind:    ConflictKind::Explicit,
                    });
                }
            }
        }

        // ── 2. Sequential барьеры ──────────────────────────────
        for i in 0..n {
            if !self.systems[i].kind.is_parallel() {
                // Все предыдущие системы → sequential
                for j in 0..i {
                    if let (Some(&from), Some(&to)) =
                        (self.graph_nodes.get(&self.systems[j].id),
                         self.graph_nodes.get(&self.systems[i].id))
                    {
                        self.dependency_graph.add_edge(from, to, ConflictKind::SequentialBarrier);
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: self.systems[j].id,
                            to_id:   self.systems[i].id,
                            kind:    ConflictKind::SequentialBarrier,
                        });
                    }
                }
                // Sequential → все последующие
                for j in (i + 1)..n {
                    if let (Some(&from), Some(&to)) =
                        (self.graph_nodes.get(&self.systems[i].id),
                         self.graph_nodes.get(&self.systems[j].id))
                    {
                        self.dependency_graph.add_edge(from, to, ConflictKind::SequentialBarrier);
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: self.systems[i].id,
                            to_id:   self.systems[j].id,
                            kind:    ConflictKind::SequentialBarrier,
                        });
                    }
                }
            }
        }

        // ── 3. Write/Read конфликты ────────────────────────────
        // Определяем конфликт и его причину (первый конфликтующий компонент)
        for i in 0..n {
            for j in (i + 1)..n {
                let ai = match self.systems[i].kind.access() { Some(a) => a, None => continue };
                let aj = match self.systems[j].kind.access() { Some(a) => a, None => continue };

                if let Some((conflict_kind, direction)) = detect_conflict_kind(
                    ai, aj,
                    self.systems[i].id,
                    self.systems[j].id,
                ) {
                    // direction = true означает i→j
                    if direction {
                        if let (Some(&from), Some(&to)) =
                            (self.graph_nodes.get(&self.systems[i].id),
                             self.graph_nodes.get(&self.systems[j].id))
                        {
                            self.dependency_graph.add_edge(from, to, conflict_kind.clone());
                            self.edge_info.push(GraphEdgeInfo {
                                from_id: self.systems[i].id,
                                to_id:   self.systems[j].id,
                                kind:    conflict_kind,
                            });
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Проверяет, существует ли ребро между двумя узлами.
    fn has_edge_between(&self, from: Index, to: Index) -> bool {
        // Проверяем все исходящие рёбра из from
        self.dependency_graph.successors(from).any(|succ| succ == to)
    }

    /// Инкрементальное добавление новых узлов и рёбер в граф.
    ///
    /// Добавляет только системы, которых ещё нет в `graph_nodes`,
    /// и рёбра для новых/изменённых систем.
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

        // ── 2. Явные зависимости для новых/изменённых систем ──
        for &idx in &systems_to_process {
            let system = &self.systems[idx];
            
            // После кого выполняется
            for &after_id in &system.after {
                if let (Some(&from), Some(&to)) =
                    (self.graph_nodes.get(&after_id), self.graph_nodes.get(&system.id))
                {
                    // Проверяем, нет ли уже такого ребра
                    if !self.has_edge_between(from, to) {
                        self.dependency_graph.add_edge(from, to, ConflictKind::Explicit);
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: after_id,
                            to_id:   system.id,
                            kind:    ConflictKind::Explicit,
                        });
                    }
                }
            }
            
            // Перед кем выполняется
            for &before_id in &system.before {
                if let (Some(&from), Some(&to)) =
                    (self.graph_nodes.get(&system.id), self.graph_nodes.get(&before_id))
                {
                    if !self.has_edge_between(from, to) {
                        self.dependency_graph.add_edge(from, to, ConflictKind::Explicit);
                        self.edge_info.push(GraphEdgeInfo {
                            from_id: system.id,
                            to_id:   before_id,
                            kind:    ConflictKind::Explicit,
                        });
                    }
                }
            }
        }

        // ── 3. Sequential барьеры для новых/изменённых систем ─
        for &idx in &systems_to_process {
            let system = &self.systems[idx];
            
            if !system.kind.is_parallel() {
                // Sequential система: sequential → par и par → sequential
                for j in 0..n {
                    if j == idx { continue; }
                    if self.systems[j].kind.is_parallel() {
                        // par → sequential (если par раньше)
                        if j < idx {
                            if let (Some(&from), Some(&to)) =
                                (self.graph_nodes.get(&self.systems[j].id),
                                 self.graph_nodes.get(&system.id))
                            {
                                if !self.has_edge_between(from, to)
                                    && !self.dependency_graph.has_path(to, from)
                                {
                                    self.dependency_graph.add_edge(from, to, ConflictKind::SequentialBarrier);
                                    self.edge_info.push(GraphEdgeInfo {
                                        from_id: self.systems[j].id,
                                        to_id:   system.id,
                                        kind:    ConflictKind::SequentialBarrier,
                                    });
                                }
                            }
                        }
                        // sequential → par (если par позже)
                        if j > idx {
                            if let (Some(&from), Some(&to)) =
                                (self.graph_nodes.get(&system.id),
                                 self.graph_nodes.get(&self.systems[j].id))
                            {
                                if !self.has_edge_between(from, to)
                                    && !self.dependency_graph.has_path(to, from)
                                {
                                    self.dependency_graph.add_edge(from, to, ConflictKind::SequentialBarrier);
                                    self.edge_info.push(GraphEdgeInfo {
                                        from_id: system.id,
                                        to_id:   self.systems[j].id,
                                        kind:    ConflictKind::SequentialBarrier,
                                    });
                                }
                            }
                        }
                    }
                    // Sequential ↔ Sequential: НЕ добавляем барьер,
                    // чтобы не конфликтовать с explicit dependencies.
                    // Внутри Stage они выполняются последовательно по порядку system_ids.
                }
            } else {
                // Параллельная система: проверяем sequential барьеры от sequential систем
                for j in 0..n {
                    if j == idx { continue; }
                    
                    if !self.systems[j].kind.is_parallel() {
                        // sequential → par или par → sequential
                        if j < idx {
                            // sequential → par
                            if let (Some(&from), Some(&to)) =
                                (self.graph_nodes.get(&self.systems[j].id),
                                 self.graph_nodes.get(&system.id))
                            {
                                if !self.has_edge_between(from, to)
                                    && !self.dependency_graph.has_path(to, from)
                                {
                                    self.dependency_graph.add_edge(from, to, ConflictKind::SequentialBarrier);
                                    self.edge_info.push(GraphEdgeInfo {
                                        from_id: self.systems[j].id,
                                        to_id:   system.id,
                                        kind:    ConflictKind::SequentialBarrier,
                                    });
                                }
                            }
                        } else {
                            // par → sequential
                            if let (Some(&from), Some(&to)) =
                                (self.graph_nodes.get(&system.id),
                                 self.graph_nodes.get(&self.systems[j].id))
                            {
                                if !self.has_edge_between(from, to)
                                    && !self.dependency_graph.has_path(to, from)
                                {
                                    self.dependency_graph.add_edge(from, to, ConflictKind::SequentialBarrier);
                                    self.edge_info.push(GraphEdgeInfo {
                                        from_id: system.id,
                                        to_id:   self.systems[j].id,
                                        kind:    ConflictKind::SequentialBarrier,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── 4. Write/Read конфликты для новых/изменённых систем ─
        for &idx in &systems_to_process {
            let system_i = &self.systems[idx];
            let ai = match system_i.kind.access() { Some(a) => a, None => continue };
            
            // Проверяем конфликты со всеми другими системами
            // Для Write+Write конфликтов добавляем ребро только если idx < j
            // чтобы избежать дублирования
            for j in 0..n {
                if j == idx { continue; }
                
                let system_j = &self.systems[j];
                let aj = match system_j.kind.access() { Some(a) => a, None => continue };
                
                if let Some((conflict_kind, direction)) = detect_conflict_kind(
                    ai, aj,
                    system_i.id,
                    system_j.id,
                ) {
                    // direction = true означает i→j
                    if direction {
                        // Для Write+Write и EventWriteWrite конфликтов добавляем ребро
                        // только если idx < j чтобы избежать дублирования
                        let is_symmetric = matches!(conflict_kind, ConflictKind::WriteWrite { .. })
                            || matches!(conflict_kind, ConflictKind::EventWriteWrite { .. });
                        if is_symmetric && idx > j {
                            continue;
                        }
                        
                        if let (Some(&from), Some(&to)) =
                            (self.graph_nodes.get(&system_i.id),
                             self.graph_nodes.get(&system_j.id))
                        {
                            if !self.has_edge_between(from, to)
                                && !self.dependency_graph.has_path(to, from)
                            {
                                self.dependency_graph.add_edge(from, to, conflict_kind.clone());
                                self.edge_info.push(GraphEdgeInfo {
                                    from_id: system_i.id,
                                    to_id:   system_j.id,
                                    kind:    conflict_kind,
                                });
                            }
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
            let reverse = self.edge_info.iter().any(|e| {
                e.from_id == edge.to_id && e.to_id == edge.from_id
            });
            if reverse {
                let from_name = self.systems.iter()
                    .find(|s| s.id == edge.from_id)
                    .map(|s| s.name.as_str())
                    .unwrap_or("?");
                let to_name = self.systems.iter()
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
            pairs.join(", ")
        }
    }

    // ── Выполнение ─────────────────────────────────────────────

    /// Запустить одну итерацию планировщика.
    /// С feature = "parallel" — параллельный путь через Rayon.
    ///
    /// Startup этап выполняется только при первом вызове `run()`.
    pub fn run(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        // Вычисляем маппинг систем → архетипы для SubWorld.
        self.compute_archetype_indices(world);

        #[cfg(feature = "parallel")]
        self.run_hybrid_parallel(world);

        #[cfg(not(feature = "parallel"))]
        self.run_sequential(world);

        // После первого run() Startup больше не выполняется
        self.startup_completed = true;
    }

    /// Последовательное выполнение — для тестов и non-parallel builds.
    pub fn run_sequential(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        let plan = self.execution_plan.as_ref().unwrap();

        // Фильтруем Startup системы если этап уже был выполнен
        let order: Vec<SystemId> = plan.stages.iter()
            .filter(|stage| {
                if stage.label == StageLabel::Startup && self.startup_completed {
                    return false; // пропускаем Startup
                }
                true
            })
            .flat_map(|s| s.system_ids.iter().copied())
            .collect();

        // Sequential системы получают &mut World, параллельные — SubWorld
        let all_indices: Vec<usize> = (0..world.archetypes().len()).collect();
        let sub_world = apex_core::SubWorld::new(
            unsafe { &*(world as *mut World as *const World) },
            &all_indices,
        );

        for sys_id in order {
            if let Some(index) = self.system_indices.get(&sys_id) {
                let system = &mut self.systems[*index];
                match &mut system.kind {
                    SystemKind::Sequential(f) => f(world),
                    SystemKind::Parallel { system, .. } => {
                        system.run(SystemContext::from_sub_world(&sub_world));
                    }
                }
            }
        }

        // После первого выполнения Startup больше не запускается
        self.startup_completed = true;
    }

    /// Параллельное выполнение через Rayon scope + spawn.
    ///
    /// Системы в одном Stage (все ParSystem, нет конфликтов) запускаются
    /// через `rayon::scope(|s| { for ... { s.spawn(...) } })` — каждая
    /// система получает свой поток без оверхеда на split/steal от par_iter.
    ///
    /// Для stage с малым числом систем (< parallel_threshold) используется
    /// последовательный запуск, чтобы избежать оверхеда rayon::scope.
    #[cfg(feature = "parallel")]
    fn run_hybrid_parallel(&mut self, world: &mut World) {
        let plan = self.execution_plan.as_ref().unwrap();
        // Фильтруем Startup если уже выполнен
        let stages: Vec<(Vec<SystemId>, bool)> = plan.stages
            .iter()
            .filter(|stage| {
                if stage.label == StageLabel::Startup && self.startup_completed {
                    return false;
                }
                true
            })
            .map(|s| (s.system_ids.clone(), s.all_parallel))
            .collect();

        // Pre-вычисляем SubWorld storage для всех систем
        // Это безопасно, потому что:
        // - SubWorld не владеет данными, только ссылается
        // - Мы не делаем structural changes во время выполнения
        // - Разные SubWorld для разных систем не пересекаются по архетипам
        let const_world: &World = unsafe { &*(world as *mut World as *const World) };
        self.prepare_sub_worlds(const_world);

        // Строим маппинг system_idx → storage_idx
        // storage_idx = порядковый номер системы в self.systems
        // (prepare_sub_worlds заполняет storage в том же порядке)
        let system_to_storage: Vec<usize> = (0..self.systems.len()).collect();

        for (stage_ids, all_parallel) in &stages {
            // Проверяем threshold для параллельного выполнения
            if !all_parallel || stage_ids.len() < self.parallel_threshold {
                for &sys_id in stage_ids {
                    if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                        // SubWorld должен быть создан ДО mutable borrow self.systems
                        let sw = self.make_sub_world(system_to_storage[sys_idx], const_world);
                        let system = &mut self.systems[sys_idx];
                        match &mut system.kind {
                            SystemKind::Sequential(f)           => f(world),
                            SystemKind::Parallel { system, .. } => {
                                system.run(SystemContext::from_sub_world(&sw));
                            }
                        }
                    }
                }
                continue;
            }

            let indices: Vec<usize> = stage_ids
                .iter()
                .filter_map(|sid| self.system_indices.get(sid))
                .copied()
                .collect();

            // Создаём SubWorld для каждой системы ПЕРЕД scope
            // (SubWorld содержит ссылки, поэтому должен быть создан до замыкания)
            let sub_worlds: Vec<apex_core::SubWorld<'_>> = indices.iter().map(|&sys_idx| {
                self.make_sub_world(system_to_storage[sys_idx], const_world)
            }).collect();

            // Используем rayon::scope + spawn вместо par_iter().for_each(),
            // чтобы избежать оверхеда на split/steal.
            // Каждая система spawn-ится как отдельная задача в thread pool.
            // Это эффективно для малого числа систем (≤ num_threads).
            // SendPtr обеспечивает Send+Sync для сырых указателей.
            // Используем into_iter() для потребления sub_worlds — каждая
            // итерация забирает один элемент, избегая move всей Vec.
            rayon::scope(|s| {
                let mut sub_worlds_iter = sub_worlds.into_iter();
                for &sys_idx in &indices {
                    let ptr = SendPtr(&mut self.systems[sys_idx] as *mut SystemDescriptor);
                    let sw = sub_worlds_iter.next().unwrap();
                    s.spawn(move |_| {
                        let descriptor = unsafe { ptr.as_mut() };
                        if let SystemKind::Parallel { system, .. } = &mut descriptor.kind {
                            system.run(SystemContext::from_sub_world(&sw));
                        }
                    });
                }
            });
        }
    }

    /// Создать SubWorld для системы на основе предвычисленных archetype_indices.
    ///
    /// Использует `archetype_indices_storage` для owned хранения данных,
    /// что позволяет избежать `Box::leak`. Storage заполняется заранее
    /// в `prepare_sub_worlds()`.
    ///
    /// # SAFETY
    /// - `storage_idx` должен быть валидным индексом в `archetype_indices_storage`
    /// - `archetype_indices_storage` не должен изменяться, пока SubWorld жив
    fn make_sub_world<'w>(&self, storage_idx: usize, world: &'w World) -> apex_core::SubWorld<'w> {
        let arch_indices: &'w [usize] = unsafe {
            let vec = &self.archetype_indices_storage[storage_idx];
            std::slice::from_raw_parts(vec.as_ptr(), vec.len())
        };
        apex_core::SubWorld::new(world, arch_indices)
    }

    /// Подготовить storage для SubWorld — заполняет `archetype_indices_storage`
    /// для всех систем. Вызывается перед `run_hybrid_parallel()`.
    /// Позволяет избежать `Box::leak` в `make_sub_world`.
    fn prepare_sub_worlds(&mut self, world: &World) {
        self.archetype_indices_storage.clear();
        let arch_count = world.archetypes().len();

        for system in &self.systems {
            let indices = self.system_archetype_indices.get(&system.id);
            match indices {
                Some(indices) if !indices.is_empty() => {
                    self.archetype_indices_storage.push(indices.clone());
                }
                _ => {
                    // fallback: все архетипы
                    self.archetype_indices_storage.push(
                        (0..arch_count).collect()
                    );
                }
            }
        }
    }

    // ── Инспекция ──────────────────────────────────────────────

    pub fn system_count(&self) -> usize { self.systems.len() }

    pub fn stages(&self) -> Option<&[Stage]> {
        self.execution_plan.as_ref().map(|p| p.stages.as_slice())
    }

    /// Краткий план выполнения.
    pub fn debug_plan(&self) -> String {
        let Some(plan) = &self.execution_plan else {
            return "(not compiled — call compile() first)".to_string();
        };
        let mut out = String::new();
        for (i, stage) in plan.stages.iter().enumerate() {
            let mode = if stage.is_parallelizable()  { "PARALLEL" }
                       else if stage.all_parallel     { "parallel/single" }
                       else                           { "sequential" };
            out.push_str(&format!("Stage {} [{}] ({}) :\n", i, mode, stage.label));
            for sys_id in &stage.system_ids {
                if let Some(s) = self.systems.iter().find(|s| s.id == *sys_id) {
                    let kind_str = match &s.kind {
                        SystemKind::Parallel { access, .. } =>
                            format!("par | R:{} W:{}", access.reads.len(), access.writes.len()),
                        SystemKind::Sequential(_) =>
                            "seq | full &mut World".to_string(),
                    };
                    out.push_str(&format!("  - {} [{}]\n", s.name, kind_str));
                }
            }
        }
        out
    }

    /// Подробный план с причинами разделения Stage.
    ///
    /// Показывает какой конфликт компонентов привёл к тому что
    /// системы оказались в разных Stage. Полезно при отладке
    /// расписания и оптимизации параллелизма.
    ///
    /// # Пример вывода
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

        // ── Стадии ────────────────────────────────────────────
        for (i, stage) in plan.stages.iter().enumerate() {
            let mode = if stage.is_parallelizable()  { "PARALLEL" }
                       else if stage.all_parallel     { "parallel/single" }
                       else                           { "sequential" };
            out.push_str(&format!("Stage {} [{}] ({}):\n", i, mode, stage.label));
            for sys_id in &stage.system_ids {
                if let Some(s) = self.systems.iter().find(|s| s.id == *sys_id) {
                    match &s.kind {
                        SystemKind::Parallel { access, .. } => {
                            let reads: Vec<_>  = access.reads.iter()
                                .map(|tid| format!("{:?}", tid))
                                .collect();
                            let writes: Vec<_> = access.writes.iter()
                                .map(|tid| format!("{:?}", tid))
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
                }
            }
        }

        // ── SubWorld маппинг (архетипы для каждой системы) ────
        if !self.system_archetype_indices.is_empty() {
            out.push_str("\n  ── SubWorld archetype mapping ──\n");
            for sys_id in plan.flat_order.iter() {
                if let Some(s) = self.systems.iter().find(|s| s.id == *sys_id) {
                    if let Some(indices) = self.system_archetype_indices.get(sys_id) {
                        out.push_str(&format!(
                            "  {}: {} archetypes [{}]\n",
                            s.name,
                            indices.len(),
                            if indices.len() <= 10 {
                                indices.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
                            } else {
                                format!("{}..{}", indices[0], indices[indices.len()-1])
                            }
                        ));
                    }
                }
            }
        }

        // ── Conflict edges ────────────────────────────────────
        if !self.edge_info.is_empty() {
            out.push_str("\n── Conflict edges ──────────────────────────────────────────────────\n");
            for edge in &self.edge_info {
                let from_name = self.systems.iter()
                    .find(|s| s.id == edge.from_id)
                    .map(|s| s.name.as_str())
                    .unwrap_or("?");
                let to_name = self.systems.iter()
                    .find(|s| s.id == edge.to_id)
                    .map(|s| s.name.as_str())
                    .unwrap_or("?");
                out.push_str(&format!(
                    "  {:<20} →  {:<20}  {}\n",
                    from_name, to_name, edge.kind
                ));
            }
        }

        // ── Параллелизм summary ───────────────────────────────
        let par_stages  = plan.stages.iter().filter(|s| s.is_parallelizable()).count();
        let seq_stages  = plan.stages.iter().filter(|s| !s.all_parallel).count();
        let max_par     = plan.stages.iter().map(|s| s.system_count()).max().unwrap_or(0);
        out.push_str(&format!(
            "\n── Summary: {} stages ({} parallel, {} sequential), max parallelism: {} systems\n",
            plan.stages.len(), par_stages, seq_stages, max_par
        ));

        out
    }

    /// Получить причины конфликта между двумя конкретными системами.
    pub fn conflicts_between(&self, a: SystemId, b: SystemId) -> Vec<&ConflictKind> {
        self.edge_info.iter()
            .filter(|e| (e.from_id == a && e.to_id == b) || (e.from_id == b && e.to_id == a))
            .map(|e| &e.kind)
            .collect()
    }
}

impl Default for Scheduler { fn default() -> Self { Self::new() } }

// ── Вспомогательные функции ────────────────────────────────────

/// Определить тип конфликта между двумя AccessDescriptor.
///
/// Возвращает (ConflictKind, направление) где направление true означает i→j.
/// Если конфликтов нет — None.
fn detect_conflict_kind(
    ai: &AccessDescriptor,
    aj: &AccessDescriptor,
    id_i: SystemId,
    id_j: SystemId,
) -> Option<(ConflictKind, bool)> {
    // ── Компонентные конфликты ──────────────────────────────────

    // Write+Write: оба пишут в один компонент
    for w in &ai.writes {
        if aj.writes.contains(w) {
            return Some((ConflictKind::WriteWrite {
                component_name: component_type_name(*w),
            }, true)); // i→j
        }
    }
    // Write(i)+Read(j): i пишет то что j читает
    for w in &ai.writes {
        if aj.reads.contains(w) {
            return Some((ConflictKind::WriteRead {
                component_name: component_type_name(*w),
                writer_id: id_i.0,
                reader_id: id_j.0,
            }, true)); // i→j (писатель → читатель)
        }
    }
    // Write(j)+Read(i): j пишет то что i читает
    for w in &aj.writes {
        if ai.reads.contains(w) {
            return Some((ConflictKind::WriteRead {
                component_name: component_type_name(*w),
                writer_id: id_j.0,
                reader_id: id_i.0,
            }, false)); // j→i
        }
    }

    // ── Event конфликты ─────────────────────────────────────────

    // EventWriteWrite: оба пишут в один тип событий
    for w in &ai.writes_event {
        if aj.writes_event.contains(w) {
            return Some((ConflictKind::EventWriteWrite {
                event_name: component_type_name(*w),
            }, true)); // i→j
        }
    }
    // EventWrite(i)+EventRead(j): i пишет событие, j читает
    for w in &ai.writes_event {
        if aj.reads_event.contains(w) {
            return Some((ConflictKind::EventWriteRead {
                event_name: component_type_name(*w),
                writer_id: id_i.0,
                reader_id: id_j.0,
            }, true)); // i→j (писатель → читатель)
        }
    }
    // EventWrite(j)+EventRead(i): j пишет событие, i читает
    for w in &aj.writes_event {
        if ai.reads_event.contains(w) {
            return Some((ConflictKind::EventWriteRead {
                event_name: component_type_name(*w),
                writer_id: id_j.0,
                reader_id: id_i.0,
            }, false)); // j→i
        }
    }

    None
}

/// Получить имя типа по TypeId.
///
/// В release builds TypeId не содержит имя — возвращаем заглушку.
/// В debug/dev — также заглушка, но ConflictKind::WriteWrite { component_name }
/// всё равно содержит TypeId для сравнения.
fn component_type_name(type_id: std::any::TypeId) -> &'static str {
    // TypeId не даёт имя в stable Rust. Для диагностики достаточно
    // знать что конфликт ЕСТЬ — имя TypeId видно в AccessDescriptor.reads/writes.
    // В будущем можно добавить registry TypeId→&str в World.
    let _ = type_id;
    "<component>"
}

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::{prelude::*, world::World, query::Query};

    #[derive(Clone, Copy)] struct Pos { x: f32, y: f32 }
    #[derive(Clone, Copy)] struct Vel { x: f32, y: f32 }
    #[derive(Clone, Copy)] struct Hp(f32);
    #[derive(Clone, Copy)] struct DeltaTime(f32);

    // ── AutoSystem тесты ──────────────────────────────────────

    struct AutoMovement;
    impl AutoSystem for AutoMovement {
        type Query = (Read<Vel>, Write<Pos>);
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<Self::Query>()
                .for_each_component(|(vel, pos)| {
                    pos.x += vel.x;
                    pos.y += vel.y;
                });
        }
    }

    struct AutoHealth;
    impl AutoSystem for AutoHealth {
        type Query = Write<Hp>;
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<Self::Query>()
                .for_each_component(|hp| {
                    hp.0 = hp.0.max(0.0);
                });
        }
    }

    #[test]
    fn auto_system_access_correct() {
        // AutoMovement должен иметь read:Vel, write:Pos
        let access = <(Read<Vel>, Write<Pos>) as WorldQuerySystemAccess>::system_access();
        assert!(!access.reads.is_empty(),  "должен читать Vel");
        assert!(!access.writes.is_empty(), "должен писать Pos");
    }

    #[test]
    fn auto_system_runs_correctly() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.spawn_bundle((Pos { x: 0.0, y: 0.0 }, Vel { x: 3.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each_component(|p| { result = (p.x, p.y); });
        assert!((result.0 - 3.0).abs() < 1e-6);
        assert!((result.1 - 4.0).abs() < 1e-6);
    }

    #[test]
    fn auto_system_no_conflict_same_stage() {
        // AutoMovement (Write<Pos>) и AutoHealth (Write<Hp>) — нет конфликта
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health",   AutoHealth);
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1, "нет конфликта — должен быть 1 Stage");
        assert!(stages[0].all_parallel);
        assert_eq!(stages[0].system_count(), 2);
    }

    #[test]
    fn auto_system_conflict_separate_stages() {
        // Два AutoSystem пишут в Pos — конфликт
        struct AutoMovement2;
        impl AutoSystem for AutoMovement2 {
            type Query = Write<Pos>;
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_auto_system("m1", AutoMovement);  // Write<Pos>
        sched.add_auto_system("m2", AutoMovement2); // Write<Pos>
        sched.compile().unwrap();

        assert_eq!(sched.stages().unwrap().len(), 2, "Write+Write должен дать 2 Stage");
    }

    // ── ConflictKind тесты ────────────────────────────────────

    #[test]
    fn conflict_kind_in_edge_info() {
        struct WriterA; impl ParSystem for WriterA {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB; impl ParSystem for WriterB {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        let a = sched.add_par_system("a", WriterA);
        let b = sched.add_par_system("b", WriterB);
        sched.compile().unwrap();

        let conflicts = sched.conflicts_between(a, b);
        assert!(!conflicts.is_empty(), "должен быть конфликт");
        assert!(matches!(conflicts[0], ConflictKind::WriteWrite { .. }));
    }

    #[test]
    fn sequential_barrier_in_edge_info() {
        let mut sched = Scheduler::new();
        let _par = sched.add_auto_system("movement", AutoMovement);
        let _seq = sched.add_system("barrier", |_| {}).id();
        sched.compile().unwrap();

        // Должны быть рёбра с SequentialBarrier
        let has_barrier = sched.edge_info.iter()
            .any(|e| matches!(e.kind, ConflictKind::SequentialBarrier));
        assert!(has_barrier, "Sequential барьер должен быть в edge_info");
    }

    // ── debug_plan_verbose тест ───────────────────────────────

    #[test]
    fn debug_plan_verbose_works() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health",   AutoHealth);
        sched.add_system("commands", |_| {});
        sched.compile().unwrap();

        let plan = sched.debug_plan_verbose();
        assert!(plan.contains("PARALLEL"),    "должен быть PARALLEL Stage");
        assert!(plan.contains("sequential"),  "должен быть sequential Stage");
        assert!(plan.contains("Conflict"),    "должен показывать конфликты");
        assert!(plan.contains("Summary"),     "должен показывать summary");
    }

    // ── Инкрементальность тест ────────────────────────────────

    #[test]
    fn incremental_compile_after_add() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.compile().unwrap();

        // Граф скомпилирован
        assert!(sched.execution_plan.is_some());
        assert!(!sched.graph_dirty);

        // Добавляем новую систему — план инвалидируется
        sched.add_auto_system("health", AutoHealth);
        assert!(sched.execution_plan.is_none());
        assert!(sched.graph_dirty);

        // Compile снова
        sched.compile().unwrap();
        assert!(sched.execution_plan.is_some());
        assert_eq!(sched.stages().unwrap().len(), 1);
    }

    // ── Оригинальные тесты (совместимость) ───────────────────

    #[test]
    fn sequential_ordering() {
        struct MovementSystem;
        impl ParSystem for MovementSystem {
            fn access() -> AccessDescriptor { AccessDescriptor::new().read::<Vel>().write::<Pos>() }
            fn run(&mut self, ctx: SystemContext<'_>) {
                ctx.query::<(Read<Vel>, Write<Pos>)>()
                    .for_each_component(|(vel, pos)| {
                        pos.x += vel.x; pos.y += vel.y;
                    });
            }
        }

        let mut sched = Scheduler::new();
        let log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> = Default::default();

        let log_a = log.clone();
        let a = sched.add_system("a", move |_| { log_a.lock().unwrap().push("a"); }).id();
        let log_b = log.clone();
        let b = sched.add_system("b", move |_| { log_b.lock().unwrap().push("b"); }).id();

        sched.add_dependency(b, a);
        sched.compile().unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);
        assert_eq!(*log.lock().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn circular_dependency_detected() {
        let mut sched = Scheduler::new();
        let a = sched.add_system("a", |_| {}).id();
        let b = sched.add_system("b", |_| {}).id();
        sched.add_dependency(b, a);
        sched.add_dependency(a, b);
        let err = sched.compile();
        assert!(err.is_err());
        // Сообщение об ошибке должно содержать имена систем
        if let Err(SchedulerError::CircularDependency { cycle_info }) = err {
            assert!(cycle_info.contains("a") || cycle_info.contains("b"));
        }
    }

    #[test]
    fn par_write_conflict_separate_stages() {
        struct WriterA; impl ParSystem for WriterA {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB; impl ParSystem for WriterB {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("a", WriterA);
        sched.add_par_system("b", WriterB);
        sched.compile().unwrap();

        assert_eq!(sched.stages().unwrap().len(), 2);
    }

    #[test]
    fn sequential_breaks_parallel_groups() {
        struct MovementSystem;
        impl ParSystem for MovementSystem {
            fn access() -> AccessDescriptor { AccessDescriptor::new().read::<Vel>().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct HealthSystem;
        impl ParSystem for HealthSystem {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Hp>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("par_a", MovementSystem);
        sched.add_system("barrier", |_| {});
        sched.add_par_system("par_b", HealthSystem);
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(stages.len() >= 3);
        assert!(stages.iter().any(|s| !s.all_parallel));
    }

    #[test]
    fn fn_par_system_with_resource() {
        let mut sched = Scheduler::new();
        sched.add_fn_par_system(
            "scaled_movement",
            |ctx: SystemContext<'_>| {
                let dt = ctx.resource::<DeltaTime>();
                ctx.query::<(Read<Vel>, Write<Pos>)>()
                    .for_each_component(|(vel, pos)| {
                        pos.x += vel.x * (*dt).0;
                        pos.y += vel.y * (*dt).0;
                    });
            },
            AccessDescriptor::new()
                .read::<DeltaTime>()
                .read::<Vel>()
                .write::<Pos>(),
        );

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.insert_resource(DeltaTime(0.5));
        world.spawn_bundle((Pos { x: 0.0, y: 0.0 }, Vel { x: 2.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each_component(|p| { result = (p.x, p.y); });

        assert!((result.0 - 1.0).abs() < 1e-6);
        assert!((result.1 - 2.0).abs() < 1e-6);
    }

    /// Параллельное выполнение: обе AutoSystem применяют изменения.
    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_auto_systems_correctness() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health",   AutoHealth);
        sched.compile().unwrap();
        assert!(sched.stages().unwrap()[0].is_parallelizable());

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.register_component::<Hp>();
        world.spawn_bundle((
            Pos { x: 0.0, y: 0.0 },
            Vel { x: 1.0, y: 2.0 },
            Hp(-5.0),
        ));

        sched.run(&mut world);

        let mut pos_result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each_component(|p| { pos_result = (p.x, p.y); });
        assert!((pos_result.0 - 1.0).abs() < 1e-6);
        assert!((pos_result.1 - 2.0).abs() < 1e-6);

        let mut hp_result = -1.0f32;
        Query::<Read<Hp>>::new(&world).for_each_component(|hp| { hp_result = hp.0; });
        assert!((hp_result - 0.0).abs() < 1e-6);
    }

    // ── StageLabel тесты ────────────────────────────────────────

    #[test]
    fn startup_system_runs_once() {
        let mut sched = Scheduler::new();
        let startup_count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let counter = startup_count.clone();

        sched.add_startup_system("init", move |_| {
            *counter.lock().unwrap() += 1;
        });

        let mut world = World::new();

        // Первый run() — Startup выполняется
        sched.run_sequential(&mut world);
        assert_eq!(*startup_count.lock().unwrap(), 1, "Startup должен выполниться 1 раз");

        // Второй run() — Startup НЕ выполняется
        sched.run_sequential(&mut world);
        assert_eq!(*startup_count.lock().unwrap(), 1, "Startup НЕ должен выполниться повторно");
    }

    #[test]
    fn stage_label_in_debug_plan() {
        let mut sched = Scheduler::new();
        sched.add_startup_system("init", |_| {});
        sched.add_auto_system("movement", AutoMovement);
        sched.compile().unwrap();

        let plan = sched.debug_plan();
        assert!(plan.contains("Startup"), "debug_plan должен содержать Startup label");
        assert!(plan.contains("Update"),  "debug_plan должен содержать Update label");
    }

    #[test]
    fn add_system_to_stage_custom_label() {
        let mut sched = Scheduler::new();

        // Добавляем системы в разные этапы
        sched.add_system_to_stage("pre", |_| {}, StageLabel::PreUpdate);
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // Должно быть минимум 2 Stage: PreUpdate и Update
        assert!(stages.len() >= 2, "Должно быть минимум 2 Stage, получено {}", stages.len());

        // Проверяем что PreUpdate идут перед Update
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        assert!(pre_idx.is_some(), "Должен быть PreUpdate Stage");
        assert!(upd_idx.is_some(), "Должен быть Update Stage");
        assert!(pre_idx.unwrap() < upd_idx.unwrap(),
            "PreUpdate должен быть перед Update");
    }

    #[test]
    fn startup_auto_system() {
        let mut sched = Scheduler::new();

        // AutoSystem на Startup
        sched.add_startup_auto_system("init_movement", AutoMovement);

        // Обычная система на Update
        sched.add_auto_system("update_health", AutoHealth);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(stages.len() >= 2, "Должно быть минимум 2 Stage");

        // Startup выполняется первым
        assert_eq!(stages[0].label, StageLabel::Startup,
            "Первый Stage должен быть Startup");
        assert!(stages.iter().any(|s| s.label == StageLabel::Update),
            "Должен быть Update Stage");
    }

    #[test]
    fn startup_system_works_via_run() {
        let mut sched = Scheduler::new();
        let startup_val = std::sync::Arc::new(std::sync::Mutex::new(0i32));
        let val = startup_val.clone();

        sched.add_startup_system("init", move |world: &mut World| {
            world.insert_resource(42i32);
            *val.lock().unwrap() = 42;
        });

        let mut world = World::new();

        // Первый run
        sched.run_sequential(&mut world);
        assert_eq!(*startup_val.lock().unwrap(), 42, "Startup система должна выполниться");
        assert_eq!(*world.resource::<i32>(), 42, "Ресурс должен быть установлен");

        // Второй run — ресурс должен остаться (Startup не перезаписывает)
        sched.run_sequential(&mut world);
        assert_eq!(*world.resource::<i32>(), 42, "Ресурс не должен измениться");
    }

    // ── Event конфликты ────────────────────────────────────────

    struct EventWriterForTest;
    impl ParSystem for EventWriterForTest {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write_event::<i32>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct AnotherEventWriter;
    impl ParSystem for AnotherEventWriter {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write_event::<i32>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct EventReaderForTest;
    impl ParSystem for EventReaderForTest {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<i32>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    #[test]
    fn event_write_write_conflict() {
        let mut sched = Scheduler::new();

        sched.add_par_system("writer_a", EventWriterForTest);
        sched.add_par_system("writer_b", AnotherEventWriter);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // Два EventWriter одного типа → должны быть в разных Stage (конфликт)
        assert!(stages.len() >= 2,
            "EventWriteWrite конфликт: ожидается минимум 2 Stage, получено {}", stages.len());
    }

    #[test]
    fn event_write_read_conflict() {
        let mut sched = Scheduler::new();

        sched.add_par_system("writer", EventWriterForTest);
        sched.add_par_system("reader", EventReaderForTest);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // EventWriter + EventReader одного типа → должны быть в разных Stage (конфликт)
        assert!(stages.len() >= 2,
            "EventWriteRead конфликт: ожидается минимум 2 Stage, получено {}", stages.len());
    }

    #[test]
    fn event_read_read_no_conflict() {
        let mut sched = Scheduler::new();

        sched.add_par_system("reader_a", EventReaderForTest);
        sched.add_par_system("reader_b", EventReaderForTest);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // Два EventReader одного типа → НЕТ конфликта, могут быть в одном Stage
        // Проверяем что есть хотя бы один Stage с обеими системами
        let found = stages.iter().any(|s| {
            s.system_ids.len() >= 2
        });
        assert!(found,
            "EventRead не должны конфликтовать: ожидается Stage с обеими системами");
    }

    #[test]
    fn event_conflict_kind_in_edge_info() {
        let mut sched = Scheduler::new();

        let wid = sched.add_par_system("writer", EventWriterForTest);
        let rid = sched.add_par_system("reader", AnotherEventWriter);

        sched.compile().unwrap();

        let conflicts = sched.conflicts_between(wid, rid);
        assert!(!conflicts.is_empty(),
            "Должен быть конфликт между EventWriter и EventWriter");

        // Проверяем тип конфликта
        let has_event_conflict = conflicts.iter().any(|c| {
            matches!(c, ConflictKind::EventWriteWrite { .. })
        });
        assert!(has_event_conflict,
            "Конфликт должен быть EventWriteWrite");
    }

    // ── configure_stages ─────────────────────────────────────────

    #[test]
    fn configure_stages_custom_order() {
        let mut sched = Scheduler::new();

        // Добавляем системы в Update и PreUpdate
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);
        sched.add_system_to_stage("pre_work", |_| {}, StageLabel::PreUpdate);

        // Меняем порядок: Update ДО PreUpdate
        sched.configure_stages(vec![
            StageLabel::Startup,
            StageLabel::Update,
            StageLabel::PreUpdate,
            StageLabel::First,
            StageLabel::PostUpdate,
            StageLabel::Last,
        ]);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();

        // Update должен быть перед PreUpdate
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);

        assert!(upd_idx.is_some(), "Должен быть Update Stage");
        assert!(pre_idx.is_some(), "Должен быть PreUpdate Stage");
        assert!(upd_idx.unwrap() < pre_idx.unwrap(),
            "Update должен быть перед PreUpdate при configure_stages");
    }

    #[test]
    fn configure_stages_keeps_missing_at_end() {
        let mut sched = Scheduler::new();

        // Добавляем системы в разные этапы
        sched.add_system_to_stage("pre", |_| {}, StageLabel::PreUpdate);
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);
        sched.add_system_to_stage("last_work", |_| {}, StageLabel::Last);

        // Указываем только Update и PreUpdate — Last не указан
        sched.configure_stages(vec![
            StageLabel::Startup,
            StageLabel::Update,
            StageLabel::PreUpdate,
        ]);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();

        // Update и PreUpdate должны быть (в указанном порядке)
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(upd_idx.is_some());
        assert!(pre_idx.is_some());
        assert!(upd_idx.unwrap() < pre_idx.unwrap(),
            "Update должен быть перед PreUpdate");

        // Last должен быть в конце (не указан в order, добавлен автоматически)
        let last_idx = stages.iter().position(|s| s.label == StageLabel::Last);
        assert!(last_idx.is_some(), "Last должен присутствовать даже если не указан в configure_stages");
        assert!(last_idx.unwrap() > pre_idx.unwrap() || last_idx.unwrap() > upd_idx.unwrap(),
            "Last (не указанный в order) должен быть в конце");
    }
}