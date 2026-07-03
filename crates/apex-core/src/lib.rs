// ── Политика clippy ────────────────────────────────────────────
// Ядро — низкоуровневый высокопроизводительный код с большим объёмом
// внутреннего `unsafe` (storage/archetype/query). Следующие линты намеренно
// смягчены: их «исправление» либо ухудшает перф/читаемость, либо относится к
// внутренним примитивам, чьи safety-контракты документированы на уровне типов.
// Корректность важнее: `unsafe`-инварианты покрыты тестами и debug_assert.
#![allow(
    clippy::needless_range_loop,   // индексные циклы в горячих путях — намеренно
    clippy::nonminimal_bool,       // явные булевы выражения ради читаемости
    clippy::question_mark,         // явный if-let ради ясности control-flow
    clippy::type_complexity,       // сложные типы в API запросов/планировщика
    clippy::too_many_arguments,    // низкоуровневые fn хранилища
)]

// Позволяет `#[derive(Component)]` (эмитит пути `::apex_core::…`) работать на
// типах ВНУТРИ самого apex-core (transform и пр.).
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

pub mod access;
pub mod archetype;
pub mod commands;
pub mod component;
pub mod dense;
pub mod entity;
pub mod events;
pub mod fn_system;
pub mod par_utils;
pub mod query;
pub mod relations;
pub mod resources;
pub mod sub_world;
pub mod system_macro;
pub mod system_param;
pub mod template;
pub mod transform;
pub mod world;

pub use access::AccessDescriptor;
pub use apex_macros::Bundle as BundleDerive;
pub use apex_macros::Component;
pub use archetype::ArchetypeId;
pub use commands::Commands;
pub use component::{
    make_serde_fns, make_serde_fns_json, Component as ComponentTrait, ComponentHookFn, ComponentId,
    ComponentRegistry, ComponentSerdeFns, NoContext, Serializable, SerdeContext, Tick,
};
pub use entity::Entity;
pub use events::{
    DelayedQueue, EventCursor, EventIterator, EventReadGuard, EventRegistry, Events,
    PartialReadGuard, PeekGuard, Removed,
};
pub use fn_system::{short_system_name, SystemParamFunction};
pub use linkme; // re-exported for #[derive(Component)] macro
pub use dense::DenseQuery;
pub use query::{
    Added, ArchetypeFilter, Changed, Maybe, MaybeWrite, Mut, Or, Query, QuerySingleError, Read,
    Ref, With, Without, WorldQuery, Write,
};
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
pub use world::{
    ArchetypeStats, Bundle, CachedQuery, ParallelWorld, QueryState, SystemContext, World,
};

/// Шорткат `Default::default()` для struct-update-литералов (Bevy-идиома):
/// `PbrMaterial { roughness: 0.5, ..default() }` вместо `..Default::default()`.
#[inline]
pub fn default<T: Default>() -> T {
    T::default()
}

pub mod prelude {
    pub use crate::default;
    pub use crate::access::AccessDescriptor;
    pub use crate::commands::Commands;
    pub use crate::component::{Component as ComponentTrait, Serializable, Tick};
    pub use crate::entity::Entity;
    pub use crate::events::{
        DelayedQueue, EventCursor, EventIterator, EventReadGuard, Events, PartialReadGuard,
        PeekGuard, Removed,
    };
    pub use crate::fn_system::SystemParamFunction;
    pub use crate::impl_entity_template;
    pub use crate::dense::DenseQuery;
    pub use crate::query::{
        Added, ArchetypeFilter, Changed, Maybe, MaybeWrite, Mut, Or, Query, QueryBuilder, Single,
        QuerySingleError, Read, Ref, With, Without, WorldQuery, Write,
    };
    pub use crate::relations::{ChildOf, Owns, RelationHookFn, RelationKind};
    pub use crate::resources::Resources;
    pub use crate::system;
    pub use crate::system_param::{
        AutoSystem, CommandsParam, Emit, EventReader, EventWriter, ExclusiveSystem, Listen,
        QueryParam, RemovedComponents, Res, ResMut, ResRead, ResWrite, SystemParam,
        WorldQuerySystemAccess,
    };
    pub use crate::template::{EntityTemplate, TemplateParam, TemplateParams};
    pub use crate::world::{Bundle, CachedQuery, QueryState, SystemContext, World};
    pub use crate::BundleDerive as Bundle;
    pub use crate::Component;
}
