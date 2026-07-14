// ── clippy policy ──────────────────────────────────────────────
// The core is low-level high-performance code with a large amount of internal
// `unsafe` (storage/archetype/query). The following lints are deliberately
// relaxed: "fixing" them either hurts perf/readability, or concerns internal
// primitives whose safety contracts are documented at the type level.
// Correctness comes first: the `unsafe` invariants are covered by tests and
// debug_assert.
#![allow(
    clippy::needless_range_loop,   // indexed loops in hot paths — intentional
    clippy::nonminimal_bool,       // explicit boolean expressions for readability
    clippy::question_mark,         // explicit if-let for control-flow clarity
    clippy::type_complexity,       // complex types in the query/scheduler API
    clippy::too_many_arguments,    // low-level storage fns
)]

// Lets `#[derive(Component)]` (which emits `::apex_core::…` paths) work on
// types INSIDE apex-core itself (transform, etc.).
extern crate self as apex_core;

/// Emit a `log::warn!` at most once per call site (§0.2a "loud, not silent").
///
/// Misuse paths — operating on a dead entity, an unknown template name, a
/// dropped component — must be surfaced, but they can recur every frame and
/// would flood the log. This macro logs the first hit at a given call site and
/// suppresses the rest (a per-site `AtomicBool` latch, `Relaxed` — no ordering
/// requirement, we only need eventual single emission). Mirrors Bevy's
/// `warn_once!`. Re-exported so downstream crates share one loudness style.
#[macro_export]
macro_rules! warn_once {
    ($($arg:tt)*) => {{
        static WARNED: ::std::sync::atomic::AtomicBool =
            ::std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, ::std::sync::atomic::Ordering::Relaxed) {
            ::log::warn!($($arg)*);
        }
    }};
}

/// Report a §0.2a [`Anomaly`](crate::error::Anomaly) through a world's
/// [`ErrorHandler`](crate::error::ErrorHandler) — the policy-driven successor to
/// [`warn_once!`]. The first `$world` argument is anything exposing
/// `error_handler()` (a `&World` or `&mut World`); the rest describe the
/// anomaly: severity, operation, optional entity, optional component name, then
/// a `format!`-style message.
///
/// The per-call-site `AtomicBool` throttle lives in the expansion (same latch
/// `warn_once!` uses), so a hot misuse path logs once — but the handler still
/// counts every hit, and `Panic`/`Custom` modes fire every time.
///
/// ```ignore
/// crate::anomaly!(
///     self, $crate::Severity::Warn, "World::insert",
///     Some(entity), Some(::std::any::type_name::<T>()),
///     "component dropped, not inserted (entity already despawned)"
/// );
/// ```
#[macro_export]
macro_rules! anomaly {
    ($world:expr, $severity:expr, $op:expr, $entity:expr, $component:expr, $($detail:tt)+) => {{
        static __ANOMALY_THROTTLE: ::std::sync::atomic::AtomicBool =
            ::std::sync::atomic::AtomicBool::new(false);
        $world.error_handler().report(
            &$crate::error::Anomaly {
                severity: $severity,
                op: $op,
                entity: $entity,
                component: $component,
                detail: ::std::format_args!($($detail)+),
            },
            &__ANOMALY_THROTTLE,
        );
    }};
}

pub mod access;
pub mod archetype;
pub mod commands;
pub mod component;
pub mod dense;
pub mod entity;
pub mod error;
pub mod events;
pub mod fn_system;
pub mod par_utils;
pub mod query;
pub mod registrar;
pub mod relations;
pub mod resources;
pub mod sub_world;
pub mod system_macro;
pub mod system_param;
pub mod template;
pub mod transform;
pub mod unsafe_world_cell;
pub mod visibility;
pub mod world;

pub use access::AccessDescriptor;
pub use apex_macros::Bundle as BundleDerive;
pub use apex_macros::Component;
pub use archetype::ArchetypeId;
pub use commands::Commands;
pub use component::{
    json_merge, make_serde_fns, make_serde_fns_json, Component as ComponentTrait, ComponentHookFn,
    ComponentId, ComponentRegistry, ComponentSerdeFns, MapEntities, MapEntitiesFn, NoContext,
    Serializable, SerdeContext, Tick,
};
pub use entity::Entity;
pub use error::{Anomaly, AnomalyCounts, ErrorHandler, ErrorMode, Severity};
pub use events::{
    DelayedQueue, EventCursor, EventIterator, EventReadGuard, Events, PartialReadGuard, PeekGuard,
    Removed,
};
pub use fn_system::{short_system_name, SystemParamFunction};
pub use linkme; // re-exported for #[derive(Component)] macro
pub use dense::DenseQuery;
pub use query::{
    Added, ArchetypeFilter, Changed, DynItem, DynItemMut, DynIter, DynQuery, DynQueryError,
    DynQueryMut, Maybe, MaybeWrite, Mut, Or, Query, QueryBuilder, QueryBuilderMut, QueryIter,
    QuerySingleError, ReadOnlyWorldQuery, Single, With, Without, WorldQuery,
};
pub use registrar::WorldRegistrar;
pub use relations::{ChildOf, Owns, RelationHookFn, RelationKind};
pub use resources::Resources;
pub use smallvec; // re-exported for #[derive(Bundle)] macro
pub use sub_world::SubWorld;
pub use system_param::{
    AutoSystem, CommandsParam, Emit, EventAccessList, EventReader, EventWriter, ExclusiveSystem,
    Listen, QueryParam, RemovedComponents, Res, ResMut, ResRead, ResWrite, ResourceAccessList,
    SystemParam, WorldQuerySystemAccess,
};
pub use transform::IndexStamp;
pub use visibility::Visibility;
pub use unsafe_world_cell::UnsafeWorldCell;
pub use world::{
    ArchetypeStats, Bundle, EventCursors, ParallelWorld, QueryState, SystemContext, World,
};

/// Shortcut for `Default::default()` in struct-update literals (Bevy idiom):
/// `PbrMaterial { roughness: 0.5, ..default() }` instead of `..Default::default()`.
#[inline]
pub fn default<T: Default>() -> T {
    T::default()
}

pub mod prelude {
    // Golden-path contract only. Advanced / internal / macro-level markers are
    // reached by explicit path (see docs/CONVENTIONS.md §2).
    pub use crate::default;
    pub use crate::commands::Commands;
    pub use crate::component::{Component as ComponentTrait, Serializable, Tick};
    pub use crate::entity::Entity;
    pub use crate::events::{EventReadGuard, Events, Removed};
    pub use crate::fn_system::SystemParamFunction;
    pub use crate::impl_entity_template;
    pub use crate::query::{
        Added, ArchetypeFilter, Changed, Maybe, MaybeWrite, Mut, Or, Query, QuerySingleError,
        ReadOnlyWorldQuery, Single, With, Without, WorldQuery,
    };
    pub use crate::relations::{ChildOf, Owns, RelationKind};
    pub use crate::system;
    pub use crate::system_param::{
        AutoSystem, CommandsParam, Emit, EventReader, EventWriter, ExclusiveSystem, Listen,
        QueryParam, RemovedComponents, Res, ResMut, ResRead, ResWrite, SystemParam,
    };
    pub use crate::template::{EntityTemplate, TemplateParam, TemplateParams};
    pub use crate::world::{Bundle, QueryState, SystemContext, World};
    pub use crate::BundleDerive as Bundle;
    pub use crate::Component;
}
