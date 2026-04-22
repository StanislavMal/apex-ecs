pub mod access;
pub mod archetype;
pub mod commands;
pub mod component;
pub mod entity;
pub mod events;
pub mod query;
pub mod relations;
pub mod resources;
pub mod storage;
pub mod system_param;
pub mod world;

pub use access::AccessDescriptor;
pub use component::{Component, Tick, Serializable, ComponentSerdeFns, make_serde_fns};
pub use entity::Entity;
pub use events::{EventQueue, EventRegistry};
pub use resources::ResourceMap;
pub use world::{World, Bundle, CachedQuery, ParallelWorld, SystemContext, DeferredQueue};
pub use query::{Query, Read, Write, With, Without, Changed, WorldQuery};
pub use commands::Commands;
pub use relations::{RelationKind, ChildOf, Owns, Likes};
pub use system_param::{
    Res, ResMut, EventReader, EventWriter,
    WorldQuerySystemAccess, AutoSystem,
};

pub mod prelude {
    pub use crate::access::AccessDescriptor;
    pub use crate::component::{Component, Tick, Serializable};
    pub use crate::entity::Entity;
    pub use crate::events::EventQueue;
    pub use crate::resources::ResourceMap;
    pub use crate::world::{World, Bundle, CachedQuery, SystemContext, DeferredQueue};
    pub use crate::query::{Query, Read, Write, With, Without, Changed, QueryBuilder, WorldQuery};
    pub use crate::commands::Commands;
    pub use crate::relations::{RelationKind, ChildOf, Owns, Likes};
    pub use crate::system_param::{
        Res, ResMut, EventReader, EventWriter,
        WorldQuerySystemAccess, AutoSystem,
    };
}
