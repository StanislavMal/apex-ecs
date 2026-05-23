pub mod access;
pub mod archetype;
pub mod commands;
pub mod component;
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
pub use query::{Changed, Maybe, MaybeWrite, Query, Read, With, Without, WorldQuery, Write};
pub use relations::{ChildOf, Owns, RelationKind};
pub use resources::Resources;
pub use smallvec; // re-exported for #[derive(Bundle)] macro
pub use sub_world::SubWorld;
pub use system_param::{
    AutoSystem, CommandsParam, Emit, EventAccessList, EventReader, EventWriter, Listen, QueryParam,
    Res, ResMut, ResRead, ResWrite, ResourceAccessList, SystemParam, WorldQuerySystemAccess,
};
pub use world::{Bundle, CachedQuery, ParallelWorld, SystemContext, World};

pub mod prelude {
    pub use crate::access::AccessDescriptor;
    pub use crate::commands::Commands;
    pub use crate::component::{Component as ComponentTrait, Serializable, Tick};
    pub use crate::entity::Entity;
    pub use crate::events::{DelayedQueue, EventCursor, Events, PartialReadGuard, PeekGuard};
    pub use crate::impl_entity_template;
    pub use crate::query::{
        Changed, Maybe, MaybeWrite, Query, QueryBuilder, Read, With, Without, WorldQuery, Write,
    };
    pub use crate::relations::{ChildOf, Owns, RelationKind};
    pub use crate::resources::Resources;
    pub use crate::sequential_system;
    pub use crate::system;
    pub use crate::system_param::{
        AutoSystem, CommandsParam, Emit, EventReader, EventWriter, Listen, QueryParam, Res, ResMut,
        ResRead, ResWrite, SystemParam, WorldQuerySystemAccess,
    };
    pub use crate::template::{EntityTemplate, TemplateParam, TemplateParams};
    pub use crate::world::{Bundle, CachedQuery, SystemContext, World};
    pub use crate::BundleDerive as Bundle;
    pub use crate::Component;
}
