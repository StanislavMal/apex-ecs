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
    FnSystemMarker, IntoScheduleConfigs, ScheduleConfigs, ScheduleConfigsMarker, SystemConfig,
};
pub mod fixed;
pub use fixed::FixedTime;
pub mod states;
pub use states::{in_state, init_state, on_enter, on_exit, NextState, State, StateTransitions, States};

/// Prelude — golden-path scheduler API (`use apex_scheduler::prelude::*`).
///
/// The core ECS golden path is in `apex_core::prelude`; this adds scheduling:
/// `Scheduler`, stages, `add_systems` config (`sys`/`seq`/`par`/`par_access` +
/// `SystemConfig`), run conditions/states, and the fixed-timestep clock.
pub mod prelude {
    pub use crate::config::{
        par, par_access, seq, sys, IntoScheduleConfigs, ScheduleConfigs, SystemConfig,
    };
    pub use crate::fixed::FixedTime;
    pub use crate::stage::{Stage, StageLabel};
    pub use crate::states::{
        in_state, init_state, on_enter, on_exit, NextState, State, StateTransitions, States,
    };
    pub use crate::Scheduler;
}

mod config;
use crate::config::SystemConfigKind;

// `impl Scheduler` is split across these modules (child modules see the crate
// root's private items). Each contributes one `impl Scheduler` block:
mod compile;
mod debug;
mod executor;
mod registration;

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
    /// D8b: pointer to this system's private per-system `Commands` slot, set ONLY
    /// for single-task (per-system scope, `chunk_ranges` empty) tasks — a command-
    /// emitting system is never row-split, so the slot has a unique writer. Row-split
    /// (query-only) tasks carry `None` and use an inline buffer (they emit no
    /// commands). Applying slots in rank order gives deterministic command ordering.
    cmds: Option<SendPtr<Commands>>,
}

unsafe impl Send for AsdTask {}
unsafe impl Sync for AsdTask {}

/// Per-system dispatch info collected once per parallel stage (Ш1: pooled in a
/// Scheduler scratch buffer, reused across frames). Holds the system's `sys_id`
/// rather than a clone of its archetype-index list — the slice is fetched from
/// `system_archetype_indices` at task-build time, so no per-system `Vec` is
/// allocated per frame. Main-thread only (not sent to rayon), so no `Send`.
struct SysInfo {
    ptr: SendPtr<dyn ParSystem>,
    sys_id: SystemId,
    entity_count: usize,
    has_events: bool,
    uses_par_for_each: bool,
    needs_whole_world: bool,
    stateful: bool,
    /// Mutates a resource (`ResMut`) or uses `Commands` — cannot be row-split
    /// (the body would run once per chunk ⇒ side-effect multiplied). See TD-37.
    non_query_side_effects: bool,
}

// ── Ш2: cost-model thresholds ──────────────────────────────────
//
// The scheduler decides SEQ vs PAR from MEASURED work (µs), not entity count
// (Д1/Д2): entity thresholds cannot tell light work (1 ns/entity) from heavy
// (500 ns/entity), so they mis-fire both ways. A stage whose dispatch-time EMA
// is below `T_STAGE_SEQ_NS` runs sequentially — below it, rayon scope + per-task
// overhead exceeds the parallel speedup ("valley of death", parallel_diagnostics).
// Defaults are tuned on a 12-thread machine; `ParallelPolicy::Fixed` (entity
// heuristic only) remains available as a fallback.

/// Below this measured stage EMA (ns), run the stage sequentially.
const T_STAGE_SEQ_NS: f64 = 40_000.0; // 40µs
/// Hysteresis band (±20%) around the threshold to prevent SEQ↔PAR flapping.
const STAGE_HYSTERESIS: f64 = 0.2;
/// EMA smoothing factor (higher = faster adaptation, noisier).
const STAGE_EMA_ALPHA: f64 = 0.2;

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
}

// ── SystemBuilder ──────────────────────────────────────────────

/// Test-only builder returned by the `add_*_to_stage` registration helpers.
/// The golden-path public API is `add_systems` + [`SystemConfig`] (with
/// `.run_if()`/`.before()`/`.after()`/`.chain()`), which fully superseded
/// builder-chaining; the builder now exists only as test-authoring sugar and is
/// `#[cfg(test)]` (zero footprint in the shipped library — CONVENTIONS §2).
///
/// Holds `&'a mut Scheduler` (not a raw `*mut`): the lifetime guarantees the
/// builder cannot outlive the scheduler — a stored builder + a dropped
/// `Scheduler` would be potential UB with a raw pointer (D10).
#[cfg(test)]
pub(crate) struct SystemBuilder<'a> {
    scheduler: &'a mut Scheduler,
    id: SystemId,
}

