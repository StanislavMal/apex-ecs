//! apex-scheduler — a hybrid system scheduler.
//!
//! # Improvements over the previous version
//!
//! ## 1. `add_auto_system` — register an AutoSystem without a manual access
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
//! // AccessDescriptor is derived statically — you cannot forget a component
//! ```
//!
//! ## 2. `ConflictKind` on graph edges — verbose diagnostics
//!
//! `debug_plan_verbose()` shows WHY systems are in different stages:
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
//! ## 3. Incremental graph — adding systems without a full recompute
//!
//! On `add_*_system` the graph is not recomputed immediately.
//! The topo sort runs lazily on the first `run()` or an explicit `compile()`.
//! Adding a new system adds only new nodes/edges.
//!
//! ## 4. `par_for_each` in SystemContext (in apex-core/world.rs)
//!
//! Intra-system parallelism across archetypes via Rayon.
//!
//! # System kinds
//!
//! | Kind | Access | Use |
//! |-----|--------|---------------|
//! | AutoSystem | auto-derived from Query + Resources + Events | recommended |
//! | FnParSystem | explicit + closure | quick prototypes |
//! | Sequential | full &mut World | structural changes |

// The scheduler is high-performance code with internal `unsafe` (ASD, SendPtr,
// rayon::scope). Some lints are relaxed on purpose (see the similar block in apex-core).
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

// ── Main-world per-system profiler (`APEX_MAIN_PROF=1`) ────────────────────
//
// Mirror of the render `APEX_PROF`: the render world and extract have long had a breakdown,
// while main-world exclusive systems were only measured by ad-hoc wrappers in examples.
// Logs every ~2s: mean ms/call per system, sorted descending.

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
        log::info!("MAIN PROF (ms/call, avg ~2s): {line}");
        for e in acc.iter_mut() {
            e.1 = 0.0;
            e.2 = 0;
        }
    }
}

