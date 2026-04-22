//! apex-hot-reload — файловый watcher + горячая перезагрузка ассетов/конфигов.
//!
//! # Фаза 2: Asset Hot Reload
//!
//! Перезагружает файлы конфигурации и ассетов без остановки мира.
//! Не требует dylib, ABI проблем нет — работает с любыми типами
//! реализующими `serde::Deserialize`.
//!
//! # Архитектура
//!
//! ```text
//!   FileWatcher (background thread)
//!       │  notify::Event (path changed)
//!       ▼
//!   AssetRegistry::poll_changes() ← вызывается в game loop
//!       │  AssetChange { path, asset_id }
//!       ▼
//!   HotReloadPlugin::apply_changes(&mut world)
//!       │  перезагружает файл → десериализует → вставляет как Resource
//!       ▼
//!   World::insert_resource::<T>(new_value)
//! ```
//!
//! # Использование
//!
//! ```ignore
//! let mut hot = HotReloadPlugin::new();
//!
//! // Регистрируем файл конфига как ресурс PhysicsConfig
//! hot.watch_config::<PhysicsConfig>("assets/physics.json", &mut world);
//!
//! // В game loop:
//! loop {
//!     hot.apply_changes(&mut world);  // < 1µs если нет изменений
//!     scheduler.run(&mut world);
//! }
//! ```

pub mod asset_registry;
pub mod watcher;
pub mod plugin;

pub use asset_registry::{AssetId, AssetRegistry, AssetChange};
pub use watcher::FileWatcher;
pub use plugin::{HotReloadPlugin, HotReloadError, ConfigLoader};