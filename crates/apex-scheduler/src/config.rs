//! SystemConfig — system configuration with conditions (value type, no reference to Scheduler).
//!
//! # Usage
//!
//! ```ignore
//! use apex_scheduler::{sys, seq, par};
//!
//! s.add_systems(StageLabel::Update, (
//!     sys("movement", movement).run_if(is_playing),
//!     seq("cleanup", |w| { ... }),
//!     par("log", |_: SystemContext| println!("tick")),
//! ));
//! ```

use crate::{AccessDescriptor, Condition, ConditionTree, ParSystem, SystemFn};
use apex_core::system_param::{
    AutoSystem, EventAccessList, ExclusiveSystem, ResourceAccessList, WorldQuerySystemAccess,
};
use apex_core::world::SystemContext;

// ── SystemConfig ───────────────────────────────────────────

pub(crate) enum SystemConfigKind {
    Auto(Box<dyn ParSystem>, AccessDescriptor),
    Sequential(SystemFn),
    ParClosure {
        access: AccessDescriptor,
        func: Box<dyn FnMut(SystemContext<'_>) + Send + Sync>,
    },
}

/// Configuration of a single system (name + body + conditions + order).
/// Value type — does not reference Scheduler, can be put in a tuple.
pub struct SystemConfig {
    pub(crate) name: String,
    pub(crate) kind: SystemConfigKind,
    pub(crate) condition: ConditionTree,
    pub(crate) condition_access: AccessDescriptor,
    pub(crate) has_deferred: bool,
    /// Names of systems this one must run BEFORE (declared via `.before()`).
    /// Resolved to `SystemId` at compile time (forward references allowed).
    pub(crate) before_names: Vec<String>,
    /// Names of systems this one must run AFTER (declared via `.after()`).
    pub(crate) after_names: Vec<String>,
}

impl SystemConfig {
    pub fn run_if<F>(self, condition: F) -> Self
    where
        F: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        self.push_and_leaf(ConditionTree::leaf(condition), AccessDescriptor::new())
    }

    pub fn run_if_cond<C: Condition>(self, condition: C) -> Self {
        let acc = condition.access();
        let leaf = ConditionTree::Leaf(condition.into_check_fn());
        self.push_and_leaf(leaf, acc)
    }

    pub fn or_else<F>(self, condition: F) -> Self
    where
        F: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        self.push_or_leaf(ConditionTree::leaf(condition), AccessDescriptor::new())
    }

    pub fn or_else_cond<C: Condition>(self, condition: C) -> Self {
        let acc = condition.access();
        let leaf = ConditionTree::Leaf(condition.into_check_fn());
        self.push_or_leaf(leaf, acc)
    }

    fn push_and_leaf(mut self, leaf: ConditionTree, acc: AccessDescriptor) -> Self {
        self.condition_access = std::mem::take(&mut self.condition_access).merge(&acc);
        match &mut self.condition {
            ConditionTree::And(ref mut conds) => conds.push(leaf),
            _ => {
                let old = std::mem::replace(&mut self.condition, ConditionTree::And(Vec::new()));
                if let ConditionTree::And(ref mut conds) = self.condition {
                    conds.push(old);
                    conds.push(leaf);
                }
            }
        }
        self
    }

    fn push_or_leaf(mut self, leaf: ConditionTree, acc: AccessDescriptor) -> Self {
        self.condition_access = std::mem::take(&mut self.condition_access).merge(&acc);
        match &mut self.condition {
            ConditionTree::Or(ref mut conds) => conds.push(leaf),
            _ => {
                let old = std::mem::replace(&mut self.condition, ConditionTree::Or(Vec::new()));
                if let ConditionTree::Or(ref mut conds) = self.condition {
                    conds.push(old);
                    conds.push(leaf);
                }
            }
        }
        self
    }

    /// Set the entire condition tree at once.
    pub fn condition(mut self, tree: ConditionTree) -> Self {
        self.condition = tree;
        self
    }

    /// Declare that this system calls `par_for_each` internally (it already
    /// parallelizes its own body over entities). The scheduler then will NOT
    /// also ASD-row-split it across worker tasks — nested parallelism would
    /// oversubscribe the pool. Golden-path declaration set at config-build time,
    /// like `.run_if()`/`.before()`.
    ///
    /// No-op on `seq()` systems (Sequential has no component access to split).
    pub fn par_for_each_used(mut self) -> Self {
        match &mut self.kind {
            SystemConfigKind::Auto(_, access) => access.uses_par_for_each = true,
            SystemConfigKind::ParClosure { access, .. } => access.uses_par_for_each = true,
            SystemConfigKind::Sequential(_) => {}
        }
        self
    }
}