#[cfg(test)]
impl<'a> SystemBuilder<'a> {
    pub(crate) fn id(self) -> SystemId {
        self.id
    }

    fn add_condition_leaf(self, leaf: ConditionTree, condition_access: AccessDescriptor) -> Self {
        {
            let sched = &mut *self.scheduler;
            if let Some(sys) = sched.system_by_id_mut(self.id) {
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

    /// Прикрепить run condition — closure.
    pub(crate) fn run_if<F>(self, condition: F) -> Self
    where
        F: Fn(&World) -> bool + Send + Sync + 'static,
    {
        let leaf = ConditionTree::leaf(condition);
        self.add_condition_leaf(leaf, AccessDescriptor::new())
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

// ── OrderEndpoint ──────────────────────────────────────────────

/// One endpoint of a config-declared ordering edge (`.before()`/`.after()`/
/// `.chain()`). Positional endpoints (chain) resolve to `Id` at registration;
/// named endpoints (`.after("x")`) stay `Name` until `compile()` so that
/// forward references (a system named later in another `add_systems` call)
/// resolve correctly.
enum OrderEndpoint {
    Id(SystemId),
    Name(String),
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

    // ── Ш2: cost-model telemetry (per-execution-stage), keyed like stage_last_run ──
    /// EMA of measured stage dispatch time (ns). Drives the cost-based SEQ/PAR
    /// decision (Д1/Д2): a parallel-eligible stage whose EMA is below the threshold
    /// is run sequentially, because rayon scope + per-task overhead exceeds the
    /// parallel win on light work (the "valley of death"). `0.0` = no history yet
    /// (first frames fall back to the entity-count heuristic). Lazily sized;
    /// cleared on `compile` (indices can change).
    stage_cost_ema_ns: Vec<f64>,
    /// Whether the stage ran sequentially last time — hysteresis to avoid SEQ↔PAR
    /// flapping at the threshold. Lazily sized; cleared on `compile`.
    stage_ran_seq: Vec<bool>,

    // ── Конфигурация параллелизма ───────────────────────────────
    // Stage-parallelism gating moved to `ChunkConfig` on the World (wave 3,
    // §1.7 single-config model): the scheduler reads
    // `world.chunk_config().stage_parallel_min_entities` /
    // `.auto_disable_stage_parallel` at stage-decision time.

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
    /// Config-declared ordering edges awaiting name resolution, drained at the
    /// start of `compile()`. Each pair is `(before, after)`: `before` runs
    /// before `after`. Populated by `add_systems` from `.before()`/`.after()`/
    /// `.chain()` on configs; `Name` endpoints are resolved via `find_id_by_name`
    /// at compile so forward references work. Deterministic (insertion order).
    pending_orderings: Vec<(OrderEndpoint, OrderEndpoint)>,

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

    // ── Ш1: pooled per-frame scratch buffers (zero steady-state alloc) ──────────
    // NOTE: per-worker `Vec<Commands>` is intentionally NOT pooled here — `Commands`
    // is `!Send`, and a `Vec<Commands>` field would make `Scheduler: !Send` (breaks
    // `ThreadPool::install`). Its outer-Vec alloc is negligible; it stays a local.
    /// Reused `arch_lengths` snapshot (cleared + refilled per frame).
    scratch_arch_lengths: Vec<usize>,
    /// Reused execution schedule (FixedUpdate expansion) index list.
    scratch_schedule: Vec<usize>,
    /// Reused per-parallel-stage system-info buffer.
    scratch_sys_infos: Vec<SysInfo>,
    /// Reused per-parallel-stage ASD task buffer.
    scratch_tasks: Vec<AsdTask>,
    /// Reused per-parallel-stage skipped-system set.
    scratch_skipped: FxHashSet<SystemId>,

    // ── D8b: deterministic parallel spawn (opt-in) ──────────────────────────────
    /// When true, every command-emitting system in a stage is seeded with a private
    /// entity-id block at a rank-deterministic base, so parallel `Commands` spawns
    /// assign IDENTICAL ids run-to-run given the same start snapshot + inputs — a
    /// record/replay / rollback-netcode primitive Bevy does not provide. Because the
    /// blocks are seeded regardless of the SEQ/PAR path the cost-model picks, id
    /// assignment is path-independent (timing does not perturb ids). Default false;
    /// see the guarantee boundary (steady-state / no-frame-1-overflow) in the guide.
    deterministic_spawn: bool,
    /// D8b: per-system observed spawn count (ids drawn from its block last stage),
    /// for adaptive block sizing (next = observed*2, clamped). Persists across
    /// frames. `SystemId`/`u32` are `Send`, so this is safe as a field (unlike a
    /// `Vec<Commands>`, which would make `Scheduler: !Send`).
    system_spawn_history: FxHashMap<SystemId, u32>,
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
mod tests;
