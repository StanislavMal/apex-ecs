//! apex-serialization — сериализация/десериализация состояния ECS мира.
//!
//! # Концепция
//!
//! Не все компоненты сериализуются — только те, которые явно зарегистрированы
//! через `world.register_component_serde::<T>()`. Это разделение принципиально:
//!
//! **Serializable** (persist state):
//!   `Position`, `Velocity`, `Health`, `Name`, `Inventory`, …
//!
//! **Non-serializable** (runtime state, пересоздаётся):
//!   `RenderHandle`, `PhysicsBody`, `AudioSource`, `GpuBuffer`, …
//!
//! # Использование
//!
//! ```ignore
//! // Сохранение
//! let snapshot = WorldSerializer::snapshot(&world)?;
//! let json = snapshot.to_json()?;
//! std::fs::write("save.json", &json)?;
//!
//! // Загрузка
//! let json = std::fs::read("save.json")?;
//! let snapshot = WorldSnapshot::from_json(&json)?;
//! let entity_map = WorldSerializer::restore(&mut world, &snapshot)?;
//! // entity_map: HashMap<old_index, new_Entity> — для патча внешних ссылок
//! ```

pub mod prefab;
pub mod snapshot;
pub mod serializer;

pub use prefab::{PrefabManifest, PrefabComponent, PrefabChild, PrefabLoader, PrefabError};
pub use snapshot::{WorldSnapshot, EntitySnapshot, ComponentSnapshot, RelationSnapshot, WorldDiff, SaveFormat};
pub use serializer::{WorldSerializer, RestoreEntityMap, SerializationError};