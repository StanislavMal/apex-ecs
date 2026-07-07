//! apex-serialization — serialization/deserialization of ECS world state.
//!
//! # Concept
//!
//! Not all components are serialized — only those explicitly registered via
//! `world.register_component_serde::<T>()`. This split is fundamental:
//!
//! **Serializable** (persist state):
//!   `Position`, `Velocity`, `Health`, `Name`, `Inventory`, …
//!
//! **Non-serializable** (runtime state, recreated):
//!   `RenderHandle`, `PhysicsBody`, `AudioSource`, `GpuBuffer`, …
//!
//! # Usage
//!
//! ```ignore
//! // Saving
//! let snapshot = WorldSerializer::snapshot(&world)?;
//! let json = snapshot.to_json()?;
//! std::fs::write("save.json", &json)?;
//!
//! // Loading
//! let json = std::fs::read("save.json")?;
//! let snapshot = WorldSnapshot::from_json(&json)?;
//! let entity_map = WorldSerializer::restore(&mut world, &snapshot)?;
//! // entity_map: HashMap<old_index, new_Entity> — for patching external references
//! ```

pub mod prefab;
pub mod snapshot;
pub mod serializer;
pub(crate) mod wire;

pub use prefab::{PrefabManifest, PrefabComponent, PrefabChild, PrefabLoader, PrefabError};
pub use snapshot::{WorldSnapshot, EntitySnapshot, ComponentSnapshot, RelationSnapshot, WorldDiff, SaveFormat};
pub use serializer::{WorldSerializer, RestoreEntityMap, SerializationError};