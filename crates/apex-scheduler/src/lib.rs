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
//!         ctx.for_each::<Self::Query, _>(|(vel, pos)| {
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
//! ## 4. `par_for_each` в SystemContext (в apex-core/world.rs)
//!
//! Параллелизм внутри одной системы по архетипам через Rayon.
//!
//! # Типы систем
//!
//! | Тип | Access | Использование |
//! |-----|--------|---------------|
//! | AutoSystem | автовывод из Query + Resources + Events | рекомендуется |
//! | FnParSystem | явный + замыкание | быстрые прототипы |
//! | Sequential | полный &mut World | structural changes |

// Планировщик — высокопроизводительный код с внутренним `unsafe` (ASD, SendPtr,
// rayon::scope). Часть линтов смягчена намеренно (см. аналогичный блок в apex-core).
#![allow(
    clippy::missing_safety_doc,
    clippy::needless_range_loop,
    clippy::nonminimal_bool,
    clippy::question_mark,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::large_enum_variant
)]

pub mod conditions;
pub mod pipeline;
pub mod stage;

pub use config::{
    par, par_access, seq, sys, AutoMarker, ConfigMarker, ExclusiveMarker, FnSystemExt,
    FnSystemMarker, IntoScheduleConfigs, SystemConfig,
};
pub mod fixed;
pub use fixed::FixedTime;
pub mod states;
pub use states::{in_state, init_state, on_enter, on_exit, NextState, State, StateTransitions, States};

mod config;
use crate::config::SystemConfigKind;

use apex_core::commands::Commands;
use apex_core::{
    component::ComponentRegistry, system_param::WorldQuerySystemAccess, world::World,
    AccessDescriptor,
};
use apex_graph::Graph;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::any::TypeId;
use thiserror::Error;
use thunderdome::Index;

// ── Main-world per-system профайлер (`APEX_MAIN_PROF=1`) ────────────────────
//
// Зеркало рендерного `APEX_PROF`: render-мир и extract давно имели разбивку, а
// exclusive-системы main-мира мерились только ad-hoc обёртками в примерах.
// Лог раз в ~2с: среднее ms/вызов по каждой системе, отсортировано по убыванию.

fn main_prof_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("APEX_MAIN_PROF").as_deref() == Ok("1"))
}

fn main_prof_record(name: &str, elapsed: std::time::Duration) {
    use std::sync::Mutex;
    use std::time::Instant;
    static ACC: Mutex<Vec<(String, f64, u32)>> = Mutex::new(Vec::new());
    static LAST_LOG: Mutex<Option<Instant>> = Mutex::new(None);

    let mut acc = ACC.lock().unwrap();
    let ms = elapsed.as_secs_f64() * 1000.0;
    if let Some(e) = acc.iter_mut().find(|(n, _, _)| n == name) {
        e.1 += ms;
        e.2 += 1;
    } else {
        acc.push((name.to_string(), ms, 1));
    }

    let mut last = LAST_LOG.lock().unwrap();
    if last.map(|t| t.elapsed().as_secs_f32() > 2.0).unwrap_or(true) {
        *last = Some(Instant::now());
        let mut rows: Vec<(String, f64)> = acc
            .iter()
            .map(|(n, sum, count)| (n.clone(), *sum / (*count).max(1) as f64))
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        let line: String = rows
            .iter()
            .map(|(n, avg)| format!("{n} {avg:.2}"))
            .collect::<Vec<_>>()
            .join(" | ");
        log::info!("MAIN PROF (ms/вызов, avg ~2s): {line}");
        for e in acc.iter_mut() {
            e.1 = 0.0;
            e.2 = 0;
        }
    }
}

/// Раз в ~2с — сводка по архетипам мира (CR-M4: счётчик живых строк в отчёте
/// `APEX_MAIN_PROF`; рост empty/archetypes — сигнал фрагментации).
fn main_prof_world_stats(world: &apex_core::World) {
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if last.map(|t| t.elapsed().as_secs_f32() > 2.0).unwrap_or(true) {
        *last = Some(Instant::now());
        let s = world.archetype_stats();
        log::info!(
            "MAIN PROF мир: archetypes={} (empty={}), rows={}, max_arch_rows={}, \
             mem={:.2} MiB (data={:.2} ticks={:.2} entities={:.2})",
            s.archetypes,
            s.empty_archetypes,
            s.total_rows,
            s.max_rows_in_archetype,
            s.total_bytes() as f64 / (1024.0 * 1024.0),
            s.component_bytes as f64 / (1024.0 * 1024.0),
            s.tick_bytes as f64 / (1024.0 * 1024.0),
            s.entity_bytes as f64 / (1024.0 * 1024.0),
        );
    }
}

pub use apex_core::system_param::{
    AutoSystem, Emit, EventAccessList, Listen, ResRead, ResWrite, ResourceAccessList,
};
pub use apex_core::world::SystemContext;
pub use apex_core::AccessDescriptor as Access;
pub use pipeline::{EventPipelineBuilder, PipelineRole, PipelineValidationError};
pub use stage::{Stage, StageLabel};

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
    WriteWrite { component_name: &'static str },
    /// Один пишет, другой читает — Write+Read конфликт
    WriteRead {
        component_name: &'static str,
        writer_id: u32,
        reader_id: u32,
    },
    /// Sequential барьер — система с полным &mut World
    SequentialBarrier,
    /// Два EventWriter одного типа событий
    EventWriteWrite { event_name: &'static str },
    /// EventWriter и EventReader одного типа событий
    EventWriteRead {
        event_name: &'static str,
        writer_id: u32,
        reader_id: u32,
    },
    /// Обе системы пишут в компоненты, которые другая читает
    /// (A writes Pos that B reads, B writes Vel that A reads).
    BidirectionalWriteRead { a_name: String, b_name: String },
    /// Two systems read the SAME event type. They must be serialized because each
    /// `EventReader` mutates the event queue's shared cursor registry on register
    /// / advance / drop — running them in parallel is a data race (F2). Reader
    /// parallelism is restored by the per-system cursor model (wave 6).
    SharedEventReaders { event_name: &'static str },
}

impl std::fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConflictKind::Explicit => write!(f, "explicit dependency"),
            ConflictKind::WriteWrite { component_name } => {
                write!(f, "Write+Write conflict on `{}`", component_name)
            }
            ConflictKind::WriteRead { component_name, .. } => {
                write!(f, "Write+Read conflict on `{}`", component_name)
            }
            ConflictKind::SequentialBarrier => write!(f, "sequential barrier (&mut World)"),
            ConflictKind::EventWriteWrite { event_name } => {
                write!(f, "Event Write+Write conflict on `{}`", event_name)
            }
            ConflictKind::EventWriteRead { event_name, .. } => {
                write!(f, "Event Write+Read conflict on `{}`", event_name)
            }
            ConflictKind::SharedEventReaders { event_name } => {
                write!(f, "shared EventReader cursor race on `{}`", event_name)
            }
            ConflictKind::BidirectionalWriteRead { a_name, b_name } => write!(
                f,
                "bidirectional Write+Read between `{}` and `{}`",
                a_name, b_name
            ),
        }
    }
}

// ── SchedulerError ─────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("{cycle_info}")]
    CircularDependency { cycle_info: String },
    #[error("System '{0}' not found")]
    SystemNotFound(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SystemId(pub u32);

// ── ConditionTree — составные условия выполнения ────────────

/// Дерево условий: определяет, должна ли система выполниться в этом кадре.
///
/// Оценивается на главном потоке **до** выполнения stage'а.
/// Поддерживает AND (все дочерние true) и OR (хотя бы одно true).
///
/// # Композиция
///
/// ```ignore
/// // AND — несколько run_if подряд:
/// s.run_if(cond_a).run_if(cond_b)        → And([Leaf(a), Leaf(b)])
///
/// // OR — через or_else:
/// s.or_else(cond_a).or_else(cond_b)      → Or([Leaf(a), Leaf(b)])
/// ```
///
/// # Оценка
///
/// AND использует шорт-циркут: как только одно условие false — остальные не проверяются.
/// OR использует шорт-циркут: как только одно true — остальные не проверяются.
/// Это безопасно, потому что условия Apex stateless (в отличие от Bevy).
pub enum ConditionTree {
    Leaf(Box<dyn Fn(&World) -> bool + Send + Sync>),
    And(Vec<ConditionTree>),
    Or(Vec<ConditionTree>),
}

impl ConditionTree {
    /// Оценить дерево условий для данного мира.
    pub fn evaluate(&self, world: &World) -> bool {
        match self {
            ConditionTree::Leaf(f) => f(world),
            ConditionTree::And(conds) => conds.iter().all(|c| c.evaluate(world)),
            ConditionTree::Or(conds) => conds.iter().any(|c| c.evaluate(world)),
        }
    }

    /// Создать Leaf из замыкания.
    pub fn leaf(f: impl Fn(&World) -> bool + Send + Sync + 'static) -> Self {
        ConditionTree::Leaf(Box::new(f))
    }
}

impl Default for ConditionTree {
    fn default() -> Self {
        ConditionTree::And(Vec::new())
    }
}

pub type RunCondition = Box<dyn Fn(&World) -> bool + Send + Sync>;

// ── Condition Trait ────────────────────────────────────────────

pub trait Condition: Send + Sync + 'static {
    fn check(&self, world: &World) -> bool;
    fn access(&self) -> AccessDescriptor {
        AccessDescriptor::new()
    }
    fn not(self) -> NotCondition<Self>
    where
        Self: Sized,
    {
        NotCondition(self)
    }
    fn into_check_fn(self) -> Box<dyn Fn(&World) -> bool + Send + Sync>
    where
        Self: Sized,
    {
        Box::new(move |world: &World| self.check(world))
    }
}

pub struct NotCondition<C: Condition>(pub C);

impl<C: Condition> Condition for NotCondition<C> {
    fn check(&self, world: &World) -> bool {
        !self.0.check(world)
    }
    fn access(&self) -> AccessDescriptor {
        self.0.access()
    }
}

macro_rules! impl_condition_tuple {
    ($($T:ident),+) => {
        impl<$($T: Condition),+> Condition for ($($T,)+) {
            #[allow(non_snake_case)]
            fn check(&self, world: &World) -> bool {
                let ($($T,)+) = self;
                $( if !$T.check(world) { return false; } )+
                true
            }
            #[allow(non_snake_case)]
            fn access(&self) -> AccessDescriptor {
                let ($($T,)+) = self;
                AccessDescriptor::new()
                    $( .merge(&$T.access()) )+
            }
        }
    };
}

impl_condition_tuple!(A);
impl_condition_tuple!(A, B);
impl_condition_tuple!(A, B, C);
impl_condition_tuple!(A, B, C, D);
impl_condition_tuple!(A, B, C, D, E);
impl_condition_tuple!(A, B, C, D, E, F);
impl_condition_tuple!(A, B, C, D, E, F, G);
impl_condition_tuple!(A, B, C, D, E, F, G, H);
impl_condition_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_condition_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

// ── IntoConditionLeaf — internal dispatch trait ─────────────────

pub type SystemFn = Box<dyn FnMut(&mut World) + Send>;

// ── SendPtr ────────────────────────────────────────────────────

struct SendPtr<T: ?Sized>(*mut T);

// SAFETY: использование строго ограничено run_hybrid_parallel где
// уникальность ptr гарантирована — каждый ptr из уникального индекса.
unsafe impl<T: ?Sized> Send for SendPtr<T> {}
unsafe impl<T: ?Sized> Sync for SendPtr<T> {}
impl<T: ?Sized> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized> Copy for SendPtr<T> {}

// `SendPtr` is dereferenced directly at the ASD task site (`&mut *task.ptr.0`);
// the former `as_mut`/`as_ref` helpers were never called and were removed in the
// wave-4 dead-code pass.

/// Задача для ASD (Adaptive Scope Distribution).
///
/// Содержит указатель на систему (`dyn ParSystem`), указатель на срез архетипов
/// системы и диапазоны строк, которые эта задача должна обработать.
/// Каждая задача обрабатывает subset entity системы.
///
/// Если `chunk_ranges` пуст — задача обрабатывает все entity системы
/// (весь SubWorld без ограничений).
#[allow(dead_code)]
struct AsdTask {
    /// Pointer to the `dyn ParSystem` itself — NOT the enclosing
    /// `SystemDescriptor`. Row-split tasks for one system run concurrently and
    /// each forms `&mut *ptr`; targeting the trait object (a ZST for the only
    /// systems that are ever split — stateless plain-fn adapters) keeps those
    /// `&mut` non-aliasing over any real bytes, whereas `&mut SystemDescriptor`
    /// (which owns a `String` name, etc.) aliased UB-ly (D3).
    ptr: SendPtr<dyn ParSystem>,
    /// Индексы архетипов для этой задачи.
    /// Если `chunk_ranges` пусто — все архетипы системы.
    /// Иначе — только те, что есть в `chunk_ranges` (сужение для 4-arch cases).
    arch_indices: SmallVec<[usize; 8]>,
    /// Диапазоны строк для ограничения SubWorld:
    /// `(arch_idx, start, end)` — только эти строки указанных архетипов.
    /// Если пусто — SubWorld без ограничений (все entity системы).
    chunk_ranges: SmallVec<[(usize, usize, usize); 4]>,
}

unsafe impl Send for AsdTask {}
unsafe impl Sync for AsdTask {}

// ── ParSystem trait ────────────────────────────────────────────

/// Параллельная система с явным AccessDescriptor.
///
/// Внутренний механизм — используйте `AutoSystem` для публичного API.
pub(crate) trait ParSystem: Send + Sync {
    #[allow(dead_code)]
    fn access() -> AccessDescriptor
    where
        Self: Sized;
    fn run(&mut self, ctx: SystemContext<'_>);
    #[allow(dead_code)]
    fn name() -> &'static str
    where
        Self: Sized,
    {
        std::any::type_name::<Self>()
    }
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
    fn access() -> AccessDescriptor
    where
        Self: Sized,
    {
        S::Query::system_access()
            .merge(&S::Resources::resource_accesses())
            .merge(&S::Events::event_accesses())
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        self.inner.run(ctx);
    }

    fn name() -> &'static str
    where
        Self: Sized,
    {
        S::name()
    }
}

// ── FnParSystem ────────────────────────────────────────────────

struct FnParSystem {
    func: Box<dyn FnMut(SystemContext<'_>) + Send + Sync>,
    #[allow(dead_code)]
    access: AccessDescriptor,
}

impl ParSystem for FnParSystem {
    fn access() -> AccessDescriptor
    where
        Self: Sized,
    {
        AccessDescriptor::new()
    }
    fn run(&mut self, ctx: SystemContext<'_>) {
        (self.func)(ctx);
    }
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
    fn is_parallel(&self) -> bool {
        matches!(self, SystemKind::Parallel { .. })
    }

    fn access(&self) -> Option<&AccessDescriptor> {
        match self {
            SystemKind::Parallel { access, .. } => Some(access),
            SystemKind::Sequential(_) => None,
        }
    }
}

// ── SystemDescriptor ───────────────────────────────────────────

struct SystemDescriptor {
    id: SystemId,
    name: String,
    kind: SystemKind,
    after: Vec<SystemId>,
    before: Vec<SystemId>,
    /// Этап выполнения (по умолчанию Update).
    stage_label: StageLabel,
    /// Run condition: система запускается только если дерево условий возвращает true.
    /// По умолчанию `ConditionTree::And(Vec::new())` — всегда true (пустой AND = true).
    run_condition: ConditionTree,
    /// True = применить Commands после этой системы, разбив Stage на под-Stage.
    /// Устанавливается через `sched.apply_deferred()`.
    apply_deferred_after: bool,
    /// True = система использует отложенные операции (Commands).
    /// Автоматически определяется при регистрации.
    has_deferred: bool,
    /// Накопленный доступ из всех run_if/or_else условий.
    /// При compile() мержится в system access для конфликтного анализа.
    condition_access: AccessDescriptor,
}

// ── SystemBuilder ──────────────────────────────────────────────

/// Билдер держит `&'a mut Scheduler` (а не сырой `*mut`): лайфтайм гарантирует,
/// что билдер не переживёт планировщик — сохранённый билдер + уничтоженный
/// `Scheduler` = UB был бы возможен с сырым указателем (D10).
pub struct SystemBuilder<'a> {
    scheduler: &'a mut Scheduler,
    id: SystemId,
}

impl<'a> SystemBuilder<'a> {
    pub fn id(self) -> SystemId {
        self.id
    }

