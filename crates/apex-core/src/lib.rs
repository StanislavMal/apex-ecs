// ── Политика clippy ────────────────────────────────────────────
// Ядро — низкоуровневый высокопроизводительный код с большим объёмом
// внутреннего `unsafe` (storage/archetype/query). Следующие линты намеренно
// смягчены: их «исправление» либо ухудшает перф/читаемость, либо относится к
// внутренним примитивам, чьи safety-контракты документированы на уровне типов.
// Корректность важнее: `unsafe`-инварианты покрыты тестами и debug_assert.
#![allow(
    clippy::missing_safety_doc,    // внутренние storage-примитивы (Column и пр.)
    clippy::needless_range_loop,   // индексные циклы в горячих путях — намеренно
    clippy::nonminimal_bool,       // явные булевы выражения ради читаемости
    clippy::question_mark,         // явный if-let ради ясности control-flow
    clippy::type_complexity,       // сложные типы в API запросов/планировщика
    clippy::too_many_arguments,    // низкоуровневые fn хранилища
)]

// Позволяет `#[derive(Component)]` (эмитит пути `::apex_core::…`) работать на
// типах ВНУТРИ самого apex-core (transform и пр.).
extern crate self as apex_core;

pub mod access;
pub mod archetype;
pub mod commands;
pub mod component;
pub mod dense;
pub mod entity;
pub mod events;
pub mod par_utils;
pub mod query;
pub mod relations;
pub mod resources;
pub mod storage;
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
    make_serde_fns, make_serde_fns_json, Component as ComponentTrait, ComponentId,
    ComponentRegistry, ComponentSerdeFns, Serializable, Tick,
};
pub use entity::Entity;
pub use events::{DelayedQueue, EventCursor, EventRegistry, Events, PartialReadGuard, PeekGuard};
pub use linkme; // re-exported for #[derive(Component)] macro
pub use dense::DenseQuery;
pub use query::{
    Changed, Maybe, MaybeWrite, Mut, Or, Query, Read, Ref, With, Without, WorldQuery, Write,
};
pub use relations::{ChildOf, Owns, RelationKind};
pub use resources::Resources;
pub use smallvec; // re-exported for #[derive(Bundle)] macro
pub use sub_world::SubWorld;
pub use system_param::{
    AutoSystem, CommandsParam, Emit, EventAccessList, EventReader, EventWriter, ExclusiveSystem,
    Listen, QueryParam, Res, ResMut, ResRead, ResWrite, ResourceAccessList, SystemParam,
    WorldQuerySystemAccess,
};
pub use transform::IndexStamp;
pub use world::{
    ArchetypeStats, Bundle, CachedQuery, ParallelWorld, QueryState, SystemContext, World,
};

pub mod prelude {
    pub use crate::access::AccessDescriptor;
    pub use crate::commands::Commands;
    pub use crate::component::{Component as ComponentTrait, Serializable, Tick};
    pub use crate::entity::Entity;
    pub use crate::events::{DelayedQueue, EventCursor, Events, PartialReadGuard, PeekGuard};
    pub use crate::impl_entity_template;
    pub use crate::dense::DenseQuery;
    pub use crate::query::{
        Changed, Maybe, MaybeWrite, Mut, Or, Query, QueryBuilder, Read, Ref, With, Without,
        WorldQuery, Write,
    };
    pub use crate::relations::{ChildOf, Owns, RelationKind};
    pub use crate::resources::Resources;
    pub use crate::system;
    pub use crate::system_param::{
        AutoSystem, CommandsParam, Emit, EventReader, EventWriter, ExclusiveSystem, Listen,
        QueryParam, Res, ResMut, ResRead, ResWrite, SystemParam, WorldQuerySystemAccess,
    };
    pub use crate::template::{EntityTemplate, TemplateParam, TemplateParams};
    pub use crate::world::{Bundle, CachedQuery, QueryState, SystemContext, World};
    pub use crate::BundleDerive as Bundle;
    pub use crate::Component;
}
