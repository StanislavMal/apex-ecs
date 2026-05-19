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
pub use archetype::{ArchetypeId};
pub use apex_macros::Component;
pub use apex_macros::Bundle as BundleDerive;
pub use component::{Component as ComponentTrait, ComponentId, ComponentRegistry, Tick, Serializable, ComponentSerdeFns, make_serde_fns, make_serde_fns_json};
pub use entity::Entity;
pub use events::{Events, EventRegistry, EventCursor, DelayedQueue, PartialReadGuard, PeekGuard};
pub use resources::Resources;
pub use sub_world::SubWorld;
pub use world::{World, Bundle, CachedQuery, ParallelWorld, SystemContext};
pub use query::{Query, Read, Write, With, Without, Changed, Maybe, MaybeWrite, WorldQuery};
pub use commands::Commands;
pub use relations::{RelationKind, ChildOf, Owns};
pub use linkme;  // re-exported for #[derive(Component)] macro
pub use smallvec;  // re-exported for #[derive(Bundle)] macro
pub use system_param::{
    Res, ResMut, EventReader, EventWriter,
    ResRead, ResWrite, Listen, Emit,
    ResourceAccessList, EventAccessList,
    WorldQuerySystemAccess, AutoSystem,
};

pub mod prelude {
    pub use crate::access::AccessDescriptor;
    pub use crate::component::{Component as ComponentTrait, Tick, Serializable};
    pub use crate::entity::Entity;
    pub use crate::events::{Events, EventCursor, DelayedQueue, PartialReadGuard, PeekGuard};
    pub use crate::resources::Resources;
    pub use crate::world::{World, Bundle, CachedQuery, SystemContext};
    pub use crate::query::{Query, Read, Write, With, Without, Changed, Maybe, MaybeWrite, QueryBuilder, WorldQuery};
    pub use crate::commands::Commands;
    pub use crate::relations::{RelationKind, ChildOf, Owns};
    pub use crate::system_param::{
        Res, ResMut, EventReader, EventWriter,
        ResRead, ResWrite, Listen, Emit,
        WorldQuerySystemAccess, AutoSystem,
    };
    pub use crate::template::{TemplateParams, EntityTemplate, TemplateParam};
    pub use crate::impl_entity_template;
    pub use crate::Component;
    pub use crate::BundleDerive as Bundle;
    pub use crate::system;
    pub use crate::sequential_system;
}