/// Every ~2s — a summary of the world's archetypes (CR-M4: live-row counter in the
/// `APEX_MAIN_PROF` report; growth in empty/archetypes signals fragmentation).
fn main_prof_world_stats(world: &apex_core::World) {
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if last.map(|t| t.elapsed().as_secs_f32() > 2.0).unwrap_or(true) {
        *last = Some(Instant::now());
        let s = world.archetype_stats();
        log::info!(
            "MAIN PROF world: archetypes={} (empty={}), rows={}, max_arch_rows={}, \
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

/// The reason for a dependency between two systems in the graph.
///
/// Stored on `dependency_graph` edges for verbose diagnostics.
/// Lets `debug_plan_verbose()` explain WHY systems
/// ended up in different stages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Explicit dependency via `add_dependency()`
    Explicit,
    /// Both write the same component — a Write+Write conflict
    WriteWrite { component_name: &'static str },
    /// One writes, the other reads — a Write+Read conflict
    WriteRead {
        component_name: &'static str,
        writer_id: u32,
        reader_id: u32,
    },
    /// Sequential barrier — a system with a full &mut World
    SequentialBarrier,
    /// Two EventWriters of the same event type
    EventWriteWrite { event_name: &'static str },
    /// An EventWriter and an EventReader of the same event type
    EventWriteRead {
        event_name: &'static str,
        writer_id: u32,
        reader_id: u32,
    },
    /// Both systems write components that the other reads
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

// ── ConditionTree — composite run conditions ────────────

/// Condition tree: decides whether a system should run this frame.
///
/// Evaluated on the main thread **before** the stage runs.
/// Supports AND (all children true) and OR (at least one true).
///
/// # Composition
///
/// ```ignore
/// // AND — several run_if in a row:
/// s.run_if(cond_a).run_if(cond_b)        → And([Leaf(a), Leaf(b)])
///
/// // OR — via or_else:
/// s.or_else(cond_a).or_else(cond_b)      → Or([Leaf(a), Leaf(b)])
/// ```
///
/// # Evaluation
///
/// AND short-circuits: as soon as one condition is false, the rest are not checked.
/// OR short-circuits: as soon as one is true, the rest are not checked.
/// This is safe because Apex conditions are stateless (unlike Bevy).
pub enum ConditionTree {
    Leaf(Box<dyn Fn(&World) -> bool + Send + Sync>),
    And(Vec<ConditionTree>),
    Or(Vec<ConditionTree>),
}

impl ConditionTree {
    /// Evaluate the condition tree for the given world.
    pub fn evaluate(&self, world: &World) -> bool {
        match self {
            ConditionTree::Leaf(f) => f(world),
            ConditionTree::And(conds) => conds.iter().all(|c| c.evaluate(world)),
            ConditionTree::Or(conds) => conds.iter().any(|c| c.evaluate(world)),
        }
    }

    /// Create a Leaf from a closure.
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

// SAFETY: usage is strictly limited to run_hybrid_parallel where
// pointer uniqueness is guaranteed — each ptr comes from a unique index.
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

/// A task for ASD (Adaptive Scope Distribution).
///
/// Holds a pointer to the system (`dyn ParSystem`), a pointer to the system's
/// archetype slice, and the row ranges this task must process.
/// Each task processes a subset of the system's entities.
///
/// If `chunk_ranges` is empty, the task processes all of the system's entities
/// (the whole SubWorld, unrestricted).
#[allow(dead_code)]
struct AsdTask {
    /// Pointer to the `dyn ParSystem` itself — NOT the enclosing
    /// `SystemDescriptor`. Row-split tasks for one system run concurrently and
    /// each forms `&mut *ptr`; targeting the trait object (a ZST for the only
    /// systems that are ever split — stateless plain-fn adapters) keeps those
    /// `&mut` non-aliasing over any real bytes, whereas `&mut SystemDescriptor`
    /// (which owns a `String` name, etc.) aliased UB-ly (D3).
    ptr: SendPtr<dyn ParSystem>,
    /// Archetype indices for this task.
    /// If `chunk_ranges` is empty — all of the system's archetypes.
    /// Otherwise only those present in `chunk_ranges` (narrowed for 4-arch cases).
    arch_indices: SmallVec<[usize; 8]>,
    /// Row ranges that restrict the SubWorld:
    /// `(arch_idx, start, end)` — only these rows of the given archetypes.
    /// If empty — an unrestricted SubWorld (all of the system's entities).
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

/// Per-system dispatch info collected once per parallel stage (Sh1: pooled in a
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

// ── Sh2: cost-model thresholds ──────────────────────────────────
//
// The scheduler decides SEQ vs PAR from MEASURED work (µs), not entity count
// (D1/D2): entity thresholds cannot tell light work (1 ns/entity) from heavy
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

/// A parallel system with an explicit AccessDescriptor.
///
/// Internal mechanism — use `AutoSystem` for the public API.
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

// ── AutoSystem → ParSystem adapter ────────────────────────────

/// A wrapper that lets an AutoSystem be registered as a ParSystem.
///
/// Access comes from `S::Query::system_access()` — statically,
/// with no room for error.
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
    /// Execution stage (Update by default).
    stage_label: StageLabel,
    /// Run condition: the system runs only if the condition tree returns true.
    /// Defaults to `ConditionTree::And(Vec::new())` — always true (an empty AND = true).
    run_condition: ConditionTree,
    /// True = apply Commands after this system, splitting the stage into sub-stages.
    /// Set via `sched.apply_deferred()`.
    apply_deferred_after: bool,
    /// True = the system uses deferred operations (Commands).
    /// Determined automatically at registration.
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

    /// Attach a run condition — closure.
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

/// Edge metadata in dependency_graph for verbose diagnostics.
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

/// A hybrid scheduler with a graph-oriented compiler.
///
/// # Lifecycle
///
/// ```text
/// add_*_system()    →  systems Vec updated, plan invalidated
/// compile()         →  graph recomputed, plan ready
/// run()             →  compile() lazily if needed, then execution
/// ```
///
/// # Incrementality
///
/// Which archetypes a system needs (CR-M4).
///
/// Systems with no component accesses (resources/events only) get
/// the `All` marker instead of a materialized `Vec` of all indices — on large
/// worlds this removed a Vec<usize> the length of the archetype count per system.
#[derive(Debug, Clone)]
enum SystemArchetypes {
    /// Unrestricted — every archetype matches the system.
    All,
    /// Archetypes containing at least one of the system's components
    /// (the `any()` criterion; the Query itself filters further via matches_archetype).
    Filtered(Vec<usize>),
}

/// The dependency graph persists between `compile()` calls.
/// `dirty_systems` tracks systems added since the last compile —
/// on the next compile only new nodes/edges are added.
pub struct Scheduler {
    systems: Vec<SystemDescriptor>,
    /// Fast system lookup by SystemId: O(1) instead of O(n)
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

    // ── Sh2: cost-model telemetry (per-execution-stage), keyed like stage_last_run ──
    /// EMA of measured stage dispatch time (ns). Drives the cost-based SEQ/PAR
    /// decision (D1/D2): a parallel-eligible stage whose EMA is below the threshold
    /// is run sequentially, because rayon scope + per-task overhead exceeds the
    /// parallel win on light work (the "valley of death"). `0.0` = no history yet
    /// (first frames fall back to the entity-count heuristic). Lazily sized;
    /// cleared on `compile` (indices can change).
    stage_cost_ema_ns: Vec<f64>,
    /// Whether the stage ran sequentially last time — hysteresis to avoid SEQ↔PAR
    /// flapping at the threshold. Lazily sized; cleared on `compile`.
    stage_ran_seq: Vec<bool>,

    // ── Parallelism configuration ───────────────────────────────
    // Stage-parallelism gating moved to `ChunkConfig` on the World (wave 3,
    // §1.7 single-config model): the scheduler reads
    // `world.chunk_config().stage_parallel_min_entities` /
    // `.auto_disable_stage_parallel` at stage-decision time.

    // ── Incremental graph ────────────────────────────────────
    /// Dependency graph: nodes = SystemId, edges = ConflictKind.
    /// Persisted between compile() calls for incremental updates.
    dependency_graph: Graph<SystemId, ConflictKind>,
    /// Maps SystemId → Index in dependency_graph (for fast lookup).
    graph_nodes: FxHashMap<SystemId, Index>,
    /// O(1) edge lookup: (from, to) → exists. Kept in sync with dependency_graph.
    edge_set: FxHashSet<(Index, Index)>,
    /// Edges with full metadata — for verbose diagnostics.
    edge_info: Vec<GraphEdgeInfo>,
    /// True if systems/dependencies were added since the last compile().
    graph_dirty: bool,
    /// Pairs of systems with an explicit order (from add_dependency / .before / .after).
    /// The edge points from "earlier" to "later": (a, b) means a before b.
    explicit_orderings: FxHashSet<(SystemId, SystemId)>,
    /// Config-declared ordering edges awaiting name resolution, drained at the
    /// start of `compile()`. Each pair is `(before, after)`: `before` runs
    /// before `after`. Populated by `add_systems` from `.before()`/`.after()`/
    /// `.chain()` on configs; `Name` endpoints are resolved via `find_id_by_name`
    /// at compile so forward references work. Deterministic (insertion order).
    pending_orderings: Vec<(OrderEndpoint, OrderEndpoint)>,

    // ── Seq/Par indices for O(P) sequential barriers ────────────
    /// Indices of sequential systems in self.systems.
    seq_system_indices: Vec<usize>,
    /// Indices of parallel systems in self.systems.
    par_system_indices: Vec<usize>,

    // ── SubWorld mapping ────────────────────────────────────────
    /// Per system — the archetype indices it needs.
    /// Populated in compile() and used in run_hybrid_parallel().
    system_archetype_indices: FxHashMap<SystemId, SystemArchetypes>,
    /// The World's archetype count as of the last compute_archetype_indices().
    /// Used for caching — recompute only on change.
    cached_archetype_count: usize,

    /// Flag: whether the Startup stage has already run.
    startup_completed: bool,

    /// A custom StageLabel order for compile().
    /// If Some — compile() uses this order instead of the hardcoded standard_order().
    /// If None — StageLabel::standard_order() is used.
    stage_order: Option<Vec<StageLabel>>,

    /// Default stage for `add_system`, `add_auto_system`, `add_par`,
    /// `add_par_access` (without the `_to_stage` suffix).
    ///
    /// Defaults to `StageLabel::Update`. Changed via `set_default_stage()`
    /// or temporarily overridden inside `staged()`.
    default_stage_label: StageLabel,

    /// Registry of component names TypeId → &'static str.
    /// Populated from the ComponentRegistry before compile() in run()/run_sequential().
    /// Used by `component_type_name()` to show real component names
    /// in ConflictKind (instead of the "<component>" placeholder).
    type_names: FxHashMap<TypeId, &'static str>,

    /// Flag: whether to account for Emit<E>/Listen<E> when building the dependency graph.
    ///
    /// Defaults to `true` — event ordering is guaranteed automatically.
    /// When `false`, behavior matches the state before EventAccessList was introduced:
    /// the Emit/Listen order is undefined (as in earlier engine versions).
    event_ordering_enabled: bool,

    /// The last registered system — for `apply_deferred()`.
    last_added_system_id: Option<SystemId>,

    /// Scope condition: every system registered inside `staged()` while this
    /// condition is active inherits it (auto AND-ed with their own conditions).
    scope_condition: Option<std::sync::Arc<dyn Fn(&World) -> bool + Send + Sync>>,

    // ── Sh1: pooled per-frame scratch buffers (zero steady-state alloc) ──────────
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

// ── Helper functions ────────────────────────────────────

/// Detect a conflict between two systems.
///
/// # Check order
/// 1. Component Write+Write
/// 2. Component Write+Read (both directions)
/// 3. Event Write+Write (two Emits of the same event → conflict)
/// 4. Event Write+Read (Emit + Listen → Emit runs first)
///
/// # Event-ordering guarantee
/// The Emit(E) → Listen(E) edge guarantees that every sender of event E
/// runs before any listener of E within a single frame.
/// Two Listen(E) do not conflict — they may run in parallel.
///
/// # Control
/// If `event_ordering = false`, the event conflict checks (3, 4)
/// are skipped — the Emit/Listen order is undefined for the scheduler.
///
/// Returns (ConflictKind, direction) where direction true means i→j.
/// If there are no conflicts — None.
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

    // ── Component conflicts ──────────────────────────────────

    // Write+Write: both write the same component
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
    // Write(i)+Read(j): i writes what j reads
    let i_writes_j_reads = ai.writes.iter().any(|w| aj.reads.contains(w));
    // Write(j)+Read(i): j writes what i reads
    let j_writes_i_reads = aj.writes.iter().any(|w| ai.reads.contains(w));

    if i_writes_j_reads && j_writes_i_reads {
        // Bidirectional WriteRead: both systems write components
        // that the other reads. This is a true cyclic conflict.
        let a_name = format!("system_{}", id_i.0);
        let b_name = format!("system_{}", id_j.0);
        return Some((
            ConflictKind::BidirectionalWriteRead { a_name, b_name },
            true,
        )); // direction irrelevant — use i→j
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
        )); // i→j (writer → reader)
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

    // ── Event conflicts ─────────────────────────────────────────
    // Active only if `event_ordering = true` (the default).

    if event_ordering {
        // EventWriteWrite: both write the same event type
        for w in &ai.writes_event {
            if aj.writes_event.iter().any(|(id, _)| *id == w.0) {
                return Some((ConflictKind::EventWriteWrite { event_name: w.1 }, true));
                // i→j
            }
        }
        // EventWrite(i)+EventRead(j): i writes the event, j reads it
        for w in &ai.writes_event {
            if aj.reads_event.iter().any(|(id, _)| *id == w.0) {
                return Some((
                    ConflictKind::EventWriteRead {
                        event_name: w.1,
                        writer_id: id_i.0,
                        reader_id: id_j.0,
                    },
                    true,
                )); // i→j (writer → reader)
            }
        }
        // EventWrite(j)+EventRead(i): j writes the event, i reads it
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

/// Split the `system_ids` list into sub-lists at `apply_deferred_after` markers.
///
/// Used in `compile()` to create sub-stages between sync points.
///
/// Rules:
/// - A system with `apply_deferred_after = true` triggers a split AFTER itself
///   (unless it is the last system in the list)
/// - Several consecutive `apply_deferred_after` → each one splits
/// - Empty sub-stages are not created
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

        // Auto-split: if this system uses Commands and the next one depends
        // on it explicitly (explicit ordering), insert a split point.
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

/// Get a type name by TypeId.
///
/// Looks the name up in the given `type_names` (populated from the ComponentRegistry).
/// If not found — returns `"<component>"`.
fn component_type_name(
    type_id: TypeId,
    type_names: &FxHashMap<TypeId, &'static str>,
) -> &'static str {
    type_names.get(&type_id).copied().unwrap_or("<component>")
}

/// Format a condition tree for debug output.
/// Returns an empty string if there are no conditions (default And([])).
fn format_condition(tree: &ConditionTree) -> String {
    match tree {
        ConditionTree::Leaf(_) => "<condition>".to_string(),
        ConditionTree::And(conds) if conds.is_empty() => String::new(),
        ConditionTree::And(conds) => format!("AND({})", conds.iter().map(|_| "<cond>").collect::<Vec<_>>().join(", ")),
        ConditionTree::Or(conds) => format!("OR({})", conds.iter().map(|_| "<cond>").collect::<Vec<_>>().join(", ")),
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