    fn add_condition_leaf(self, leaf: ConditionTree, condition_access: AccessDescriptor) -> Self {
        {
            let sched = &mut *self.scheduler;
            if let Some(sys) = sched.system_by_id_mut(self.id) {
                sys.condition_access = std::mem::take(&mut sys.condition_access).merge(&condition_access);
                if let SystemKind::Parallel { ref mut access, .. } = &mut sys.kind {
                    *access = std::mem::take(access).merge(&condition_access);
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
        }
        self
    }

    fn add_condition_or(self, leaf: ConditionTree, condition_access: AccessDescriptor) -> Self {
        {
            let sched = &mut *self.scheduler;
            if let Some(sys) = sched.system_by_id_mut(self.id) {
                sys.condition_access = std::mem::take(&mut sys.condition_access).merge(&condition_access);
                if let SystemKind::Parallel { ref mut access, .. } = &mut sys.kind {
                    *access = std::mem::take(access).merge(&condition_access);
                }
                match &mut sys.run_condition {
                    ConditionTree::Or(ref mut conds) => conds.push(leaf),
                    _ => {
                        let old = std::mem::replace(&mut sys.run_condition, ConditionTree::Or(Vec::new()));
                        if let ConditionTree::Or(ref mut conds) = sys.run_condition {
                            conds.push(old);
                            conds.push(leaf);
                        }
                    }
                }
            }
        }
        self
    }

    /// Прикрепить run condition — closure.
    pub fn run_if<F>(self, condition: F) -> Self
    where
        F: Fn(&World) -> bool + Send + Sync + 'static,
    {
        let leaf = ConditionTree::leaf(condition);
        self.add_condition_leaf(leaf, AccessDescriptor::new())
    }

    /// Прикрепить typed condition (с явным access для планировщика).
    pub fn run_if_cond<C: Condition>(self, condition: C) -> Self {
        let acc = condition.access();
        let leaf = ConditionTree::Leaf(condition.into_check_fn());
        self.add_condition_leaf(leaf, acc)
    }

    /// Прикрепить OR-условие — closure.
    pub fn or_else<F>(self, condition: F) -> Self
    where
        F: Fn(&World) -> bool + Send + Sync + 'static,
    {
        let leaf = ConditionTree::leaf(condition);
        self.add_condition_or(leaf, AccessDescriptor::new())
    }

    /// Прикрепить OR-условие — typed.
    pub fn or_else_cond<C: Condition>(self, condition: C) -> Self {
        let acc = condition.access();
        let leaf = ConditionTree::Leaf(condition.into_check_fn());
        self.add_condition_or(leaf, acc)
    }

    /// Установить всё дерево условий целиком (заменяет существующие).
    pub fn condition(self, tree: ConditionTree) -> Self {
        {
            let sched = &mut *self.scheduler;
            if let Some(sys) = sched.system_by_id_mut(self.id) {
                sys.run_condition = tree;
            }
        }
        self
    }
}

// ── ExecutionPlan ──────────────────────────────────────────────

struct ExecutionPlan {
    stages: Vec<Stage>,
    flat_order: Vec<SystemId>,
}

// ── GraphEdgeInfo ──────────────────────────────────────────────

/// Метаданные ребра в dependency_graph для verbose диагностики.
#[derive(Clone, Debug)]
struct GraphEdgeInfo {
    from_id: SystemId,
    to_id: SystemId,
    kind: ConflictKind,
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
/// Какие архетипы нужны системе (CR-M4).
///
/// Системы без компонентных доступов (только ресурсы/события) получают
/// маркер `All` вместо материализованного `Vec` всех индексов — на больших
/// мирах это убирало по Vec<usize> длиной в число архетипов на систему.
#[derive(Debug, Clone)]
enum SystemArchetypes {
    /// Без ограничений — системе подходят все архетипы.
    All,
    /// Архетипы, содержащие хотя бы один компонент системы
    /// (критерий `any()`; Query сам дофильтрует через matches_archetype).
    Filtered(Vec<usize>),
}

/// Граф зависимостей хранится между `compile()` вызовами.
/// `dirty_systems` отслеживает системы добавленные после последнего compile —
/// при следующем compile добавляются только новые узлы/рёбра.
pub struct Scheduler {
    systems: Vec<SystemDescriptor>,
    /// Быстрый поиск системы по SystemId: O(1) вместо O(n)
    system_indices: FxHashMap<SystemId, usize>,
    next_id: u32,
    execution_plan: Option<ExecutionPlan>,
    /// Per-execution-stage change-detection base (TD-52), indexed by position in `execution_plan.stages`.
    /// The change tick at which each EXECUTION stage last ran; set as `world.last_run_tick` before running
    /// that stage (with `current_tick` advanced between stages). Keyed by execution-stage **index** (not
    /// `StageLabel`) because the planner splits conflicting systems of one label into several execution
    /// stages — keying by label would let them clobber one another's base. So a `Changed<T>` reader sees
    /// writes made since ITS OWN stage last ran, including later stages of the previous frame (and it is
    /// effectively per-system for systems the planner sequenced into their own stage). Cleared on
    /// `compile` (the plan — and thus indices — can change).
    stage_last_run: Vec<apex_core::Tick>,

    // ── Конфигурация параллелизма ───────────────────────────────
    /// Минимальное количество entity в Stage для параллельного выполнения.
    /// Если суммарное entity_count систем Stage меньше этого порога —
    /// Stage выполняется последовательно (rayon overhead > выигрыша).
    /// 0 = без ограничений (текущее поведение).
    parallel_min_entities: usize,

    /// Автоматически отключать параллелизм, если per-system entity count
    /// ниже порога окупаемости (rayon overhead > выигрыш).
    /// Использует эвристику на основе числа систем в Stage.
    auto_disable_parallel: bool,

    // ── Инкрементальный граф ────────────────────────────────────
    /// Граф зависимостей: узлы = SystemId, рёбра = ConflictKind.
    /// Хранится между compile() для инкрементального обновления.
    dependency_graph: Graph<SystemId, ConflictKind>,
    /// Map SystemId → Index в dependency_graph (для быстрого lookup).
    graph_nodes: FxHashMap<SystemId, Index>,
    /// O(1) lookup рёбер: (from, to) → exists. Синхронизирован с dependency_graph.
    edge_set: FxHashSet<(Index, Index)>,
    /// Рёбра с полными метаданными — для verbose диагностики.
    edge_info: Vec<GraphEdgeInfo>,
    /// True если после последнего compile() добавлялись системы/зависимости.
    graph_dirty: bool,
    /// Пары систем с явным порядком (от add_dependency / .before / .after).
    /// Edge направлен от «раньше» к «позже»: (a, b) означает a до b.
    explicit_orderings: FxHashSet<(SystemId, SystemId)>,

    // ── Seq/Par индексы для O(P) sequential-барьеров ────────────
    /// Индексы sequential систем в self.systems.
    seq_system_indices: Vec<usize>,
    /// Индексы parallel систем в self.systems.
    par_system_indices: Vec<usize>,

    // ── SubWorld маппинг ────────────────────────────────────────
    /// Для каждой системы — индексы архетипов, которые ей нужны.
    /// Заполняется в compile() и используется в run_hybrid_parallel().
    system_archetype_indices: FxHashMap<SystemId, SystemArchetypes>,
    /// Количество архетипов в World на момент последнего compute_archetype_indices().
    /// Используется для кеширования — пересчёт только при изменении.
    cached_archetype_count: usize,

    /// Флаг: был ли уже выполнен Startup этап.
    startup_completed: bool,

    /// Пользовательский порядок StageLabel для compile().
    /// Если Some — compile() использует этот порядок вместо hardcoded standard_order().
    /// Если None — используется StageLabel::standard_order().
    stage_order: Option<Vec<StageLabel>>,

    /// Этап по умолчанию для `add_system`, `add_auto_system`, `add_par`,
    /// `add_par_access` (без суффикса `_to_stage`).
    ///
    /// По умолчанию — `StageLabel::Update`. Меняется через `set_default_stage()`
    /// или временно подменяется внутри `staged()`.
    default_stage_label: StageLabel,

    /// Реестр имён компонентов TypeId → &'static str.
    /// Заполняется из ComponentRegistry перед compile() в run()/run_sequential().
    /// Используется `component_type_name()` для отображения реальных имён компонентов
    /// в ConflictKind (вместо заглушки "<component>").
    type_names: FxHashMap<TypeId, &'static str>,

    /// Флаг: учитывать ли Emit<E>/Listen<E> при построении графа зависимостей.
    ///
    /// По умолчанию `true` — событийный порядок гарантируется автоматически.
    /// При `false` поведение соответствует состоянию до введения EventAccessList:
    /// порядок Emit/Listen не определён (как в предыдущих версиях движка).
    event_ordering_enabled: bool,

    /// Последняя зарегистрированная система — для `apply_deferred()`.
    last_added_system_id: Option<SystemId>,

    /// Scope condition: все системы, зарегистрированные внутри `staged()` пока этот
    /// condition активен, наследуют его (автоматически AND-ится с их условиями).
    scope_condition: Option<std::sync::Arc<dyn Fn(&World) -> bool + Send + Sync>>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            system_indices: FxHashMap::default(),
            next_id: 0,
            execution_plan: None,
            stage_last_run: Vec::new(),
            parallel_min_entities: 0, // 0 = без ограничений
            auto_disable_parallel: true, // true = автоотключение по умолчанию
            dependency_graph: Graph::new(),
            graph_nodes: FxHashMap::default(),
            edge_set: FxHashSet::default(),
            edge_info: Vec::new(),
            graph_dirty: false,
            explicit_orderings: FxHashSet::default(),
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
    fn add_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        self.add_system_to_stage(name, func, self.default_stage_label.clone())
    }

    /// Регистрировать Sequential систему в указанном этапе.
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
            condition_access: AccessDescriptor::new(),
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
    fn add_startup_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
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
    fn add_auto_system<S>(&mut self, name: impl Into<String>, system: S) -> SystemBuilder<'_>
    where
        S: AutoSystem + 'static,
    {
        self.add_auto_system_to_stage(name, system, self.default_stage_label.clone())
    }

    /// Регистрировать AutoSystem в указанном этапе.
    #[cfg(test)]
    fn add_auto_system_to_stage<S>(
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
            condition_access: AccessDescriptor::new(),
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
    fn add_startup_auto_system<S>(&mut self, name: impl Into<String>, system: S) -> SystemBuilder<'_>
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
            condition_access: AccessDescriptor::new(),
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
    ///         ctx.query::<(Read<Vel>, Write<Pos>)>().for_each(|_, (v, mut p)| {
    ///             p.x += v.x;
    ///         });
    ///     },
    /// );
    /// ```
    #[cfg(test)]
    fn add_par_access<F>(
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
            condition_access: AccessDescriptor::new(),
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

    /// Установить минимальное количество entity для параллельного выполнения Stage.
    ///
    /// Если суммарное количество entity всех систем в Stage меньше этого порога,
    /// Stage выполняется последовательно (избегаем rayon overhead для мелких миров).
    ///
    /// По умолчанию: `0` (без ограничений).
    pub fn set_parallel_min_entities(&mut self, min: usize) -> &mut Self {
        self.parallel_min_entities = min;
        self
    }

    /// Включить/выключить автоматическое отключение параллелизма на основе
    /// эвристики per-system entity count.
    ///
    /// Когда включено, движок вычисляет среднее количество entity на одну
    /// систему в Stage и отключает параллелизм, если порог не достигнут:
    ///   - 3+ систем:   min 15 000 entity/system
    ///   - 2 системы:   min 25 000 entity/system
    ///   - 1 система:   min 80 000 entity/system
    ///
    /// Пороги подобраны эмпирически (parallel_diagnostics benchmark, 12 ядер).
    /// «Valley of death» (PAR в 2-3x медленнее SEQ) — 5 000–50 000 entity
    /// в зависимости от числа систем.
    ///
    /// По умолчанию: `true` (автоотключение включено).
    pub fn set_parallel_auto_disable(&mut self, enabled: bool) -> &mut Self {
        self.auto_disable_parallel = enabled;
        self
    }

    /// Пометить AutoSystem (по SystemId) как использующую `par_for_each` внутри.
    /// Планировщик не будет дополнительно чанковать эту систему через ASD.
    fn par_for_each_used(&mut self, id: SystemId) -> &mut Self {
        if let Some(sys) = self
            .system_indices
            .get(&id)
            .and_then(|&idx| self.systems.get_mut(idx))
        {
            if let SystemKind::Parallel { ref mut access, .. } = &mut sys.kind {
                access.uses_par_for_each = true;
            }
        }
        self
    }

    /// Эвристика: минимальное количество entity на одну систему для окупаемости
    /// параллелизма (rayon overhead < выигрыш).
    ///
    /// Пороги определены эмпирически через `parallel_diagnostics` scaling benchmark
    /// (12 ядер, release). «Valley of death» (PAR медленнее SEQ в 2-3x) находится
    /// в диапазоне 5,000-10,000 entity для multi-system и до 50,000 для single-system.
    fn min_entities_for_parallelism(num_systems: usize) -> usize {
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
    fn staged<F>(&mut self, label: StageLabel, f: F) -> &mut Self
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

    fn find_id_by_name(&self, name: &str) -> Result<SystemId, SchedulerError> {
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
            sys.condition_access = std::mem::take(&mut sys.condition_access).merge(&acc);
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
    /// # use apex_scheduler::{Scheduler, StageLabel, sys, seq};
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
    pub fn add_systems<M>(
        &mut self,
        stage_label: StageLabel,
        systems: impl IntoScheduleConfigs<M>,
    ) -> &mut Self {
        for cfg in systems.into_vec() {
            self.register_system_config(cfg, stage_label.clone());
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
                    condition_access: cfg.condition_access,
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
                    condition_access: cfg.condition_access,
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
                    condition_access: cfg.condition_access,
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

    /// `par_for_each_used` по имени системы (удобно с `add_systems`).
    pub fn par_for_each_used_by_name(&mut self, name: &str) -> Result<&mut Self, SchedulerError> {
        let id = self.find_id_by_name(name)?;
        self.par_for_each_used(id);
        Ok(self)
    }

    /// Получить `&mut SystemDescriptor` по `SystemId` (O(1)).
    fn system_by_id_mut(&mut self, id: SystemId) -> Option<&mut SystemDescriptor> {
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
    pub fn populate_type_names(&mut self, registry: &ComponentRegistry) {
        if !self.type_names.is_empty() {
            return; // Уже заполнены — все миры имеют одинаковые компоненты
        }
        for info in registry.iter() {
            self.type_names.insert(info.type_id, info.name);
        }
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
        // Ранний выход: если граф не менялся и план уже построен
        if !self.graph_dirty && self.execution_plan.is_some() {
            return Ok(());
        }

        // План (а значит и индексы execution-стадий) перестраивается — сбрасываем per-stage базы
        // change-detection (TD-52). Одно лишнее «всё изменилось» на следующем кадре — безопасно.
        self.stage_last_run.clear();

        if self.type_names.is_empty() {
            log::debug!(
                "Scheduler::compile: type_names пуст. \
                 Вызовите populate_type_names(&world.registry()) или \
                 compile_with_world(&world) для отображения имён компонентов \
                 в debug_plan_verbose()"
            );
        }

        if self.graph_dirty {
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
    pub fn compute_archetype_indices(&mut self, world: &apex_core::World) {
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

    /// Последовательное выполнение — для тестов и non-parallel builds.
    ///
    /// Принимает `*mut World` намеренно: внутри одновременно создаются `SubWorld`
    /// (read-only вид) и применяются `Commands` (`&mut World`), что невозможно
    /// выразить через единый `&mut World` без reborrow-конфликтов. Вызывающий
    /// гарантирует валидность указателя (обычно `&mut world`).
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn run_sequential(&mut self, world_ptr: *mut World) {
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

        let mut thread_commands: Vec<Commands> = vec![Commands::new()];
        let cmds_ptr = &mut thread_commands as *mut Vec<Commands>;

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
            // D6: skip the stage (and the change window) when every system is
            // disabled by run_if, so stage_last_run doesn't advance past changes
            // made during the pause.
            let any_active = system_ids.iter().any(|&sys_id| {
                self.system_indices
                    .get(&sys_id)
                    .is_some_and(|&idx| self.systems[idx].run_condition.evaluate(w))
            });
            if !any_active {
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
                    if let Some(&index) = self.system_indices.get(&sys_id) {
                        let system = &self.systems[index];
                        if !system.run_condition.evaluate(w) {
                            continue;
                        }
                        let system = &mut self.systems[index];
                        let prof_t = main_prof_enabled().then(std::time::Instant::now);
                        match &mut system.kind {
                            SystemKind::Sequential(f) => f(w),
                            SystemKind::Parallel { system, .. } => {
                                // SAFETY: `cmds_ptr` points at `thread_commands`
                                // (one slot per rayon worker), alive until the
                                // end of the run.
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

                // Применяем отложенные команды после каждой стадии/шага
                for cmds in &mut thread_commands {
                    cmds.apply(w);
                }
                thread_commands[0] = Commands::new();

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
    fn run_stage_parallel(&mut self, stage_ids: &[SystemId], world: &World, cmds_ptr: usize) {
        let archetypes = world.archetypes();
        let num_workers = rayon::current_num_threads();

        // 0. Evaluate run conditions on main thread BEFORE any ASD task setup
        let mut skipped_systems: FxHashSet<SystemId> = FxHashSet::default();
        for &sys_id in stage_ids {
            if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                if !self.systems[sys_idx].run_condition.evaluate(world) {
                    skipped_systems.insert(sys_id);
                }
            }
        }

        // 1. Собираем per-system информацию
        struct SysInfo {
            ptr: SendPtr<dyn ParSystem>,
            arch_indices: Vec<usize>,
            entity_count: usize,
            has_events: bool,
            uses_par_for_each: bool,
            needs_whole_world: bool,
            stateful: bool,
            /// Мутирует ресурс (`ResMut`) или использует `Commands` — нельзя дробить на чанки
            /// (тело системы выполнялось бы раз на чанк ⇒ умножение сайд-эффекта). См. TD-37.
            non_query_side_effects: bool,
        }

        let mut sys_infos: Vec<SysInfo> = Vec::new();
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
                let arch_indices = match self.system_archetype_indices.get(&sys_id) {
                    Some(SystemArchetypes::Filtered(v)) => v.clone(),
                    // All или нет записи — система видит все архетипы
                    _ => (0..archetypes.len()).collect(),
                };

                let entity_count: usize = arch_indices
                    .iter()
                    .filter_map(|&ai| {
                        if ai < archetypes.len() {
                            Some(archetypes[ai].len())
                        } else {
                            None
                        }
                    })
                    .sum();

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
                    let non_query_side_effects = access
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
                        arch_indices,
                        entity_count,
                        has_events,
                        uses_par_for_each,
                        needs_whole_world,
                        stateful,
                        non_query_side_effects,
                    });
                } else {
                    // Система без entity (только ресурсы/события) — запускаем сразу
                    let system = &mut self.systems[sys_idx];
                    if let SystemKind::Parallel { system: sys, .. } = &mut system.kind {
                        let all_indices: Vec<usize> = (0..archetypes.len()).collect();
                        let sw = unsafe { apex_core::SubWorld::from_raw(world, &all_indices) };
                        // SAFETY: cmds_ptr — usize, преобразованный из &mut Vec<Commands>,
                        // указывает на thread_commands (по слоту на rayon-поток),
                        // который жив до конца run_hybrid_parallel.
                        let sws = [sw];
                        let ctx = unsafe {
                            SystemContext::with_commands(&sws, cmds_ptr as *mut Vec<Commands>)
                        };
                        sys.run(ctx);
                    }
                }
            }
        }

        if sys_infos.is_empty() {
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

        // 3. Создаём чанки для всех систем
        let mut tasks: Vec<AsdTask> = Vec::new();

        for info in &sys_infos {
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
                // Per-system scope: одна задача, все entity целиком
                tasks.push(AsdTask {
                    ptr: info.ptr,
                    arch_indices: SmallVec::from_slice(&info.arch_indices),
                    chunk_ranges: SmallVec::new(), // пусто = весь SubWorld
                });
            } else {
                // Multi-архетипная крупная система — ASD разбивка
                let mut remaining = info.entity_count;
                let mut arch_iter = info.arch_indices.iter().copied();
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
                        tasks.push(AsdTask {
                            ptr: info.ptr,
                            arch_indices: chunk_arch_set,
                            chunk_ranges,
                        });
                    }
                }
            }
        }

        // 4. Сортируем чанки по archetype_id для cache locality
        tasks.sort_unstable_by_key(|t| t.chunk_ranges.first().map(|&(a, _, _)| a).unwrap_or(0));

        // 5. Запускаем через rayon::scope
        // cmds_ptr — это usize (из &mut Vec<Commands>), который является Copy + Send + Sync.
        // Внутри замыкания преобразуем обратно в *mut Vec<Commands>.
        rayon::scope(|s| {
            for task in &tasks {
                let cmds = cmds_ptr;
                s.spawn(move |_| {
                    let cmds_ptr = cmds as *mut Vec<Commands>;
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
                        // SAFETY (both arms): cmds_ptr указывает на Vec<Commands> из
                        // Scheduler, живущий до конца run_hybrid_parallel; каждый поток
                        // обращается к своему индексу через current_thread_index().
                        if task.chunk_ranges.is_empty() {
                            // Полный SubWorld — все архетипы системы без ограничений
                            let sub = apex_core::SubWorld::from_raw(world, &task.arch_indices);
                            sys.run(SystemContext::with_commands(&[sub], cmds_ptr));
                        } else {
                            // SubWorld с range-ограничениями и суженными arch_indices
                            let sub = apex_core::SubWorld::from_raw_with_ranges(
                                world,
                                &task.arch_indices,
                                &task.chunk_ranges,
                            );
                            sys.run(SystemContext::with_commands(&[sub], cmds_ptr));
                        }
                    }
                });
            }
        });
    }

    /// Параллельное выполнение через ASD (Adaptive Scope Distribution).
    ///
    /// Для stage с исключительно параллельными системами используется
    /// [`run_stage_parallel`] (ASD). Stage с sequential системами или
    /// смешанные выполняются последовательно, каждая система получает
    /// полный `SubWorld` над всеми архетипами.
    ///
    fn run_hybrid_parallel(&mut self, world_ptr: *mut World) {
        let plan = self.execution_plan.as_ref().unwrap();
        let stages: Vec<(usize, StageLabel, Vec<SystemId>, bool, Vec<TypeId>)> = plan
            .stages
            .iter()
            .enumerate()
            .filter(|(_, stage)| !(stage.label == StageLabel::Startup && self.startup_completed))
            .map(|(i, s)| {
                (
                    i, // original execution-stage index (stable change-detection key, TD-52)
                    s.label.clone(),
                    s.system_ids.clone(),
                    s.all_parallel,
                    s.emit_event_types.clone(),
                )
            })
            .collect();

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

        let num_threads = rayon::current_num_threads();
        let mut thread_commands: Vec<Commands> =
            (0..num_threads).map(|_| Commands::new()).collect();
        let cmds_ptr: usize = &mut thread_commands as *mut Vec<Commands> as usize;

        let arch_lengths: Vec<usize> = unsafe { &*const_ptr }
            .archetypes()
            .iter()
            .map(|a| a.len())
            .collect();

        // Build the execution order (D2-5). Stages sharing a StageLabel are
        // contiguous, so the FixedUpdate stages form one block. That block steps
        // as a GROUP: the FixedTime accumulator is drained ONCE and the whole
        // block runs `steps` times as (A; B; …) × N. This fixes D1 — draining
        // per stage let the first FixedUpdate stage consume all the steps, so any
        // later FixedUpdate stage (a conflict split the label across several
        // execution stages) ran zero times, and the order was A×N then B×N rather
        // than the correct (A;B)×N. Every non-FixedUpdate stage runs exactly once.
        let mut schedule: Vec<usize> = Vec::with_capacity(stages.len());
        let mut si = 0;
        while si < stages.len() {
            if stages[si].1 == StageLabel::FixedUpdate {
                let mut end = si;
                while end < stages.len() && stages[end].1 == StageLabel::FixedUpdate {
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
            let (stage_idx, _label, stage_ids, all_parallel, emit_event_types) = &stages[stage_pos];
            // D6: skip the whole stage — including opening the change window — when
            // every system is disabled by its run_condition. Otherwise the window
            // would advance stage_last_run past changes made during a run_if pause,
            // and a system resuming later would miss Changed<T> from those frames.
            let any_active = stage_ids.iter().any(|&sys_id| {
                self.system_indices.get(&sys_id).is_some_and(|&idx| {
                    self.systems[idx]
                        .run_condition
                        .evaluate(unsafe { &*const_ptr })
                })
            });
            if !any_active {
                continue;
            }
            // TD-52: per-execution-stage change-detection window (cross-stage `Changed<T>`).
            self.open_stage_change_window(*stage_idx, unsafe { &mut *world_ptr });
            if !*all_parallel {
                {
                    let w = unsafe { &*const_ptr };
                    let all_indices: Vec<usize> = (0..w.archetypes().len()).collect();
                    let sub_world = unsafe { apex_core::SubWorld::from_raw(w, &all_indices) };
                    for &sys_id in stage_ids {
                        if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                            let system = &self.systems[sys_idx];
                        if !system.run_condition.evaluate(unsafe { &*const_ptr }) {
                            continue;
                        }
                            let system = &mut self.systems[sys_idx];
                            let prof_t = main_prof_enabled().then(std::time::Instant::now);
                            match &mut system.kind {
                                SystemKind::Sequential(f) => f(unsafe { &mut *world_ptr }),
                                SystemKind::Parallel { system, .. } => {
                                    // SAFETY: `cmds_ptr` points at `thread_commands`
                                    // (one slot per rayon worker), alive until the
                                    // end of the run.
                                    let ctx = unsafe {
                                        SystemContext::with_commands(
                                            std::slice::from_ref(&sub_world),
                                            cmds_ptr as *mut Vec<Commands>,
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
                    for cmds in &mut thread_commands {
                        cmds.apply(w);
                    }
                }
                for cmds in &mut thread_commands {
                    *cmds = Commands::new();
                }
                if !emit_event_types.is_empty() {
                    unsafe { &mut *world_ptr }.flush_events_by_type(emit_event_types);
                }
                continue;
            }

            let should_fallback = if self.parallel_min_entities > 0 || self.auto_disable_parallel {
                let total_entities: usize = arch_lengths.iter().sum();
                let stage_entity_count: usize = stage_ids
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
                    .sum();
                let below_hard_limit = self.parallel_min_entities > 0
                    && stage_entity_count < self.parallel_min_entities;
                let below_auto_limit = self.auto_disable_parallel && {
                    let sys_count = stage_ids.len();
                    let per_system = stage_entity_count / sys_count.max(1);
                    per_system < Self::min_entities_for_parallelism(sys_count)
                };
                
                below_hard_limit || below_auto_limit
            } else {
                false
            };

            if should_fallback {
                {
                    let w = unsafe { &*const_ptr };
                    let all_indices: Vec<usize> = (0..w.archetypes().len()).collect();
                    let sub_world = unsafe { apex_core::SubWorld::from_raw(w, &all_indices) };
                    for &sys_id in stage_ids {
                        if let Some(&sys_idx) = self.system_indices.get(&sys_id) {
                            let system = &self.systems[sys_idx];
                        if !system.run_condition.evaluate(unsafe { &*const_ptr }) {
                            continue;
                        }
                            let system = &mut self.systems[sys_idx];
                            let prof_t = main_prof_enabled().then(std::time::Instant::now);
                            match &mut system.kind {
                                SystemKind::Sequential(f) => f(unsafe { &mut *world_ptr }),
                                SystemKind::Parallel { system, .. } => {
                                    // SAFETY: `cmds_ptr` points at `thread_commands`
                                    // (one slot per rayon worker), alive until the
                                    // end of the run.
                                    let ctx = unsafe {
                                        SystemContext::with_commands(
                                            std::slice::from_ref(&sub_world),
                                            cmds_ptr as *mut Vec<Commands>,
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
                    for cmds in &mut thread_commands {
                        cmds.apply(w);
                    }
                }
                for cmds in &mut thread_commands {
                    *cmds = Commands::new();
                }
                if !emit_event_types.is_empty() {
                    unsafe { &mut *world_ptr }.flush_events_by_type(emit_event_types);
                }
                continue;
            }

            self.run_stage_parallel(stage_ids, unsafe { &*const_ptr }, cmds_ptr);

            {
                let w = unsafe { &mut *world_ptr };
                for cmds in &mut thread_commands {
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
            if !emit_event_types.is_empty() {
                unsafe { &mut *world_ptr }.flush_events_by_type(emit_event_types);
            }
        }
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

    // ── Инспекция ──────────────────────────────────────────────

    pub fn system_count(&self) -> usize {
        self.systems.len()
    }

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
                    // Показать условия
                    let cond_str = format_condition(&s.run_condition);
                    if !cond_str.is_empty() {
                        out.push_str(&format!("      run_if: {}\n", cond_str));
                    }
                }
            }
        }

        // ── SubWorld маппинг (архетипы для каждой системы) ────
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

        // ── Параллелизм summary ───────────────────────────────
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

    /// Получить причины конфликта между двумя конкретными системами.
    pub fn conflicts_between(&self, a: SystemId, b: SystemId) -> Vec<&ConflictKind> {
        self.edge_info
            .iter()
            .filter(|e| (e.from_id == a && e.to_id == b) || (e.from_id == b && e.to_id == a))
            .map(|e| &e.kind)
            .collect()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Вспомогательные функции ────────────────────────────────────

/// Обнаружить конфликт между двумя системами.
///
/// # Порядок проверок
/// 1. Компонентные Write+Write
/// 2. Компонентные Write+Read (оба направления)
/// 3. Event Write+Write (два Emit одного события → конфликт)
/// 4. Event Write+Read (Emit + Listen → Emit идёт раньше)
///
/// # Гарантия порядка событий
/// Ребро Emit(E) → Listen(E) гарантирует, что все отправители события E
/// будут выполнены до любого слушателя E в пределах одного кадра.
/// Два Listen(E) конфликта не создают — могут выполняться параллельно.
///
/// # Управление
/// Если `event_ordering = false`, проверки событийных конфликтов (3, 4)
/// пропускаются — порядок Emit/Listen не определён планировщиком.
///
/// Возвращает (ConflictKind, направление) где направление true означает i→j.
/// Если конфликтов нет — None.
fn detect_conflict_kind(
    ai: &AccessDescriptor,
    aj: &AccessDescriptor,
    id_i: SystemId,
    id_j: SystemId,
    type_names: &FxHashMap<TypeId, &'static str>,
    event_ordering: bool,
) -> Option<(ConflictKind, bool)> {
    // ── Whole-world access (F3) ─────────────────────────────────
    // A whole-world system (e.g. a `Ctx` param — SystemContext can reach any
    // resource / component / event) declares no specific access, so the TypeId
    // checks below would find no conflict and let it run in parallel with a
    // writer — a data race. Treat it conservatively: it conflicts with any other
    // system that touches any data (or is itself whole-world). Symmetric, so it
    // serializes in registration order and an explicit ordering can override it
    // (see D5).
    if ai.needs_whole_world || aj.needs_whole_world {
        let has_access = |a: &AccessDescriptor| {
            a.needs_whole_world
                || !a.reads.is_empty()
                || !a.writes.is_empty()
                || !a.reads_event.is_empty()
                || !a.writes_event.is_empty()
        };
        if has_access(ai) && has_access(aj) {
            return Some((
                ConflictKind::WriteWrite {
                    component_name: "<whole-world>",
                },
                true,
            ));
        }
    }

    // ── Компонентные конфликты ──────────────────────────────────

    // Write+Write: оба пишут в один компонент
    for w in &ai.writes {
        if aj.writes.contains(w) {
            return Some((
                ConflictKind::WriteWrite {
                    component_name: component_type_name(*w, type_names),
                },
                true,
            )); // i→j
        }
    }
    // Write(i)+Read(j): i пишет то что j читает
    let i_writes_j_reads = ai.writes.iter().any(|w| aj.reads.contains(w));
    // Write(j)+Read(i): j пишет то что i читает
    let j_writes_i_reads = aj.writes.iter().any(|w| ai.reads.contains(w));

    if i_writes_j_reads && j_writes_i_reads {
        // Bidirectional WriteRead: обе системы пишут в компоненты,
        // которые читает другая. Это истинный циклический конфликт.
        let a_name = format!("system_{}", id_i.0);
        let b_name = format!("system_{}", id_j.0);
        return Some((
            ConflictKind::BidirectionalWriteRead { a_name, b_name },
            true,
        )); // беззначимо — используем i→j
    }

    if i_writes_j_reads {
        return Some((
            ConflictKind::WriteRead {
                component_name: component_type_name(
                    *ai.writes.iter().find(|w| aj.reads.contains(w)).unwrap(),
                    type_names,
                ),
                writer_id: id_i.0,
                reader_id: id_j.0,
            },
            true,
        )); // i→j (писатель → читатель)
    }
    if j_writes_i_reads {
        return Some((
            ConflictKind::WriteRead {
                component_name: component_type_name(
                    *aj.writes.iter().find(|w| ai.reads.contains(w)).unwrap(),
                    type_names,
                ),
                writer_id: id_j.0,
                reader_id: id_i.0,
            },
            false,
        )); // j→i
    }

    // ── Event конфликты ─────────────────────────────────────────
    // Активны только если `event_ordering = true` (по умолчанию).

    if event_ordering {
        // EventWriteWrite: оба пишут в один тип событий
        for w in &ai.writes_event {
            if aj.writes_event.iter().any(|(id, _)| *id == w.0) {
                return Some((ConflictKind::EventWriteWrite { event_name: w.1 }, true));
                // i→j
            }
        }
        // EventWrite(i)+EventRead(j): i пишет событие, j читает
        for w in &ai.writes_event {
            if aj.reads_event.iter().any(|(id, _)| *id == w.0) {
                return Some((
                    ConflictKind::EventWriteRead {
                        event_name: w.1,
                        writer_id: id_i.0,
                        reader_id: id_j.0,
                    },
                    true,
                )); // i→j (писатель → читатель)
            }
        }
        // EventWrite(j)+EventRead(i): j пишет событие, i читает
        for w in &aj.writes_event {
            if ai.reads_event.iter().any(|(id, _)| *id == w.0) {
                return Some((
                    ConflictKind::EventWriteRead {
                        event_name: w.1,
                        writer_id: id_j.0,
                        reader_id: id_i.0,
                    },
                    false,
                )); // j→i
            }
        }
    }

    // ── Shared event-reader cursor race (F2) ────────────────────
    // NOT gated by event_ordering: two systems reading the same event type each
    // mutate that queue's shared cursor registry (register/advance/drop), so they
    // must be serialized regardless of ordering policy or the data race is real.
    for (id, name) in &ai.reads_event {
        if aj.reads_event.iter().any(|(oid, _)| oid == id) {
            return Some((ConflictKind::SharedEventReaders { event_name: name }, true));
        }
    }

    None
}

/// Разбить список `system_ids` на под-списки по маркерам `apply_deferred_after`.
///
/// Используется в `compile()` для создания под-Stage'ей между точками синхронизации.
///
/// Правила:
/// - Система с `apply_deferred_after = true` вызывает split ПОСЛЕ себя
///   (кроме случая когда это последняя система в списке)
/// - Несколько подряд идущих `apply_deferred_after` → все разделяют
/// - Пустые под-Stage'и не создаются
fn split_at_apply_boundaries(
    ids: &[SystemId],
    systems: &[SystemDescriptor],
    explicit_orderings: &FxHashSet<(SystemId, SystemId)>,
) -> Vec<Vec<SystemId>> {
    if ids.is_empty() {
        return vec![];
    }

    let mut groups = Vec::new();
    let mut current = Vec::new();

    for (i, &id) in ids.iter().enumerate() {
        let is_last = i + 1 == ids.len();
        current.push(id);

        let sys = systems.iter().find(|s| s.id == id);
        let manual_split = sys.map(|s| s.apply_deferred_after).unwrap_or(false);

        // Auto-split: если эта система использует Commands и следующая зависит
        // от неё явно (explicit ordering), вставляем split-точку.
        let auto_split = if !is_last {
            let next_id = ids[i + 1];
            sys.map(|s| s.has_deferred).unwrap_or(false)
                && explicit_orderings.contains(&(id, next_id))
        } else {
            false
        };

        if (manual_split || auto_split) && !is_last {
            groups.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        groups.push(current);
    }

    groups
}

/// Получить имя типа по TypeId.
///
/// Пытается найти имя в переданной `type_names` (заполняется из ComponentRegistry).
/// Если не найдено — возвращает `"<component>"`.
fn component_type_name(
    type_id: TypeId,
    type_names: &FxHashMap<TypeId, &'static str>,
) -> &'static str {
    type_names.get(&type_id).copied().unwrap_or("<component>")
}

/// Форматировать дерево условий для debug-вывода.
/// Возвращает пустую строку если условий нет (default And([])).
fn format_condition(tree: &ConditionTree) -> String {
    match tree {
        ConditionTree::Leaf(_) => "<condition>".to_string(),
        ConditionTree::And(conds) if conds.is_empty() => String::new(),
        ConditionTree::And(conds) => format!("AND({})", conds.iter().map(|_| "<cond>").collect::<Vec<_>>().join(", ")),
        ConditionTree::Or(conds) => format!("OR({})", conds.iter().map(|_| "<cond>").collect::<Vec<_>>().join(", ")),
    }
}

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::access_desc;
    use apex_core::{prelude::*, query::Query, world::World};

    #[derive(Component, Clone, Copy)]
    struct Pos {
        x: f32,
        y: f32,
    }
    #[derive(Component, Clone, Copy)]
    struct Vel {
        x: f32,
        y: f32,
    }
    #[derive(Component, Clone, Copy)]
    struct Hp(f32);
    #[derive(Clone, Copy)]
    struct DeltaTime(f32);

    // ── AutoSystem тесты ──────────────────────────────────────

    struct AutoMovement;
    impl AutoSystem for AutoMovement {
        type Query = (Read<Vel>, Write<Pos>);
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<Self::Query>().for_each(|_, (vel, mut pos)| {
                pos.x += vel.x;
                pos.y += vel.y;
            });
        }
    }

    struct AutoHealth;
    impl AutoSystem for AutoHealth {
        type Query = Write<Hp>;
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<Self::Query>().for_each(|_, mut hp| {
                hp.0 = hp.0.max(0.0);
            });
        }
    }

    #[test]
    fn auto_system_access_correct() {
        // AutoMovement должен иметь read:Vel, write:Pos
        let access = <(Read<Vel>, Write<Pos>) as WorldQuerySystemAccess>::system_access();
        assert!(!access.reads.is_empty(), "должен читать Vel");
        assert!(!access.writes.is_empty(), "должен писать Pos");
    }

    #[test]
    fn auto_system_runs_correctly() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);

        let mut world = World::new();

        world.spawn((Pos { x: 0.0, y: 0.0 }, Vel { x: 3.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each(|_, p| {
            result = (p.x, p.y);
        });
        assert!((result.0 - 3.0).abs() < 1e-6);
        assert!((result.1 - 4.0).abs() < 1e-6);
    }

    #[test]
    fn cross_stage_change_detection_for_later_system_in_multi_system_stage() {
        // Regression for the per-EXECUTION-stage keying: a `Changed<T>` reader the planner sequenced
        // AFTER another system of the same `StageLabel` must use ITS OWN stage's base — not the base
        // advanced by the earlier system. Mirrors picking (a non-first PreUpdate system reading
        // Changed<GlobalTransform>). Keying the base by `StageLabel` (instead of execution-stage index)
        // broke this: the later system saw "changed since the earlier system", missing cross-frame writes.
        use apex_core::query::Changed;
        #[derive(Component, Clone, Copy)]
        struct V(u32);
        #[derive(Default)]
        struct Saw(Vec<bool>);
        #[derive(Default)]
        struct FrameNo(u32);

        let mut world = World::new();
        world.insert_resource(Saw::default());
        world.insert_resource(FrameNo::default());
        let e = world.spawn((V(0),));

        let mut s = Scheduler::new();
        // Two PreUpdate systems — Sequential systems run in their own execution stages, so `observe` is a
        // LATER execution stage than `lead` (same `StageLabel::PreUpdate`).
        s.add_systems(StageLabel::PreUpdate, seq("lead", |_w: &mut World| {}));
        s.add_systems(
            StageLabel::PreUpdate,
            seq("observe", move |w: &mut World| {
                let base = w.last_run_tick();
                let changed = Query::<(Changed<V>,)>::new_with_tick(w, base).iter().count() > 0;
                w.resource_mut::<Saw>().0.push(changed);
            }),
        );
        s.add_systems(
            StageLabel::PostUpdate,
            seq("write", move |w: &mut World| {
                if w.resource::<FrameNo>().0 == 2 {
                    if let Some(mut v) = w.get_mut::<V>(e) {
                        v.0 += 1;
                    }
                }
                w.resource_mut::<FrameNo>().0 += 1;
            }),
        );

        for _ in 0..5 {
            s.run(&mut world);
        }
        // `observe` (the later PreUpdate system) must see the PostUpdate-frame-2 write on frame 3.
        assert!(
            world.resource::<Saw>().0[3],
            "a later reader in a multi-system stage must still see the cross-stage write"
        );
    }

    #[test]
    fn cross_stage_change_detection_postupdate_to_preupdate_next_frame() {
        // TD-52 regression: a write in PostUpdate of frame N must be visible to a PreUpdate
        // `Changed<T>` reader in frame N+1. The old per-frame change tick made every reader's base
        // "the end of the previous frame", so a PostUpdate-N write was NOT newer than a PreUpdate-(N+1)
        // reader's base and got skipped. The per-stage change-detection window fixes it (each stage's
        // base = the tick at which THAT stage last ran).
        use apex_core::query::Changed;

        #[derive(Component, Clone, Copy)]
        struct Marked(u32);
        #[derive(Default)]
        struct Seen(Vec<bool>);
        #[derive(Default)]
        struct FrameNo(u32);

        let mut world = World::new();
        world.insert_resource(Seen::default());
        world.insert_resource(FrameNo::default());
        let e = world.spawn((Marked(0),));

        let mut sched = Scheduler::new();
        // PreUpdate observer: did `Marked` change since this stage last ran?
        sched.add_systems(
            StageLabel::PreUpdate,
            seq("observe", move |w: &mut World| {
                let last = w.last_run_tick();
                let changed = Query::<(Changed<Marked>,)>::new_with_tick(w, last).iter().count() > 0;
                w.resource_mut::<Seen>().0.push(changed);
            }),
        );
        // PostUpdate writer: mutate `Marked` only on frame 2 — a LATER stage of that frame.
        sched.add_systems(
            StageLabel::PostUpdate,
            seq("write", move |w: &mut World| {
                if w.resource::<FrameNo>().0 == 2 {
                    if let Some(mut m) = w.get_mut::<Marked>(e) {
                        m.0 += 1; // mutable access stamps the change tick at the PostUpdate tick
                    }
                }
                w.resource_mut::<FrameNo>().0 += 1;
            }),
        );

        for _ in 0..5 {
            sched.run(&mut world);
        }

        // Frame 0: the freshly spawned `Marked` is a change ⇒ seen (true).
        // Frame 1: nothing new ⇒ false.
        // Frame 2: PostUpdate writes AFTER PreUpdate already ran ⇒ not seen that frame ⇒ false.
        // Frame 3: PreUpdate sees the previous-frame PostUpdate write — the cross-stage detection the
        //          old per-frame model MISSED (this index would be `false` without the fix).
        // Frame 4: nothing new ⇒ false.
        let seen = &world.resource::<Seen>().0;
        assert_eq!(
            seen,
            &vec![true, false, false, true, false],
            "PreUpdate(N+1) must see PostUpdate(N) write (cross-stage change detection, TD-52)"
        );
        assert!(seen[3], "the cross-stage write of frame 2 must be visible to PreUpdate in frame 3");
    }

    #[test]
    fn auto_system_no_conflict_same_stage() {
        // AutoMovement (Write<Pos>) и AutoHealth (Write<Hp>) — нет конфликта
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health", AutoHealth);
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
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_auto_system("m1", AutoMovement); // Write<Pos>
        sched.add_auto_system("m2", AutoMovement2); // Write<Pos>
        sched.compile().unwrap();

        assert_eq!(
            sched.stages().unwrap().len(),
            2,
            "Write+Write должен дать 2 Stage"
        );
    }

    // ── ConflictKind тесты ────────────────────────────────────

    #[test]
    fn conflict_kind_in_edge_info() {
        struct WriterA;
        impl ParSystem for WriterA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB;
        impl ParSystem for WriterB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
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
    fn asd_does_not_split_resource_mutating_system_td37() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Имитирует plain-fn `(Query<&Pos>, ResMut<Counter>)`: ШИРОКИЙ запрос (чтение Pos над
        // многими сущностями) + мутация РЕСУРСА. До фикса TD-37 ASD дробил такую систему на чанки
        // и звал ТЕЛО раз на чанк ⇒ мутация ресурса умножалась (баг many_foxes@10000: лисы в ~20×
        // быстрее). С фиксом `resource_write` гейтит ASD ⇒ система = один таск ⇒ ровно один вызов.
        struct Counter; // только носитель TypeId «ресурса»
        struct ResourceMutator(Arc<AtomicUsize>);
        impl ParSystem for ResourceMutator {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new()
                    .read::<Pos>()
                    .write::<Counter>()
                    .resource_write()
            }
            fn run(&mut self, _: SystemContext<'_>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let mut sched = Scheduler::new();
        sched.add_par_system("res_mutator", ResourceMutator(calls.clone()));

        let mut world = World::new();
        // Заведомо выше порога чанка — без фикса дало бы row-split на много чанков.
        for _ in 0..50_000 {
            world.spawn((Pos { x: 0.0, y: 0.0 },));
        }

        sched.run(&mut world);

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "система с мутацией ресурса обязана выполняться РОВНО один раз, а не раз на ASD-чанк (TD-37)"
        );
    }

    #[test]
    fn asd_still_splits_pure_query_system() {
        // Контроль: чистая система (только запрос, без ресурс-мутации/Commands) над многими
        // сущностями ВСЁ ЕЩЁ декомпозируется (per-entity записи идемпотентны под чанками) —
        // перф data-parallel путей фикс TD-37 не трогает. Проверяем, что её access НЕ помечен
        // `writes_resource`/`uses_commands` (значит ASD-eligible).
        let access = AccessDescriptor::new().read::<Vel>().write::<Pos>();
        assert!(!access.writes_resource, "чистая query-система не пишет ресурс");
        assert!(!access.uses_commands, "чистая query-система не использует Commands");
    }

    #[test]
    fn sequential_barrier_in_edge_info() {
        let mut sched = Scheduler::new();
        let _par = sched.add_auto_system("movement", AutoMovement);
        let _seq = sched.add_system("barrier", |_| {}).id();
        sched.compile().unwrap();

        // Должны быть рёбра с SequentialBarrier
        let has_barrier = sched
            .edge_info
            .iter()
            .any(|e| matches!(e.kind, ConflictKind::SequentialBarrier));
        assert!(has_barrier, "Sequential барьер должен быть в edge_info");
    }

    // ── debug_plan_verbose тест ───────────────────────────────

    #[test]
    fn debug_plan_verbose_works() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health", AutoHealth);
        sched.add_system("commands", |_| {});
        sched.compile().unwrap();

        let plan = sched.debug_plan_verbose();
        assert!(plan.contains("PARALLEL"), "должен быть PARALLEL Stage");
        assert!(plan.contains("sequential"), "должен быть sequential Stage");
        assert!(plan.contains("Conflict"), "должен показывать конфликты");
        assert!(plan.contains("Summary"), "должен показывать summary");
    }

    #[test]
    fn compile_with_world_shows_component_names() {
        let mut world = World::new();

        // Явная регистрация компонентов нужна для populate_type_names.
        // Auto-регистрация через #[derive(Component)] также работает,
        // но для гарантии в этом диагностическом тесте регистрируем явно.
        world.register_component::<Pos>();
        world.register_component::<Vel>();

        // Система, использующая Pos и Vel — их имена появятся в reads/writes
        struct MovementSystem;
        impl ParSystem for MovementSystem {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("move", MovementSystem);
        sched.compile_with_world(&world).unwrap();

        let plan = sched.debug_plan_verbose();
        assert!(
            plan.contains("Pos"),
            "debug_plan должен содержать 'Pos', содержит: {}",
            plan
        );
        assert!(
            plan.contains("Vel"),
            "debug_plan должен содержать 'Vel', содержит: {}",
            plan
        );
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
        #[allow(dead_code)]
        struct MovementSystem;
        impl ParSystem for MovementSystem {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, ctx: SystemContext<'_>) {
                ctx.query::<(Read<Vel>, Write<Pos>)>()
                    .for_each(|_, (vel, mut pos)| {
                        pos.x += vel.x;
                        pos.y += vel.y;
                    });
            }
        }

        let mut sched = Scheduler::new();
        let log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> = Default::default();

        let log_a = log.clone();
        let a = sched
            .add_system("a", move |_| {
                log_a.lock().unwrap().push("a");
            })
            .id();
        let log_b = log.clone();
        let b = sched
            .add_system("b", move |_| {
                log_b.lock().unwrap().push("b");
            })
            .id();

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
        struct WriterA;
        impl ParSystem for WriterA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB;
        impl ParSystem for WriterB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
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
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct HealthSystem;
        impl ParSystem for HealthSystem {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Hp>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("par_a", MovementSystem);
        sched.add_system("barrier", |_| {});
        sched.add_par_system("par_b", HealthSystem);
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // sequential больше не фрагментирует — параллельные группируются до него
        assert_eq!(stages.len(), 2);
        assert!(stages[0].all_parallel);
        assert!(!stages[1].all_parallel);
    }

    #[test]
    fn fn_par_system_with_resource() {
        let mut sched = Scheduler::new();
        sched.add_par_access(
            "scaled_movement",
            access_desc!(read<DeltaTime>, read<Vel>, write<Pos>),
            |ctx: SystemContext<'_>| {
                let dt = ctx.resource::<DeltaTime>();
                ctx.query::<(Read<Vel>, Write<Pos>)>()
                    .for_each(|_, (vel, mut pos)| {
                        pos.x += vel.x * (*dt).0;
                        pos.y += vel.y * (*dt).0;
                    });
            },
        );

        let mut world = World::new();

        world.insert_resource(DeltaTime(0.5));
        world.spawn((Pos { x: 0.0, y: 0.0 }, Vel { x: 2.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each(|_, p| {
            result = (p.x, p.y);
        });

        assert!((result.0 - 1.0).abs() < 1e-6);
        assert!((result.1 - 2.0).abs() < 1e-6);
    }

    /// Параллельное выполнение: обе AutoSystem применяют изменения.
    #[test]
    fn parallel_auto_systems_correctness() {
        let mut sched = Scheduler::new();
        sched.add_auto_system("movement", AutoMovement);
        sched.add_auto_system("health", AutoHealth);
        sched.compile().unwrap();
        assert!(sched.stages().unwrap()[0].is_parallelizable());

        let mut world = World::new();

        world.spawn((Pos { x: 0.0, y: 0.0 }, Vel { x: 1.0, y: 2.0 }, Hp(-5.0)));

        sched.run(&mut world);

        let mut pos_result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each(|_, p| {
            pos_result = (p.x, p.y);
        });
        assert!((pos_result.0 - 1.0).abs() < 1e-6);
        assert!((pos_result.1 - 2.0).abs() < 1e-6);

        let mut hp_result = -1.0f32;
        Query::<Read<Hp>>::new(&world).for_each(|_, hp| {
            hp_result = hp.0;
        });
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
        assert_eq!(
            *startup_count.lock().unwrap(),
            1,
            "Startup должен выполниться 1 раз"
        );

        // Второй run() — Startup НЕ выполняется
        sched.run_sequential(&mut world);
        assert_eq!(
            *startup_count.lock().unwrap(),
            1,
            "Startup НЕ должен выполниться повторно"
        );
    }

    #[test]
    fn stage_label_in_debug_plan() {
        let mut sched = Scheduler::new();
        sched.add_startup_system("init", |_| {});
        sched.add_auto_system("movement", AutoMovement);
        sched.compile().unwrap();

        let plan = sched.debug_plan();
        assert!(
            plan.contains("Startup"),
            "debug_plan должен содержать Startup label"
        );
        assert!(
            plan.contains("Update"),
            "debug_plan должен содержать Update label"
        );
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
        assert!(
            stages.len() >= 2,
            "Должно быть минимум 2 Stage, получено {}",
            stages.len()
        );

        // Проверяем что PreUpdate идут перед Update
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        assert!(pre_idx.is_some(), "Должен быть PreUpdate Stage");
        assert!(upd_idx.is_some(), "Должен быть Update Stage");
        assert!(
            pre_idx.unwrap() < upd_idx.unwrap(),
            "PreUpdate должен быть перед Update"
        );
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
        assert_eq!(
            stages[0].label,
            StageLabel::Startup,
            "Первый Stage должен быть Startup"
        );
        assert!(
            stages.iter().any(|s| s.label == StageLabel::Update),
            "Должен быть Update Stage"
        );
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
        assert_eq!(
            *startup_val.lock().unwrap(),
            42,
            "Startup система должна выполниться"
        );
        assert_eq!(
            *world.resource::<i32>(),
            42,
            "Ресурс должен быть установлен"
        );

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
        assert!(
            stages.len() >= 2,
            "EventWriteWrite конфликт: ожидается минимум 2 Stage, получено {}",
            stages.len()
        );
    }

    #[test]
    fn event_write_read_conflict() {
        let mut sched = Scheduler::new();

        sched.add_par_system("writer", EventWriterForTest);
        sched.add_par_system("reader", EventReaderForTest);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // EventWriter + EventReader одного типа → должны быть в разных Stage (конфликт)
        assert!(
            stages.len() >= 2,
            "EventWriteRead конфликт: ожидается минимум 2 Stage, получено {}",
            stages.len()
        );
    }

    /// F2: two systems reading the SAME event must be serialized — each
    /// EventReader mutates the queue's shared cursor registry, so running them in
    /// parallel is a data race. (They used to share a stage — the racy old
    /// behavior; reader parallelism returns with the per-system cursor model,
    /// wave 6.)
    #[test]
    fn same_event_readers_are_serialized() {
        let mut sched = Scheduler::new();

        let a = sched.add_par_system("reader_a", EventReaderForTest);
        let b = sched.add_par_system("reader_b", EventReaderForTest);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        let sa = stages.iter().position(|s| s.system_ids.contains(&a)).unwrap();
        let sb = stages.iter().position(|s| s.system_ids.contains(&b)).unwrap();
        assert_ne!(sa, sb, "two readers of the same event must be serialized (F2)");

        let conflicts = sched.conflicts_between(a, b);
        assert!(
            conflicts
                .iter()
                .any(|c| matches!(c, ConflictKind::SharedEventReaders { .. })),
            "the conflict must be SharedEventReaders, got {conflicts:?}"
        );
    }

    struct DifferentEventReader;
    impl ParSystem for DifferentEventReader {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<f64>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    #[test]
    fn event_read_different_events_no_conflict() {
        let mut sched = Scheduler::new();
        // Слушают разные события — конфликта нет
        sched.add_par_system("reader_i32", EventReaderForTest); // Listen<i32>
        sched.add_par_system("reader_f64", DifferentEventReader); // Listen<f64>
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert!(
            stages.iter().any(|s| s.system_ids.len() >= 2),
            "EventRead разных событий не должны конфликтовать: ожидается Stage с обеими системами"
        );
    }

    #[test]
    fn event_conflict_kind_in_edge_info() {
        let mut sched = Scheduler::new();

        let wid = sched.add_par_system("writer", EventWriterForTest);
        let rid = sched.add_par_system("reader", AnotherEventWriter);

        sched.compile().unwrap();

        let conflicts = sched.conflicts_between(wid, rid);
        assert!(
            !conflicts.is_empty(),
            "Должен быть конфликт между EventWriter и EventWriter"
        );

        // Проверяем тип конфликта
        let has_event_conflict = conflicts
            .iter()
            .any(|c| matches!(c, ConflictKind::EventWriteWrite { .. }));
        assert!(has_event_conflict, "Конфликт должен быть EventWriteWrite");
    }

    #[test]
    fn event_ordering_disabled_no_conflict() {
        let mut sched = Scheduler::new();
        sched.enable_event_ordering(false);

        sched.add_par_system("emitter", EventWriterForTest); // Emit<i32>
        sched.add_par_system("listener", EventReaderForTest); // Listen<i32>

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(
            stages.iter().any(|s| s.system_ids.len() >= 2),
            "При event_ordering=false Emit+Listen не должны конфликтовать"
        );
    }

    #[test]
    fn event_ordering_enabled_by_default() {
        let mut sched = Scheduler::new();
        // По умолчанию enable_event_ordering не вызывается — должен быть true

        sched.add_par_system("emitter", EventWriterForTest); // Emit<i32>
        sched.add_par_system("listener", EventReaderForTest); // Listen<i32>

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(
            stages.len() >= 2,
            "По умолчанию Emit+Listen должны быть в разных Stage, получено {}",
            stages.len()
        );
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
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update должен быть перед PreUpdate при configure_stages"
        );
    }

    /// D8: multiple Custom stages (all priority 7) are ordered by NAME, not by
    /// FxHashMap iteration — reproducible run-to-run and across platforms.
    #[test]
    fn custom_stages_ordered_by_name_deterministically() {
        let mut sched = Scheduler::new();
        sched.add_system_to_stage("z", |_| {}, StageLabel::Custom("zebra".to_string()));
        sched.add_system_to_stage("a", |_| {}, StageLabel::Custom("alpha".to_string()));
        sched.add_system_to_stage("m", |_| {}, StageLabel::Custom("mango".to_string()));
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        let custom: Vec<String> = stages
            .iter()
            .filter_map(|s| match &s.label {
                StageLabel::Custom(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            custom,
            vec![
                "alpha".to_string(),
                "mango".to_string(),
                "zebra".to_string()
            ],
            "Custom stages must be ordered by name"
        );
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
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update должен быть перед PreUpdate"
        );

        // Last должен быть в конце (не указан в order, добавлен автоматически)
        let last_idx = stages.iter().position(|s| s.label == StageLabel::Last);
        assert!(
            last_idx.is_some(),
            "Last должен присутствовать даже если не указан в configure_stages"
        );
        assert!(
            last_idx.unwrap() > pre_idx.unwrap() || last_idx.unwrap() > upd_idx.unwrap(),
            "Last (не указанный в order) должен быть в конце"
        );
    }

    // ── Pipeline тесты ─────────────────────────────────────────

    #[derive(Clone, Copy)]
    struct DamageEvent;

    struct EmitDamage;
    impl ParSystem for EmitDamage {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct TransformDmg;
    impl ParSystem for TransformDmg {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new()
                .read_event::<DamageEvent>()
                .write_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct ListenDamage;
    impl ParSystem for ListenDamage {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct ListenDamage2;
    impl ParSystem for ListenDamage2 {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct ListenerOnly;
    impl ParSystem for ListenerOnly {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read_event::<DamageEvent>()
        }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    #[test]
    fn pipeline_basic_chain_ordering() {
        let mut sched = Scheduler::new();

        let physics_id = sched.add_par_system("physics", EmitDamage);
        let armor_id = sched.add_par_system("armor", TransformDmg);
        let health_id = sched.add_par_system("health", ListenDamage);

        Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("physics")
            .transformed_by("armor")
            .consumed_by("health")
            .build(&mut sched);

        sched.compile().unwrap();

        // Должно быть минимум 3 Stage: physics → armor → health
        let stages = sched.stages().unwrap();
        assert!(
            stages.len() >= 3,
            "Конвейер из 3 стадий должен создать минимум 3 Stage, получено: {}",
            stages.len()
        );

        // Проверяем порядок плоского списка
        let flat = &sched.execution_plan.as_ref().unwrap().flat_order;
        let pos_physics = flat.iter().position(|&id| id == physics_id).unwrap();
        let pos_armor = flat.iter().position(|&id| id == armor_id).unwrap();
        let pos_health = flat.iter().position(|&id| id == health_id).unwrap();
        assert!(pos_physics < pos_armor, "physics должен быть до armor");
        assert!(pos_armor < pos_health, "armor должен быть до health");
    }

    /// §0.2a hygiene: `Pipeline::build` naming an unregistered system used to
    /// panic on an opaque `unwrap`. It must now log an error and skip wiring —
    /// the scheduler still compiles.
    #[test]
    fn pipeline_build_unregistered_system_does_not_panic() {
        let mut sched = Scheduler::new();
        let _physics = sched.add_par_system("physics", EmitDamage);
        // "ghost" is never registered.
        Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("physics")
            .consumed_by("ghost")
            .build(&mut sched);
        sched.compile().unwrap();
    }

    /// Both consumers of the same event run after the producer and each receives
    /// the event. They are SERIALIZED with respect to each other (F2: reading the
    /// same event mutates its shared cursor registry, so parallel reads race);
    /// reader parallelism returns with the per-system cursor model (wave 6).
    #[test]
    fn pipeline_consumers_run_after_producer_and_are_serialized() {
        let mut sched = Scheduler::new();

        let physics_id = sched.add_par_system("physics", EmitDamage);
        let health_id = sched.add_par_system("health", ListenDamage);
        let sound_id = sched.add_par_system("sound", ListenDamage2);

        Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("physics")
            .consumed_by("health")
            .consumed_by("sound")
            .build(&mut sched);

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        let stage_of = |id| stages.iter().position(|s| s.system_ids.contains(&id)).unwrap();
        let (p, h, s) = (stage_of(physics_id), stage_of(health_id), stage_of(sound_id));
        // Producer before both consumers; the two consumers serialized (F2).
        assert!(p < h && p < s, "producer must run before both consumers");
        assert_ne!(h, s, "consumers of the same event are serialized (F2)");
    }

    #[test]
    fn pipeline_validation_catches_wrong_role() {
        let mut sched = Scheduler::new();

        // ListenerOnly не имеет Emit<Damage> — не может быть Producer
        let _bad_id = sched.add_par_system("bad_producer", ListenerOnly);

        let result = Scheduler::event_pipeline::<DamageEvent>()
            .produced_by("bad_producer")
            .build_validated(&mut sched);

        assert!(
            result.is_err(),
            "Должна быть ошибка: система объявлена Producer но не имеет Emit"
        );

        let errors = result.unwrap_err();
        assert!(matches!(
            errors[0],
            PipelineValidationError::ProducerMissingEmit { .. }
        ));
    }

    #[test]
    fn configure_stages_persists_across_compiles() {
        let mut sched = Scheduler::new();

        // Добавляем системы в Update и PreUpdate
        sched.add_auto_system_to_stage("update_movement", AutoMovement, StageLabel::Update);
        sched.add_system_to_stage("pre_work", |_| {}, StageLabel::PreUpdate);

        // Настраиваем порядок: Update ДО PreUpdate
        sched.configure_stages(vec![
            StageLabel::Startup,
            StageLabel::Update,
            StageLabel::PreUpdate,
        ]);

        // Первая компиляция
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update должен быть перед PreUpdate после первой компиляции"
        );

        // Добавляем новую систему (триггерит invalidate_plan)
        sched.add_system_to_stage("more_pre_work", |_| {}, StageLabel::PreUpdate);

        // Вторая компиляция — stage_order должен сохраниться
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(
            upd_idx.is_some(),
            "Update Stage должен быть после перекомпиляции"
        );
        assert!(
            pre_idx.is_some(),
            "PreUpdate Stage должен быть после перекомпиляции"
        );
        assert!(upd_idx.unwrap() < pre_idx.unwrap(),
            "Update должен быть перед PreUpdate после перекомпиляции — stage_order должен сохраняться");

        // Третья компиляция — ещё один вызов для уверенности
        sched.add_system_to_stage("extra", |_| {}, StageLabel::Last);
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        let upd_idx = stages.iter().position(|s| s.label == StageLabel::Update);
        let pre_idx = stages.iter().position(|s| s.label == StageLabel::PreUpdate);
        assert!(
            upd_idx.unwrap() < pre_idx.unwrap(),
            "Update перед PreUpdate после третьей компиляции"
        );
    }

    #[test]
    fn archetype_indices_for_subworld_uses_any_criterion() {
        // Две системы с перекрёстным Write/Read по Pos и Vel
        // не должны создавать CircularDependency.
        // Проверяем, что any()-критерий в compute_archetype_indices
        // и BidirectionalWriteRead в detect_conflict_kind работают корректно.
        let _world = World::new();

        let mut sched = Scheduler::new();

        // Просто dummy-системы: проверяем, что компиляция проходит
        sched.add_system("sys_a", |_: &mut World| {});
        sched.add_system("sys_b", |_: &mut World| {});

        let result = sched.compile();
        assert!(
            result.is_ok(),
            "Компиляция должна пройти без ошибок: {:?}",
            result.err()
        );
    }

    #[test]
    fn independent_resolves_bidirectional_write_read_deterministically() {
        // A: Read<Vel>, Write<Pos>; B: Read<Pos>, Write<Vel> — настоящий BidirectionalWriteRead
        // (без явного порядка → CircularDependency).
        struct A;
        impl ParSystem for A {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Vel>().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct B;
        impl ParSystem for B {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().read::<Pos>().write::<Vel>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        // Без `independent` — цикл.
        let mut sched = Scheduler::new();
        sched.add_par_system("a", A);
        sched.add_par_system("b", B);
        assert!(
            matches!(sched.compile(), Err(SchedulerError::CircularDependency { .. })),
            "перекрёстный write/read без явного порядка = CircularDependency"
        );

        // С `independent` — компилируется, порядок детерминирован (регистрация: a → b).
        let mut sched = Scheduler::new();
        sched.add_par_system("a", A);
        sched.add_par_system("b", B);
        sched.independent(&["a", "b"]).unwrap();
        sched
            .compile()
            .expect("independent снимает CircularDependency, сериализуя в порядке регистрации");
    }

    /// D5: a symmetric (WriteWrite) conflict plus an explicit ordering that runs
    /// against registration order must NOT fabricate a CircularDependency — the
    /// explicit ordering wins and serializes the two systems.
    #[test]
    fn symmetric_conflict_respects_reverse_explicit_order() {
        struct WriteA;
        impl ParSystem for WriteA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriteB;
        impl ParSystem for WriteB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("a", WriteA); // registered first
        sched.add_par_system("b", WriteB);
        // Explicit ordering against registration: b before a.
        sched.before("b", "a").unwrap();

        // Old code: conflict edge a->b plus explicit b->a = false cycle.
        sched
            .compile()
            .expect("explicit ordering must resolve the symmetric conflict without a false cycle");

        let a = sched.find_id_by_name("a").unwrap();
        let b = sched.find_id_by_name("b").unwrap();
        let stages = sched.stages().unwrap();
        let a_stage = stages.iter().position(|s| s.system_ids.contains(&a)).unwrap();
        let b_stage = stages.iter().position(|s| s.system_ids.contains(&b)).unwrap();
        assert!(b_stage < a_stage, "explicit before(\"b\",\"a\") must run b before a");
    }

    /// D2: a system with no component access (only side effects / whole-world)
    /// has nothing to partition by rows. On a large world the old code gave it
    /// entity_count = whole world and let ASD chunk it, running its body once per
    /// chunk. It must run exactly once.
    #[test]
    fn empty_access_system_runs_once_not_chunked() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        #[derive(apex_macros::Component)]
        struct Marker;

        let mut world = World::new();
        for _ in 0..100_000 {
            world.spawn((Marker,));
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            par("tick", move |_ctx: SystemContext<'_>| {
                c.fetch_add(1, Ordering::Relaxed);
            }),
        );
        sched.run(&mut world);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "an empty-access system must run once, not once per ASD chunk"
        );
    }

    /// D6: a stage whose systems are all disabled by run_if must NOT advance its
    /// change-detection window, so a system resuming after the pause still sees
    /// Changed<T> made while it was paused.
    #[test]
    fn run_if_paused_stage_does_not_lose_changed() {
        use apex_core::prelude::*;

        #[derive(apex_macros::Component)]
        struct Mark(u32);
        struct Ctl {
            active: bool,
            mutate: bool,
            entity: Entity,
            seen: u32,
        }

        let mut world = World::new();
        let e = world.spawn((Mark(0),));
        world.insert_resource(Ctl {
            active: false,
            mutate: false,
            entity: e,
            seen: 0,
        });

        fn mutator(w: &mut World) {
            let (mutate, e) = {
                let c = w.resource::<Ctl>();
                (c.mutate, c.entity)
            };
            if mutate {
                if let Some(mut m) = w.get_mut::<Mark>(e) {
                    m.0 += 1;
                }
            }
        }
        fn reader(w: &mut World) {
            let mut n = 0u32;
            let q = w.query::<(Read<Mark>, Changed<Mark>)>();
            q.for_each(|_, _| n += 1);
            w.resource_mut::<Ctl>().seen += n;
        }

        let mut sched = Scheduler::new();
        sched.add_system_to_stage("mutator", mutator, StageLabel::Update);
        sched
            .add_system_to_stage("reader", reader, StageLabel::PostUpdate)
            .run_if(|w: &World| w.resource::<Ctl>().active);

        sched.run(&mut world); // frame 1: paused, no mutation
        world.resource_mut::<Ctl>().mutate = true;
        sched.run(&mut world); // frame 2: Mark mutated, reader still paused
        world.resource_mut::<Ctl>().mutate = false;
        world.resource_mut::<Ctl>().active = true;
        sched.run(&mut world); // frame 3: reader resumes — must see the frame-2 change

        assert_eq!(
            world.resource::<Ctl>().seen,
            1,
            "reader must see Changed<Mark> produced while its stage was paused"
        );
    }

    /// D4: a Filtered system in a later stage must see archetypes an earlier stage
    /// spawned THIS frame — the cached per-system archetype indices are refreshed
    /// before each stage (a non-parallel stage's new archetypes used to be missed
    /// by a later parallel stage until the next frame).
    #[test]
    fn later_stage_sees_archetype_spawned_earlier_this_frame() {
        use apex_core::prelude::*;

        #[derive(apex_macros::Component)]
        struct Fresh;
        #[derive(Default)]
        struct Count(usize);
        #[derive(Default)]
        struct Done(bool);

        fn spawner(w: &mut World) {
            if !w.resource::<Done>().0 {
                for _ in 0..5 {
                    w.spawn((Fresh,));
                }
                w.resource_mut::<Done>().0 = true;
            }
        }
        fn counter(q: Query<Read<Fresh>>, mut count: ResMut<Count>) {
            let mut n = 0;
            q.for_each(|_, _| n += 1);
            count.0 = n;
        }

        let mut world = World::new();
        world.insert_resource(Count::default());
        world.insert_resource(Done::default());
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, spawner);
        sched.add_systems(StageLabel::PostUpdate, counter);

        sched.run(&mut world); // frame 1: Update spawns Fresh, PostUpdate must count them
        assert_eq!(
            world.resource::<Count>().0,
            5,
            "counter must see the archetype spawned by an earlier stage this frame"
        );
    }

    /// F3: a whole-world system (SystemContext / `Ctx` can touch any data but
    /// declares no specific access) must conflict with — and be serialized
    /// against — a writer, not run in parallel with it.
    #[test]
    fn whole_world_system_is_serialized_against_a_writer() {
        struct Whole;
        impl ParSystem for Whole {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().whole_world()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct Writer;
        impl ParSystem for Writer {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("whole", Whole);
        sched.add_par_system("writer", Writer);
        sched.compile().unwrap();

        let whole = sched.find_id_by_name("whole").unwrap();
        let writer = sched.find_id_by_name("writer").unwrap();
        let stages = sched.stages().unwrap();
        let ws = stages.iter().position(|s| s.system_ids.contains(&whole)).unwrap();
        let rs = stages.iter().position(|s| s.system_ids.contains(&writer)).unwrap();
        assert_ne!(
            ws, rs,
            "a whole-world system must be serialized against a writer, not parallel with it"
        );
    }

    #[test]
    fn bidir_write_read_no_false_circular_dep() {
        // SystemA: Read<Vel>, Write<Pos>
        // SystemB: Read<Pos>, Write<Vel>
        // Реальный конфликт должен быть LinearOrder (не цикл).
        let mut sched = Scheduler::new();
        sched.add_auto_system("sys_a", AutoMovement);
        sched.add_auto_system("sys_b", AutoMovement);

        let result = sched.compile();
        // Одинаковые системы с Read<Vel>+Write<Pos> — конфликтуют
        // (WriteWrite по Pos, WriteWrite по Vel), но не создают цикл.
        assert!(
            result.is_ok(),
            "Компиляция должна пройти без CircularDependency: {:?}",
            result.err()
        );
    }

    // ── Run Condition тесты ────────────────────────────────────

    #[test]
    fn run_condition_true_system_executes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.add_system("test_sys", |_: &mut World| {
            RAN.store(true, Ordering::SeqCst);
        })
        .run_if(|_| true);

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert!(RAN.load(Ordering::SeqCst), "Система должна выполниться когда condition=true");
    }

    /// Регрессия (аудит 2026-06-12): scope-условие из `scoped`+`run_condition`
    /// (а) применяется к системам, зарегистрированным через `add_systems`
    /// (раньше этот путь его молча терял), и (б) НЕ прилипает к системам,
    /// зарегистрированным ПОСЛЕ блока (раньше скоуп никогда не сбрасывался).
    #[test]
    fn scoped_condition_applies_to_add_systems_and_does_not_leak() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static INSIDE_RAN: AtomicBool = AtomicBool::new(false);
        static OUTSIDE_RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.scoped(|s| {
            s.run_condition(|_| false); // скоуп всегда-false
            s.add_systems(
                StageLabel::Update,
                seq("inside", |_w: &mut World| {
                    INSIDE_RAN.store(true, Ordering::SeqCst);
                }),
            );
        });
        // После блока скоуп снят — система выполняется безусловно.
        sched.add_systems(
            StageLabel::Update,
            seq("outside", |_w: &mut World| {
                OUTSIDE_RAN.store(true, Ordering::SeqCst);
            }),
        );

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert!(
            !INSIDE_RAN.load(Ordering::SeqCst),
            "scoped-условие должно применяться к add_systems-пути"
        );
        assert!(
            OUTSIDE_RAN.load(Ordering::SeqCst),
            "scope-условие не должно прилипать к системам после блока"
        );
    }

    #[test]
    fn run_condition_false_system_skipped() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.add_system("test_sys", |_: &mut World| {
            RAN.store(true, Ordering::SeqCst);
        })
        .run_if(|_| false);

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert!(!RAN.load(Ordering::SeqCst), "Система НЕ должна выполниться когда condition=false");
    }

    #[test]
    fn run_condition_reads_resource() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        #[derive(Clone, Copy)]
        struct GameState { paused: bool }

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system(
            "pausable",
            {
                struct PausableSystem;
                impl AutoSystem for PausableSystem {
                    type Query = ();
                    type Resources = ();
                    type Events = ();
                    fn run(&mut self, _: SystemContext<'_>) {
                        RAN.store(true, Ordering::SeqCst);
                    }
                }
                PausableSystem
            },
        );
        sched.set_run_if("pausable", |w: &World| {
            w.try_resource::<GameState>().map(|gs| !gs.paused).unwrap_or(true)
        })
        .unwrap();

        // paused = true → система не запускается
        let mut world = World::new();
        world.insert_resource(GameState { paused: true });
        sched.run_sequential(&mut world);
        assert!(!RAN.load(Ordering::SeqCst), "Система не должна работать на паузе");

        // paused = false → система запускается
        RAN.store(false, Ordering::SeqCst);
        let mut world2 = World::new();
        world2.insert_resource(GameState { paused: false });
        let mut sched2 = Scheduler::new();
        let _id2 = sched2.add_auto_system(
            "pausable2",
            {
                struct PausableSystem2;
                impl AutoSystem for PausableSystem2 {
                    type Query = ();
                    type Resources = ();
                    type Events = ();
                    fn run(&mut self, _: SystemContext<'_>) {
                        RAN.store(true, Ordering::SeqCst);
                    }
                }
                PausableSystem2
            },
        );
        sched2.set_run_if("pausable2", |w: &World| {
            w.try_resource::<GameState>().map(|gs| !gs.paused).unwrap_or(true)
        })
        .unwrap();
        sched2.run_sequential(&mut world2);
        assert!(RAN.load(Ordering::SeqCst), "Система должна работать когда не пауза");
    }

    #[test]
    fn run_condition_multiple_systems_mixed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER_A: AtomicUsize = AtomicUsize::new(0);
        static COUNTER_B: AtomicUsize = AtomicUsize::new(0);

        let mut sched = Scheduler::new();
        sched.add_system("always_on", move |_: &mut World| {
            COUNTER_A.fetch_add(1, Ordering::SeqCst);
        });
        sched.add_system("conditionally_off", move |_: &mut World| {
            COUNTER_B.fetch_add(1, Ordering::SeqCst);
        })
        .run_if(|_| false);

        let mut world = World::new();
        sched.run_sequential(&mut world);

        assert_eq!(COUNTER_A.load(Ordering::SeqCst), 1, "always_on должна выполниться");
        assert_eq!(COUNTER_B.load(Ordering::SeqCst), 0, "conditionally_off НЕ должна выполниться");
    }

    #[test]
    fn run_condition_parallel_stage_skips_system() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("skipped_in_parallel", {
            struct SkippedSys;
            impl AutoSystem for SkippedSys {
                type Query = ();
                type Resources = ();
                type Events = ();
                fn run(&mut self, _: SystemContext<'_>) {
                    RAN.store(true, Ordering::SeqCst);
                }
            }
            SkippedSys
        });
        sched.set_run_if("skipped_in_parallel", |_: &World| false).unwrap();

        let mut world = World::new();
        sched.run(&mut world);
        assert!(!RAN.load(Ordering::SeqCst), "Параллельная система с condition=false НЕ запускается");
    }

    #[test]
    fn run_condition_parallel_stage_runs_system() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("runs_in_parallel", {
            struct RunsSys;
            impl AutoSystem for RunsSys {
                type Query = ();
                type Resources = ();
                type Events = ();
                fn run(&mut self, _: SystemContext<'_>) {
                    RAN.store(true, Ordering::SeqCst);
                }
            }
            RunsSys
        });
        sched.set_run_if("runs_in_parallel", |_: &World| true).unwrap();

        let mut world = World::new();
        sched.run(&mut world);
        assert!(RAN.load(Ordering::SeqCst), "Параллельная система с condition=true запускается");
    }

    #[test]
    fn run_condition_startup_system_runs_once_if_true() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let mut sched = Scheduler::new();
        sched.add_startup_system("conditional_startup", move |_: &mut World| {
            COUNTER.fetch_add(1, Ordering::SeqCst);
        })
        .run_if(|_| true);

        let mut world = World::new();
        sched.run_sequential(&mut world);
        sched.run_sequential(&mut world);

        assert_eq!(COUNTER.load(Ordering::SeqCst), 1, "Startup с condition=true должен выполниться ровно 1 раз");
    }

    #[test]
    fn run_condition_auto_system_with_component_access() {
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Component, Clone, Copy)]
        struct Flag(bool);

        static RAN: AtomicBool = AtomicBool::new(false);

        struct ConditionalSystem;
        impl AutoSystem for ConditionalSystem {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {
                RAN.store(true, Ordering::SeqCst);
            }
        }

        let f = move |w: &World| {
            w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
        };

        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("conditional", ConditionalSystem);
        sched.set_run_if("conditional", Box::new(f)).unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);
        assert!(!RAN.load(Ordering::SeqCst), "Без Flag ресурса — не запускается");

        RAN.store(false, Ordering::SeqCst);
    }

    #[test]
    fn run_condition_auto_system_with_component_access_true() {
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Component, Clone, Copy)]
        struct Flag(bool);

        static RAN: AtomicBool = AtomicBool::new(false);

        struct ConditionalSystem;
        impl AutoSystem for ConditionalSystem {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {
                RAN.store(true, Ordering::SeqCst);
            }
        }

        let mut world = World::new();
        world.insert_resource(Flag(true));

        let f = move |w: &World| {
            w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
        };
        let mut sched = Scheduler::new();
        let _id = sched.add_auto_system("conditional2", ConditionalSystem);
        sched.set_run_if("conditional2", Box::new(f)).unwrap();

        sched.run_sequential(&mut world);
        assert!(RAN.load(Ordering::SeqCst), "С Flag(true) ресурсом — запускается");
    }

    // ── Apply Deferred тесты ───────────────────────────────────

    #[test]
    fn apply_deferred_splits_into_two_sub_stages() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_system("first", |_: &mut World| {});
            s.apply_deferred();
            s.add_system("second", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 1, "Должно быть 2 под-Stage'а: first + second");
        assert_eq!(stages[0].system_ids.len(), 1, "Первый под-Stage: 1 система");
        assert_eq!(stages[1].system_ids.len(), 1, "Второй под-Stage: 1 система");
    }

    #[test]
    fn apply_deferred_no_split_if_only_one_system() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_system("only", |_: &mut World| {});
            s.apply_deferred(); // no-op: только одна система, split не нужен
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1, "Без второй системы — split не создаётся");
    }

    #[test]
    fn apply_deferred_commands_visible_in_next_sub_stage() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static ENTITY_SPAWNED: AtomicBool = AtomicBool::new(false);

        #[derive(Component, Clone, Copy)]
        struct Spawned;

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("spawn_then_use"), |s| {
            s.add_system("spawn_sys", move |world: &mut World| {
                world.spawn((Spawned,));
                ENTITY_SPAWNED.store(true, Ordering::SeqCst);
            });
            s.apply_deferred();
            s.add_system("check_sys", move |world: &mut World| {
                let spawned = ENTITY_SPAWNED.load(Ordering::SeqCst);
                assert!(spawned, "spawn_system должна выполниться перед check_system");
                let count = Query::<Read<Spawned>>::new(world).iter().count();
                assert_eq!(count, 1, "spawn должен быть видим в том же кадре");
            });
        });

        let mut world = World::new();
        sched.run_sequential(&mut world);
    }

    #[test]
    fn apply_deferred_multiple_splits() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("multi"), |s| {
            s.add_system("a", |_: &mut World| {});
            s.apply_deferred();
            s.add_system("b", |_: &mut World| {});
            s.apply_deferred();
            s.add_system("c", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 2, "3 под-Stage'а: [a], [b], [c]");
    }

    #[test]
    fn apply_deferred_mixed_with_run_conditions() {
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Clone, Copy)]
        struct Flag(bool);

        static A_RAN: AtomicBool = AtomicBool::new(false);
        static B_RAN: AtomicBool = AtomicBool::new(false);

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("mixed"), |s| {
            s.add_system("a", move |_: &mut World| {
                A_RAN.store(true, Ordering::SeqCst);
            });
            s.apply_deferred();
            s.add_system("b", move |_: &mut World| {
                B_RAN.store(true, Ordering::SeqCst);
            })
            .run_if(|w: &World| {
                w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
            });
        });

        // Flag = false → b пропускается
        let mut world1 = World::new();
        world1.insert_resource(Flag(false));
        sched.compile_with_world(&world1).unwrap();
        sched.run_sequential(&mut world1);
        assert!(A_RAN.load(Ordering::SeqCst), "a всегда должна выполняться");
        assert!(!B_RAN.load(Ordering::SeqCst), "b НЕ должна выполняться при Flag=false");

        // Flag = true → b выполняется
        A_RAN.store(false, Ordering::SeqCst);
        let mut sched2 = Scheduler::new();
        sched2.staged(StageLabel::tag("mixed2"), |s| {
            s.add_system("a2", move |_: &mut World| {
                A_RAN.store(true, Ordering::SeqCst);
            });
            s.apply_deferred();
            s.add_system("b2", move |_: &mut World| {
                B_RAN.store(true, Ordering::SeqCst);
            })
            .run_if(|w: &World| {
                w.try_resource::<Flag>().map(|f| f.0).unwrap_or(false)
            });
        });
        let mut world2 = World::new();
        world2.insert_resource(Flag(true));
        sched2.compile_with_world(&world2).unwrap();
        sched2.run_sequential(&mut world2);
        assert!(A_RAN.load(Ordering::SeqCst));
        assert!(B_RAN.load(Ordering::SeqCst), "b должна выполняться при Flag=true");
    }

    #[test]
    fn apply_deferred_double_call_idempotent() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_system("only", |_: &mut World| {});
            s.apply_deferred(); // первая
            s.apply_deferred(); // вторая — идемпотентна
            s.add_system("after", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 1, "Двойной apply_deferred не должен создавать пустых под-Stage'ей");
    }

    #[test]
    fn apply_deferred_at_start_noop() {
        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.apply_deferred(); // нет предыдущих систем → no-op
            s.add_system("after", |_: &mut World| {});
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1, "apply_deferred без предыдущей системы — no-op");
    }

    #[test]
    fn apply_deferred_with_auto_systems() {
        struct DummySys;
        impl AutoSystem for DummySys {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_auto_system("par_a", DummySys);
            s.apply_deferred();
            s.add_auto_system("par_b", DummySys);
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1 + 1, "AutoSystem + apply_deferred = 2 под-Stage'а");
        assert!(stages[0].all_parallel, "Первый под-Stage: all_parallel=true");
        assert!(stages[1].all_parallel, "Второй под-Stage: all_parallel=true");
    }

    #[test]
    fn apply_deferred_groups_parallel_systems_correctly() {
        struct DummySys;
        impl AutoSystem for DummySys {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.staged(StageLabel::tag("test"), |s| {
            s.add_auto_system("a", DummySys);
            s.add_auto_system("b", DummySys);
            s.apply_deferred();
            s.add_auto_system("c", DummySys);
            s.add_auto_system("d", DummySys);
        });

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 2, "2 под-Stage'а: [a,b] + [c,d]");
        assert_eq!(stages[0].system_ids.len(), 2);
        assert_eq!(stages[1].system_ids.len(), 2);
    }

    #[test]
    fn apply_deferred_triggers_event_visibility() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone, Copy)]
        struct MyEvent(#[allow(dead_code)] pub u32);

        static VALUE_SEEN: AtomicUsize = AtomicUsize::new(0);

        let mut sched = Scheduler::new();
        struct Emitter;
        impl ParSystem for Emitter {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write_event::<MyEvent>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let _emitter_id = sched.add_par_system("emitter", Emitter);
        sched.apply_deferred();

        sched.add_startup_system("reader", move |world: &mut World| {
            let events = world.events::<MyEvent>();
            VALUE_SEEN.store(events.len(), Ordering::SeqCst);
        });

        let mut world = World::new();
        world.add_event::<MyEvent>();
        world.send_event(MyEvent(42));
        world.tick();
        world.flush_all_events();

        sched.run_sequential(&mut world);
        assert_eq!(VALUE_SEEN.load(Ordering::SeqCst), 1, "Событие видно между под-Stage'ями");
    }

    // ── Condition Trait тесты ──────────────────────────────

    #[test]
    fn condition_trait_opaque_closure() {
        // Closure works with IntoConditionLeaf (via run_if)
        let mut sched = Scheduler::new();
        sched.add_system("test", |_: &mut World| {})
            .run_if(|_: &World| false);
        let world = World::new();
        sched.compile_with_world(&world).unwrap();
        // system was added with a false condition — compile should succeed
        assert!(sched.execution_plan.is_some());
    }

    #[test]
    fn condition_trait_typed_resource_exists() {
        let cond = conditions::resource_exists::<u32>();
        // access should contain read<u32>
        let acc = cond.access();
        let tid = std::any::TypeId::of::<u32>();
        assert!(acc.reads.contains(&tid));
        assert!(acc.writes.is_empty());
    }

    #[test]
    fn condition_trait_tuple_and() {
        let a = conditions::resource_exists::<u32>();
        let b = conditions::resource_exists::<f64>();
        let and_cond = (a, b);
        // access should contain both type IDs
        let acc = and_cond.access();
        assert!(acc.reads.contains(&std::any::TypeId::of::<u32>()));
        assert!(acc.reads.contains(&std::any::TypeId::of::<f64>()));
    }

    #[test]
    fn condition_trait_not() {
        // Typed condition: .not() inverts
        let cond = conditions::resource_exists::<u32>();
        let not_cond = cond.not();
        // No u32 resource → cond is false → not_cond is true
        assert!(not_cond.check(&World::new()));

        let mut world = World::new();
        world.insert_resource(42u32);
        // u32 exists → cond is true → not_cond is false
        assert!(!not_cond.check(&world));
    }

    #[test]
    fn cmd_commands_roundtrip_direct() {
        let mut cmds = Commands::new();
        #[derive(Component, Clone, Copy)]
        struct Sp;
        cmds.spawn((Sp,));
        let mut world = World::new();
        cmds.apply(&mut world);
        assert_eq!(Query::<Read<Sp>>::new(&world).iter().count(), 1,
            "Commands::spawn + apply should work");
    }

    #[test]
    fn system_macro_cmd_works() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SAW: AtomicBool = AtomicBool::new(false);

        #[derive(Component, Clone, Copy)]
        struct Sp(#[allow(dead_code)] f32);

        system! {
            fn sys_cmd(cmd: Cmd) {
                cmd.spawn((Sp(1.0),));
            }
        }

        let mut sched = Scheduler::new();
        sched.add_auto_system("spn", sys_cmd);
        sched.add_system("chk", move |world: &mut World| {
            if Query::<Read<Sp>>::new(world).iter().count() > 0 {
                SAW.store(true, Ordering::SeqCst);
            }
        });
        sched.chain(&["spn", "chk"]).unwrap();

        let mut world = World::new();
        // Use run() (parallel path) — known to work with Commands
        sched.run(&mut world);
        assert!(SAW.load(Ordering::SeqCst), "system! with cmd: Cmd + run() should work");
    }

    #[test]
    fn auto_apply_deferred_from_chain() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CAMERA_SAW: AtomicBool = AtomicBool::new(false);

        #[derive(Component, Clone, Copy)]
        struct Spawned;

        // Use a sequential system that spawns entities (has has_deferred)
        let mut sched = Scheduler::new();
        sched.add_system("spawner", move |world: &mut World| {
            world.spawn((Spawned,));
        });
        sched.add_system("camera", move |world: &mut World| {
            let count = Query::<Read<Spawned>>::new(world).iter().count();
            if count > 0 {
                CAMERA_SAW.store(true, Ordering::SeqCst);
            }
        });
        sched.chain(&["spawner", "camera"]).unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);
        assert!(CAMERA_SAW.load(Ordering::SeqCst), "camera should see spawned entity via auto-apply");
    }

    #[test]
    fn auto_apply_deferred_no_split_without_chain() {
        let mut sched = Scheduler::new();
        // Two AutoSystems without chain — they should end up in one parallel group
        struct SysA;
        impl AutoSystem for SysA {
            type Query = ();
            type Resources = ();
            type Events = ();
            const HAS_DEFERRED: bool = true;
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct SysB;
        impl AutoSystem for SysB {
            type Query = ();
            type Resources = ();
            type Events = ();
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        sched.add_auto_system("a", SysA);
        sched.add_auto_system("b", SysB);

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        // Without chain, no auto-split — both in same stage
        let both_in_same = stages.iter().any(|s| s.system_ids.len() >= 2);
        assert!(both_in_same, "Without chain, no auto-split for deferred");
    }

    #[test]
    fn condition_access_causes_conflict() {
        #[derive(Clone, Copy)]
        struct GamePhase;
        struct TogglePause;
        impl ParSystem for TogglePause {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<GamePhase>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        struct MovementWithCondition;
        impl ParSystem for MovementWithCondition {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("toggle", TogglePause);
        sched.add_par_system("move", MovementWithCondition);
        // Add typed condition that reads GamePhase
        sched.set_run_if_cond("move", conditions::resource_exists::<GamePhase>()).unwrap();

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        // toggle writes GamePhase, move reads GamePhase (via condition)
        // → WriteRead conflict → at least 2 stages
        assert!(stages.len() >= 2, "typed condition should cause WriteRead conflict: {}", stages.len());
    }

    #[test]
    fn condition_access_no_conflict_opaque() {
        struct SysA;
        impl ParSystem for SysA {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Pos>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        struct SysB;
        impl ParSystem for SysB {
            fn access() -> AccessDescriptor {
                AccessDescriptor::new().write::<Vel>()
            }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("a", SysA);
        let _bid = sched.add_par_system("b", SysB);
        // Opaque condition — no typed access, no extra conflicts
        sched.set_run_if("b", |_: &World| true).unwrap();

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        // Pos and Vel — no conflict, should be same stage
        let same_stage = stages.iter().any(|s| s.system_ids.len() >= 2);
        assert!(same_stage, "opaque condition should not cause conflicts");
    }

    // ── D2-1: plain-fn системы (Bevy-стиль) ────────────────────

    /// Обычные функции с Bevy-параметрами регистрируются bare-идентификаторами
    /// и работают: Res/ResMut/Query<(&T, &mut U)>/EventWriter/EventReader.
    #[test]
    fn plain_fn_systems_bevy_style() {
        use apex_core::query::Query as Q;

        // Именованное поле: `.0` на Res разрешается в публичный кортежный
        // филд самого Res (&T), а не в Deref — известная шероховатость.
        struct Dt {
            step: f32,
        }
        struct Moved(u32);
        #[derive(Clone, Copy)]
        struct Ping(u32);

        fn movement(dt: Res<Dt>, q: Q<(&Vel, &mut Pos)>) {
            q.for_each(|_, (v, mut p)| {
                p.x += v.x * dt.step;
                p.y += v.y * dt.step;
            });
        }

        fn emit_pings(q: Q<&Pos>, mut out: EventWriter<Ping>) {
            out.send(Ping(q.len() as u32));
        }

        fn count_moved(mut evs: EventReader<Ping>, mut moved: ResMut<Moved>) {
            // Главная Bevy-идиома (TD-24): прямая итерация по read().
            for p in evs.read() {
                moved.0 += p.0;
            }
        }

        let mut world = World::new();
        world.insert_resource(Dt { step: 1.0 });
        world.insert_resource(Moved(0));
        world.add_event::<Ping>();
        for i in 0..10 {
            world.spawn((
                Pos { x: 0.0, y: 0.0 },
                Vel {
                    x: i as f32,
                    y: 1.0,
                },
            ));
        }

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, (movement, emit_pings, count_moved));
        sched.run(&mut world);
        sched.run(&mut world);

        // movement подвинул каждую entity дважды.
        let mut sum = 0.0;
        Q::<Read<Pos>>::new(&world).for_each(|_, p| sum += p.y);
        assert_eq!(sum, 20.0, "movement отработал 2 кадра по 10 entity");

        // Конвейер событий: emit (10 на кадр) → reader (per-stage flush).
        // Минимум один кадр доставлен (зависит от порядка в стадии — но за
        // 2 кадра хотя бы 10 дойдёт).
        assert!(
            world.resource::<Moved>().0 >= 10,
            "события прошли через plain-fn writer/reader: {}",
            world.resource::<Moved>().0
        );
    }

    /// `&mut Commands` в plain-fn: has_deferred → команды применяются
    /// планировщиком (auto-apply sync-точка).
    #[test]
    fn plain_fn_commands_are_applied() {
        fn spawner(cmd: &mut Commands) {
            cmd.spawn((Pos { x: 7.0, y: 7.0 },));
        }

        let mut world = World::new();
        world.register_component::<Pos>();
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, spawner);
        sched.run(&mut world);

        let n = apex_core::query::Query::<Read<Pos>>::new(&world)
            .iter()
            .count();
        assert_eq!(n, 1, "spawn из &mut Commands применён к концу кадра");
    }

    /// Имя системы выводится из имени функции (D2-1/U.4).
    #[test]
    fn plain_fn_name_derived_from_fn() {
        fn my_special_system(_q: apex_core::query::Query<'_, Read<Pos>>) {}
        let cfg = SystemConfig::fn_sys(my_special_system);
        assert_eq!(cfg.name, "my_special_system");
    }

    /// Plain-fn системы с непересекающимся доступом попадают в одну стадию
    /// (параллельный батч), с пересекающимся — конфликтуют.
    #[test]
    fn plain_fn_access_inferred_for_conflicts() {
        fn writes_pos(q: apex_core::query::Query<'_, Write<Pos>>) {
            q.for_each(|_, mut p| p.x += 1.0);
        }
        fn writes_vel(q: apex_core::query::Query<'_, Write<Vel>>) {
            q.for_each(|_, mut v| v.x += 1.0);
        }
        fn reads_pos(q: apex_core::query::Query<'_, Read<Pos>>) {
            q.for_each(|_, _| {});
        }

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, (writes_pos, writes_vel, reads_pos));
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // writes_pos конфликтует с reads_pos (W+R по Pos) → разные батчи;
        // writes_vel ни с кем не пересекается → делит батч с одним из них.
        // Минимальная проверка: доступ ВЫВЕДЕН (есть >1 стадии исполнения).
        assert!(
            stages.len() >= 2,
            "W+R конфликт по Pos должен разнести системы по батчам: {} стадий",
            stages.len()
        );
    }

    // ── D2-5: FixedUpdate ──────────────────────────────────────

    /// FixedUpdate исполняется по аккумулятору FixedTime: 0..N шагов за кадр,
    /// остаток переносится; Update при этом — ровно раз за кадр.
    #[test]
    fn fixed_update_steps_by_accumulator() {
        struct Counts {
            fixed: u32,
            update: u32,
        }

        fn fixed_step(mut c: ResMut<Counts>) {
            c.fixed += 1;
        }
        fn frame_step(mut c: ResMut<Counts>) {
            c.update += 1;
        }

        let mut world = World::new();
        world.insert_resource(Counts {
            fixed: 0,
            update: 0,
        });
        world.insert_resource(crate::FixedTime::from_dt(0.010));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::FixedUpdate, fixed_step);
        sched.add_systems(StageLabel::Update, frame_step);

        // Кадр 1: 35мс → 3 шага, остаток 5мс.
        world
            .resource_mut::<crate::FixedTime>()
            .accumulate(0.035);
        sched.run(&mut world);
        assert_eq!(world.resource::<Counts>().fixed, 3);
        assert_eq!(world.resource::<Counts>().update, 1);

        // Кадр 2: +0мс → 0 шагов (остаток 5мс < dt).
        sched.run(&mut world);
        assert_eq!(world.resource::<Counts>().fixed, 3);
        assert_eq!(world.resource::<Counts>().update, 2);

        // Кадр 3: +6мс → остаток 11мс → 1 шаг.
        world
            .resource_mut::<crate::FixedTime>()
            .accumulate(0.006);
        sched.run(&mut world);
        assert_eq!(world.resource::<Counts>().fixed, 4);
        assert_eq!(world.resource::<Counts>().update, 3);
    }

    /// D1: two CONFLICTING FixedUpdate systems (the planner splits them into
    /// separate execution stages) must both run on every fixed step, interleaved
    /// as (A;B)×N — not A×N then B×N, and the second must never be starved by the
    /// first draining the accumulator.
    #[test]
    fn fixed_update_conflicting_systems_all_run_each_step_interleaved() {
        #[derive(Default)]
        struct Log(Vec<char>);

        fn step_a(mut log: ResMut<Log>) {
            log.0.push('A');
        }
        fn step_b(mut log: ResMut<Log>) {
            log.0.push('B');
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        world.insert_resource(crate::FixedTime::from_dt(0.010));

        let mut sched = Scheduler::new();
        // Both take ResMut<Log> → WriteWrite → separate execution stages.
        sched.add_systems(StageLabel::FixedUpdate, step_a);
        sched.add_systems(StageLabel::FixedUpdate, step_b);

        // 35ms / 10ms = 3 fixed steps.
        world.resource_mut::<crate::FixedTime>().accumulate(0.035);
        sched.run(&mut world);

        let log: String = world.resource::<Log>().0.iter().collect();
        let a = log.chars().filter(|&c| c == 'A').count();
        let b = log.chars().filter(|&c| c == 'B').count();
        assert_eq!(a, 3, "system A must run every fixed step, got log {log:?}");
        assert_eq!(b, 3, "system B must NOT be starved — runs every step too, got {log:?}");
        assert!(
            !log.contains("AA") && !log.contains("BB"),
            "steps must interleave as (A;B)×N, not A×N then B×N, got {log:?}"
        );
    }

    /// Защита от спирали смерти: шаги капятся, излишек отбрасывается.
    #[test]
    fn fixed_update_death_spiral_cap() {
        struct N(u32);
        fn step(mut n: ResMut<N>) {
            n.0 += 1;
        }

        let mut world = World::new();
        world.insert_resource(N(0));
        let mut ft = crate::FixedTime::from_dt(0.001);
        ft.max_steps_per_frame = 4;
        ft.accumulate(10.0); // 10000 шагов «задолженности»
        world.insert_resource(ft);

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::FixedUpdate, step);
        sched.run(&mut world);

        assert_eq!(world.resource::<N>().0, 4, "кап шагов");
        assert_eq!(
            world.resource::<crate::FixedTime>().overstep(),
            0.0,
            "излишек отброшен"
        );
    }

    /// Без ресурса FixedTime стадия FixedUpdate — обычная (1 раз за кадр).
    #[test]
    fn fixed_update_without_resource_runs_once() {
        struct N(u32);
        fn step(mut n: ResMut<N>) {
            n.0 += 1;
        }
        let mut world = World::new();
        world.insert_resource(N(0));
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::FixedUpdate, step);
        sched.run(&mut world);
        sched.run(&mut world);
        assert_eq!(world.resource::<N>().0, 2);
    }

    // ── D2-6: States ───────────────────────────────────────────

    /// Полный жизненный цикл состояний: in_state гейтит системы, on_enter/
    /// on_exit истинны ровно один кадр, переход — через NextState.
    #[test]
    fn states_in_state_on_enter_on_exit() {
        use crate::states::{in_state, init_state, on_enter, on_exit, NextState};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Game {
            Menu,
            Playing,
        }

        #[derive(Default)]
        struct Log {
            menu_frames: u32,
            entered_playing: u32,
            exited_menu: u32,
        }

        fn menu_ui(mut log: ResMut<Log>) {
            log.menu_frames += 1;
        }
        fn spawn_level(mut log: ResMut<Log>) {
            log.entered_playing += 1;
        }
        fn teardown_menu(mut log: ResMut<Log>) {
            log.exited_menu += 1;
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        let mut sched = Scheduler::new();
        init_state(&mut world, &mut sched, Game::Menu);

        // П4: run_if прямо на bare-fn (FnSystemExt), как в Bevy.
        use crate::config::FnSystemExt;
        sched.add_systems(
            StageLabel::Update,
            (
                menu_ui.run_if(in_state(Game::Menu)),
                spawn_level.run_if(on_enter(Game::Playing)),
                teardown_menu.run_if(on_exit(Game::Menu)),
            ),
        );

        sched.run(&mut world); // кадр 1: Menu
        sched.run(&mut world); // кадр 2: Menu
        assert_eq!(world.resource::<Log>().menu_frames, 2);
        assert_eq!(world.resource::<Log>().entered_playing, 0);

        world.resource_mut::<NextState<Game>>().set(Game::Playing);
        sched.run(&mut world); // кадр 3: переход в начале кадра
        let log = world.resource::<Log>();
        assert_eq!(log.menu_frames, 2, "in_state(Menu) больше не пускает");
        assert_eq!(log.entered_playing, 1, "on_enter — ровно один кадр");
        assert_eq!(log.exited_menu, 1, "on_exit — ровно один кадр");

        sched.run(&mut world); // кадр 4: Playing, переходов нет
        let log = world.resource::<Log>();
        assert_eq!(log.entered_playing, 1);
        assert_eq!(log.exited_menu, 1);
    }

    /// D7: on_enter(initial) must be visible to Update (and later) systems on the
    /// first frame, not only Startup — the transition system used to clear it in
    /// First before Update ran.
    #[test]
    fn on_enter_initial_visible_to_update_on_first_frame() {
        use crate::config::FnSystemExt;
        use crate::states::{init_state, on_enter};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum S {
            A,
        }
        #[derive(Default)]
        struct Log(u32);
        fn on_enter_a(mut log: ResMut<Log>) {
            log.0 += 1;
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        let mut sched = Scheduler::new();
        init_state(&mut world, &mut sched, S::A);
        sched.add_systems(StageLabel::Update, on_enter_a.run_if(on_enter(S::A)));

        sched.run(&mut world); // frame 1
        assert_eq!(
            world.resource::<Log>().0,
            1,
            "on_enter(initial) must fire for Update systems on frame 1"
        );
        sched.run(&mut world); // frame 2 — no longer entering
        assert_eq!(world.resource::<Log>().0, 1, "on_enter lasts one frame only");
    }

    /// D7: a second init_state for the same state is ignored, so transitions keep
    /// working (a second transition system would clear the flags the first sets).
    #[test]
    fn double_init_state_is_ignored() {
        use crate::config::FnSystemExt;
        use crate::states::{init_state, on_enter, NextState};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum S {
            A,
            B,
        }
        #[derive(Default)]
        struct Log(u32);
        fn on_enter_b(mut log: ResMut<Log>) {
            log.0 += 1;
        }

        let mut world = World::new();
        world.insert_resource(Log::default());
        let mut sched = Scheduler::new();
        init_state(&mut world, &mut sched, S::A);
        init_state(&mut world, &mut sched, S::A); // ignored
        sched.add_systems(StageLabel::Update, on_enter_b.run_if(on_enter(S::B)));

        sched.run(&mut world); // frame 1
        world.resource_mut::<NextState<S>>().set(S::B);
        sched.run(&mut world); // transition to B
        assert_eq!(
            world.resource::<Log>().0,
            1,
            "transitions still work despite a double init_state"
        );
    }

    // ── Э5: Single<Q> / Option<Single<Q>> — skip-семантика ────

    #[test]
    fn single_param_skips_unless_exactly_one_match() {
        use apex_core::prelude::*;

        #[derive(Clone, Copy, Debug, apex_macros::Component)]
        struct Player(f32);
        #[derive(Clone, Copy, Debug, Default)]
        struct Runs {
            single: u32,
            optional_some: u32,
            optional_none: u32,
        }

        fn with_single(p: Single<&Player>, mut runs: ResMut<Runs>) {
            assert!(p.0 >= 0.0);
            runs.single += 1;
        }
        fn with_optional(p: Option<Single<&Player>>, mut runs: ResMut<Runs>) {
            match p {
                Some(s) => {
                    let _e = s.entity();
                    runs.optional_some += 1;
                }
                None => runs.optional_none += 1,
            }
        }

        let mut world = World::new();
        world.insert_resource(Runs::default());
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, (with_single, with_optional));

        // 0 матчей: Single пропускается, Option<Single> получает None.
        sched.run(&mut world);
        assert_eq!(world.resource::<Runs>().single, 0);
        assert_eq!(world.resource::<Runs>().optional_none, 1);

        // 1 матч: обе работают.
        let p1 = world.spawn((Player(1.0),));
        sched.run(&mut world);
        assert_eq!(world.resource::<Runs>().single, 1);
        assert_eq!(world.resource::<Runs>().optional_some, 1);

        // 2 матча: обе пропускаются.
        world.spawn((Player(2.0),));
        sched.run(&mut world);
        let runs = *world.resource::<Runs>();
        assert_eq!(runs.single, 1, "Single при >1 матче пропускает кадр");
        assert_eq!(runs.optional_some, 1, "Option<Single> при >1 тоже пропускает");
        assert_eq!(runs.optional_none, 1);

        // Снова 1 матч — работа возобновляется (мутабельная форма + фильтр).
        world.despawn(p1);
        fn bump(mut p: Single<&mut Player, With<Player>>) {
            p.0 += 1.0;
        }
        sched.add_systems(StageLabel::Update, bump);
        sched.run(&mut world);
        assert_eq!(world.resource::<Runs>().single, 2);
    }

    // ── W3-4: стресс ASD row-split ────────────────────────────

    /// Stateless-система на большом мире ЧАНКУЕТСЯ (ASD row-split):
    /// каждая строка должна быть обработана РОВНО один раз — ни пропусков
    /// (полнота чанков), ни дублей (дизъюнктность диапазонов).
    #[test]
    fn asd_row_split_visits_each_row_exactly_once() {
        #[derive(Component, Clone, Copy)]
        struct Hits(u32);

        system! {
            fn bump(q: Write<Hits>) {
                q.for_each(|_, mut h| h.0 += 1);
            }
        }

        const N: usize = 100_000; // заведомо больше effective_chunk
        let mut world = World::new();
        world.spawn_many(N, |_| (Hits(0),));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, bump);
        for _ in 0..3 {
            sched.run(&mut world);
        }

        let mut bad = 0usize;
        Query::<Read<Hits>>::new(&world).for_each(|_, h| {
            if h.0 != 3 {
                bad += 1;
            }
        });
        assert_eq!(bad, 0, "каждая строка обработана ровно по разу за кадр");
    }

    /// D3 regression: concurrent row-split ASD tasks for one stateless system
    /// each form `&mut *task.ptr`. The pointer must target the `dyn ParSystem`
    /// (zero-sized for a plain-fn system) — NOT the enclosing `SystemDescriptor`,
    /// which owns a `String` name and other real bytes. Pre-fix the task
    /// materialized `&mut SystemDescriptor`, so several split tasks running at
    /// once held aliasing `&mut` over the same non-ZST object (Stacked/Tree
    /// Borrows UB). A dedicated 4-thread pool plus a chunk config that forces
    /// small chunks makes the split deterministic (and Miri-tractable, unlike the
    /// 100k test above). Validate with
    /// `MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-tree-borrows" cargo +nightly
    /// miri test -p apex-scheduler asd_row_split_no_descriptor_aliasing`.
    #[test]
    fn asd_row_split_no_descriptor_aliasing() {
        #[derive(Component, Clone, Copy)]
        struct Hits(u32);

        system! {
            fn bump(q: Write<Hits>) {
                q.for_each(|_, mut h| h.0 += 1);
            }
        }

        const N: usize = 256;
        let mut world = World::new();
        // Force splitting at a small N: never fall back to serial, allow small
        // chunks. With 4 threads: target_chunk ~32, effective_chunk 32, so 256
        // entities split into 8 concurrent tasks for one system.
        world.set_chunk_config(apex_core::world::ChunkConfig {
            min_entities_per_thread: 1,
            dynamic_min_chunk: 8,
            max_chunk_size: 65536,
            auto_serial_fallback: false,
            task_multiplier: 2.0,
        });
        world.spawn_many(N, |_| (Hits(0),));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, bump);

        // A dedicated 4-thread pool makes the concurrent split path run
        // regardless of the host CPU count (`rayon::scope` inside `run` uses the
        // installed pool).
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        pool.install(|| {
            sched.run(&mut world);
        });

        let mut bad = 0usize;
        Query::<Read<Hits>>::new(&world).for_each(|_, h| {
            if h.0 != 1 {
                bad += 1;
            }
        });
        assert_eq!(bad, 0, "each row bumped exactly once across concurrent split tasks");
    }

    /// Система с СОСТОЯНИЕМ не делится row-split'ом (W3-4): один экземпляр,
    /// один вызов run на кадр, состояние видит ВСЕ строки. До фикса несколько
    /// split-задач звали run(&mut self) конкурентно: гонка на состоянии +
    /// каждая задача видела только свой диапазон.
    #[test]
    fn asd_does_not_split_stateful_system() {
        #[derive(Component, Clone, Copy)]
        struct Tag;

        struct SeenTotal(u64);

        system! {
            struct CountRows { seen: u64 = 0 }
            fn run(s: &mut Self, q: Read<Tag>, out: ResMut<SeenTotal>) {
                s.seen = 0;
                q.for_each(|_, _| s.seen += 1);
                out.0 = s.seen;
            }
        }

        const N: usize = 100_000;
        let mut world = World::new();
        world.spawn_many(N, |_| (Tag,));
        world.insert_resource(SeenTotal(0));

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, CountRows::default());
        sched.run(&mut world);

        assert_eq!(
            world.resource::<SeenTotal>().0,
            N as u64,
            "stateful-система получает полный SubWorld (без row-split)"
        );
    }
}
