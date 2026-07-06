//! apex-hot-reload — file watcher + hot reload of assets/configs.
//!
//! # Phase 2: Asset Hot Reload
//!
//! Reloads configuration and asset files without stopping the world.
//! Requires no dylib and has no ABI problems — works with any type
//! implementing `serde::Deserialize`.
//!
//! # Architecture
//!
//! ```text
//!   FileWatcher (background thread)
//!       │  notify::Event (path changed)
//!       ▼
//!   AssetRegistry::poll_changes() ← called in the game loop
//!       │  AssetChange { path, asset_id }
//!       ▼
//!   HotReloadPlugin::apply_changes(&mut world)
//!       │  reloads the file → deserializes → inserts as a Resource
//!       ▼
//!   World::insert_resource::<T>(new_value)
//! ```
//!
//! # Usage
//!
//! ```ignore
//! let mut hot = HotReloadPlugin::new();
//!
//! // Register the config file as a PhysicsConfig resource
//! hot.watch_config::<PhysicsConfig>("assets/physics.json", &mut world);
//!
//! // In the game loop:
//! loop {
//!     hot.apply_changes(&mut world);  // < 1µs if there are no changes
//!     scheduler.run(&mut world);
//! }
//! ```

pub mod asset_registry;
pub mod watcher;
pub mod plugin;
pub mod prefab_plugin;

pub use asset_registry::{AssetId, AssetRegistry, AssetChange};
pub use watcher::FileWatcher;
pub use plugin::{HotReloadPlugin, HotReloadError, ConfigLoader};
pub use prefab_plugin::{PrefabPlugin, PrefabAsset, PrefabInstance};