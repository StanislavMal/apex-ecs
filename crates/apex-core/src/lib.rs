pub mod archetype;
pub mod component;
pub mod entity;
pub mod query;
pub mod storage;
pub mod world;

pub use component::Component;
pub use entity::Entity;
pub use world::World;

/// Prelude — удобный импорт
pub mod prelude {
    pub use crate::component::Component;
    pub use crate::entity::Entity;
    pub use crate::world::World;
    pub use crate::query::QueryBuilder;
}