pub mod archetype;
pub mod commands;
pub mod component;
pub mod entity;
pub mod events;
pub mod query;
pub mod relations;
pub mod resources;
pub mod storage;
pub mod world;

pub use component::{Component, Tick};
pub use entity::Entity;
pub use events::{EventQueue, EventRegistry};
pub use resources::ResourceMap;
pub use world::{World, Bundle, CachedQuery};
pub use query::{Query, Read, Write, With, Without, Changed, WorldQuery};
pub use commands::Commands;
pub use relations::{RelationKind, ChildOf, Owns, Likes};

pub mod prelude {
    pub use crate::component::{Component, Tick};
    pub use crate::entity::Entity;
    pub use crate::events::EventQueue;
    pub use crate::resources::ResourceMap;
    pub use crate::world::{World, Bundle, CachedQuery};
    pub use crate::query::{Query, Read, Write, With, Without, Changed, QueryBuilder, WorldQuery};
    pub use crate::commands::Commands;
    pub use crate::relations::{RelationKind, ChildOf, Owns, Likes};
}