// ── Constructors ───────────────────────────────────────────

impl SystemConfig {
    /// AutoSystem (includes `system!` struct).
    pub(crate) fn sys<S: AutoSystem + 'static>(name: impl Into<String>, s: S) -> Self {
        let mut access = S::Query::system_access()
            .merge(&S::Resources::resource_accesses())
            .merge(&S::Events::event_accesses());
        if S::NEEDS_WHOLE_WORLD {
            access.needs_whole_world = true;
        }
        // W3-4: stateful system → no ASD row-split (run(&mut self)
        // of a single instance cannot be called concurrently from multiple tasks).
        if std::mem::size_of::<S>() > 0 {
            access.stateful = true;
        }
        // F4b: an event-reading system must be single-task — a persistent cursor
        // shared across ASD row-split chunks would re-read events per chunk. Forcing
        // `stateful` keeps it whole (no row-split), so its adapter-owned cursor store
        // is touched by exactly one task.
        if !access.reads_event.is_empty() {
            access.stateful = true;
        }
        struct Adapter<S: AutoSystem> {
            inner: S,
            cursors: apex_core::EventCursors,
        }
        impl<S: AutoSystem + 'static> ParSystem for Adapter<S> {
            fn access() -> AccessDescriptor { unreachable!() }
            fn run(&mut self, ctx: SystemContext<'_>) {
                // F4b: install the persistent cursor store (single-task ⇒ no alias).
                let ctx = unsafe { ctx.with_event_cursors(std::ptr::addr_of_mut!(self.cursors)) };
                self.inner.run(ctx);
            }
        }
        Self {
            name: name.into(),
            kind: SystemConfigKind::Auto(
                Box::new(Adapter { inner: s, cursors: apex_core::EventCursors::default() }),
                access,
            ),
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: S::HAS_DEFERRED,
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Sequential system.
    pub(crate) fn seq<F>(name: impl Into<String>, f: F) -> Self
    where
        F: FnMut(&mut apex_core::world::World) + Send + 'static,
    {
        Self {
            name: name.into(),
            kind: SystemConfigKind::Sequential(Box::new(f)),
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Exclusive system (`system!` with `world: &mut World`).
    ///
    /// Declares FULL access and is registered as Sequential — the scheduler
    /// runs it alone. The name is taken from the system itself.
    pub fn exclusive<S: ExclusiveSystem>(mut system: S) -> Self {
        let name = system.name().to_string();
        Self {
            name,
            kind: SystemConfigKind::Sequential(Box::new(move |w| system.run(w))),
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Plain-fn system (D2-1): an ordinary function with Bevy parameters
    /// (`Res<T>`/`ResMut<T>`/`Query<Q>`/`EventReader<E>`/`EventWriter<E>`/
    /// `&mut Commands`). Access is derived from the parameters, the name from the
    /// function name. Usually invoked implicitly via `add_systems(stage, (fn1, …))`.
    pub fn fn_sys<F, M>(f: F) -> Self
    where
        F: apex_core::SystemParamFunction<M>,
    {
        let mut access = <F::Param as apex_core::SystemParam>::access();
        // W3-4: a closure with captures = state → no ASD row-split
        // (an fn-item is a ZST, not affected).
        if std::mem::size_of::<F>() > 0 {
            access.stateful = true;
        }
        let has_deferred = <F::Param as apex_core::SystemParam>::has_deferred();
        let mut f = f;
        // V3: the closure OWNS the per-system state (Query arch-index caches etc.),
        // alive across frames. `State: Send + Sync + Default + 'static`, so the box
        // keeps its existing Send + Sync bounds (no ripple).
        let mut state = <<F::Param as apex_core::SystemParam>::State>::default();
        let func: Box<dyn FnMut(SystemContext<'_>) + Send + Sync> =
            Box::new(move |ctx: SystemContext<'_>| {
                // E5: parameter skip-semantics (Single<Q> etc.) — Bevy
                // validate_param: an invalid parameter silently skips the frame.
                if !<F::Param as apex_core::SystemParam>::validate(&ctx) {
                    return;
                }
                let item =
                    <F::Param as apex_core::SystemParam>::get_param(&ctx, &mut state);
                f.run(item);
            });
        Self {
            name: apex_core::short_system_name::<F>().to_string(),
            kind: SystemConfigKind::ParClosure { access, func },
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred,
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Parallel closure (no component access).
    pub(crate) fn par<F>(name: impl Into<String>, f: F) -> Self
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let mut access = AccessDescriptor::new();
        // W3-4: a closure with captures = state → no ASD row-split.
        if std::mem::size_of::<F>() > 0 {
            access.stateful = true;
        }
        Self {
            name: name.into(),
            kind: SystemConfigKind::ParClosure {
                access,
                func: Box::new(f),
            },
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Parallel closure with an explicit AccessDescriptor.
    pub(crate) fn par_access<F>(name: impl Into<String>, access: AccessDescriptor, f: F) -> Self
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let mut access = access;
        // W3-4: a closure with captures = state → no ASD row-split.
        if std::mem::size_of::<F>() > 0 {
            access.stateful = true;
        }
        Self {
            name: name.into(),
            kind: SystemConfigKind::ParClosure { access, func: Box::new(f) },
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }
}

// ── Free function aliases — short API ───────────────────────

/// AutoSystem (includes `system!` struct). Alias for `SystemConfig::sys()`.
pub fn sys<S: AutoSystem + 'static>(name: impl Into<String>, s: S) -> SystemConfig {
    SystemConfig::sys(name, s)
}

/// Sequential system. Alias for `SystemConfig::seq()`.
pub fn seq<F>(name: impl Into<String>, f: F) -> SystemConfig
where F: FnMut(&mut apex_core::world::World) + Send + 'static
{
    SystemConfig::seq(name, f)
}

/// Parallel closure (no access). Alias for `SystemConfig::par()`.
pub fn par<F>(name: impl Into<String>, f: F) -> SystemConfig
where F: FnMut(SystemContext<'_>) + Send + Sync + 'static
{
    SystemConfig::par(name, f)
}

/// Parallel closure with an explicit AccessDescriptor. Alias for `SystemConfig::par_access()`.
pub fn par_access<F>(name: impl Into<String>, access: AccessDescriptor, f: F) -> SystemConfig
where F: FnMut(SystemContext<'_>) + Send + Sync + 'static
{
    SystemConfig::par_access(name, access, f)
}

// ── IntoScheduleConfigs — single registration entry (U.3/U.4) ──
//
// The marker parameter `M` distinguishes sources (Bevy-style disambiguation),
// which allows passing HETEROGENEOUS elements into `add_systems` as one tuple:
//   - bare `AutoSystem` marker (parallel system from `system!`) — name from fn;
//   - bare `ExclusiveSystem` marker (`system!` with `world: &mut World`) — name from fn;
//   - ready-made `SystemConfig` (`sys()`/`seq()`/`.run_if()`…).
// Different markers → different trait instantiations → no coherence conflict.

/// Marker: element is a ready-made `SystemConfig`.
#[doc(hidden)]
pub struct ConfigMarker;
/// Marker: element is a bare `AutoSystem` (parallel system).
#[doc(hidden)]
pub struct AutoMarker;
/// Marker: element is a bare `ExclusiveSystem`.
#[doc(hidden)]
pub struct ExclusiveMarker;
/// Marker: element is a plain-fn system (D2-1, `SystemParamFunction`).
/// Not a tuple — otherwise it would overlap with the tuple impl of `IntoScheduleConfigs`.
#[doc(hidden)]
pub struct FnSystemMarker<M>(std::marker::PhantomData<M>);

/// Marker: element is an already-assembled [`ScheduleConfigs`] (e.g. the result
/// of `.chain()`/`.before()`/`.after()`).
#[doc(hidden)]
pub struct ScheduleConfigsMarker;

/// A group of configured systems + their ordering edges — what
/// [`IntoScheduleConfigs`] turns systems/tuples into before registration.
///
/// The golden-path ordering methods (`.before()`/`.after()`/`.chain()`)
/// return this type. `edges` are positional "earlier→later" edges between
/// elements of `configs` (indices into `configs`); populated by `.chain()` and
/// preserved when tuples are merged (nested `.chain()` calls are not lost).
/// Named edges (`.before("x")`) are stored on the
/// [`SystemConfig::before_names`]/[`SystemConfig::after_names`] themselves and are
/// resolved by the scheduler at `compile()` (forward references allowed).
pub struct ScheduleConfigs {
    pub(crate) configs: Vec<SystemConfig>,
    /// Positional ordering edges `(before_idx, after_idx)` within `configs`.
    pub(crate) edges: Vec<(usize, usize)>,
}

impl ScheduleConfigs {
    fn single(cfg: SystemConfig) -> Self {
        Self { configs: vec![cfg], edges: Vec::new() }
    }
}

/// Conversion of systems/tuples into [`ScheduleConfigs`] — the single entry
/// point for `add_systems`.
///
/// Implemented for `SystemConfig`, bare `AutoSystem`/`ExclusiveSystem`/plain-fn
/// systems, [`ScheduleConfigs`], and tuples of up to 12 elements. The ordering
/// methods (`before`/`after`/`chain`) are the golden path for declaring
/// dependencies directly on the configs (Bevy idiom); the string-based
/// `Scheduler::before/after/chain` remains for DYNAMIC ordering by name
/// (editor/scripts).
pub trait IntoScheduleConfigs<M>: Sized {
    /// Turn into a group of configs (for `add_systems`).
    fn into_configs(self) -> ScheduleConfigs;

    /// Declare that THIS system (or all systems of the group) run BEFORE the
    /// system named `name`. The name is resolved at `compile()` (forward
    /// references allowed).
    fn before(self, name: impl Into<String>) -> ScheduleConfigs {
        let mut c = self.into_configs();
        let name = name.into();
        for cfg in &mut c.configs {
            cfg.before_names.push(name.clone());
        }
        c
    }

    /// Declare that THIS system (or all systems of the group) run AFTER the
    /// system named `name`. The name is resolved at `compile()`.
    fn after(self, name: impl Into<String>) -> ScheduleConfigs {
        let mut c = self.into_configs();
        let name = name.into();
        for cfg in &mut c.configs {
            cfg.after_names.push(name.clone());
        }
        c
    }

    /// Chain the systems of the group: `a → b → c` (each after the previous).
    /// A no-op for a single element. Positional (by order in the tuple); no
    /// names needed.
    fn chain(self) -> ScheduleConfigs {
        let mut c = self.into_configs();
        for i in 0..c.configs.len().saturating_sub(1) {
            c.edges.push((i, i + 1));
        }
        c
    }
}

impl IntoScheduleConfigs<ScheduleConfigsMarker> for ScheduleConfigs {
    fn into_configs(self) -> ScheduleConfigs {
        self
    }
}

impl IntoScheduleConfigs<ConfigMarker> for SystemConfig {
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(self)
    }
}

impl<S: AutoSystem + 'static> IntoScheduleConfigs<AutoMarker> for S {
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(SystemConfig::sys(S::name(), self))
    }
}

impl<S: ExclusiveSystem> IntoScheduleConfigs<ExclusiveMarker> for S {
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(SystemConfig::exclusive(self))
    }
}

/// Plain-fn system (D2-1): `fn movement(time: Res<Time>, q: Query<…>)` →
/// `add_systems(stage, (movement, …))`. The marker carries the signature
/// `fn(P1, …)`, so coherence does not conflict with the Auto/Exclusive impls.
impl<F, M> IntoScheduleConfigs<FnSystemMarker<M>> for F
where
    F: apex_core::SystemParamFunction<M>,
{
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(SystemConfig::fn_sys(self))
    }
}

/// Builder methods directly on a plain-fn system (P4, like Bevy
/// `IntoSystemConfigs`): `movement.run_if(in_state(Game::Playing))` — without
/// wrapping in `SystemConfig::fn_sys`. (The ordering `.before()/.after()/.chain()`
/// are available via [`IntoScheduleConfigs`].)
pub trait FnSystemExt<M>: apex_core::SystemParamFunction<M> + Sized {
    /// Turn the fn into a [`SystemConfig`] (name from the function name).
    fn into_config(self) -> SystemConfig {
        SystemConfig::fn_sys(self)
    }

    /// Run condition (see [`SystemConfig::run_if`]).
    fn run_if<C>(self, cond: C) -> SystemConfig
    where
        C: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        self.into_config().run_if(cond)
    }
}

impl<F, M> FnSystemExt<M> for F where F: apex_core::SystemParamFunction<M> {}

macro_rules! impl_into_schedule_configs_tuple {
    ($($T:ident : $M:ident),+) => {
        impl<$($T, $M),+> IntoScheduleConfigs<($($M,)+)> for ($($T,)+)
        where
            $( $T: IntoScheduleConfigs<$M>, )+
        {
            #[allow(non_snake_case)]
            fn into_configs(self) -> ScheduleConfigs {
                let ($($T,)+) = self;
                let mut configs: Vec<SystemConfig> = Vec::new();
                let mut edges: Vec<(usize, usize)> = Vec::new();
                $(
                    // Merge: shift the child group's positional edges by the
                    // current count of already-accumulated configs, so nested
                    // `.chain()` calls survive the flatten.
                    let mut sub = $T.into_configs();
                    let offset = configs.len();
                    for (a, b) in sub.edges.drain(..) {
                        edges.push((a + offset, b + offset));
                    }
                    configs.append(&mut sub.configs);
                )+
                ScheduleConfigs { configs, edges }
            }
        }
    };
}

impl_into_schedule_configs_tuple!(A: MA);
impl_into_schedule_configs_tuple!(A: MA, B: MB);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI, J: MJ);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI, J: MJ, K: MK);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI, J: MJ, K: MK, L: ML);
