//! PrefabPlugin — интеграция префабов с AssetRegistry и hot-reload.
//!
//! Позволяет загружать `.prefab.json` файлы как ассеты,
//! автоматически отслеживать их изменения и перезагружать кеш.
//!
//! # Использование
//!
//! ```ignore
//! use apex_hot_reload::PrefabPlugin;
//!
//! let mut plugin = PrefabPlugin::new();
//! let mut registry = apex_hot_reload::AssetRegistry::new();
//!
//! // Загрузить все префабы из директории
//! plugin.load_directory("assets/prefabs/", &mut registry).unwrap();
//!
//! // В game loop — проверить изменения
//! if let Some(changes) = check_for_changes() {
//!     for change in changes {
//!         plugin.on_asset_changed(&change);
//!     }
//! }
//! ```

use std::{
    collections::HashMap,
    path::Path,
};

use apex_core::entity::Entity;
use apex_core::relations::ChildOf;
use apex_core::world::World;
use apex_serialization::prefab::{PrefabComponent, PrefabLoader, PrefabManifest};

use crate::asset_registry::{AssetChange, AssetId, AssetRegistry};
use crate::plugin::HotReloadError;

/// Spawn context of one tracked prefab instance.
///
/// Stores everything needed to recreate the instance faithfully on hot-reload:
/// its current root [`Entity`], the parent it was spawned under, and the
/// per-instance component overrides. Without this, a reload would collapse
/// N instances (e.g. three goblins in different spots, each with its own
/// parent/overrides) into a single default one at the world root.
#[derive(Debug, Clone)]
pub struct PrefabInstance {
    /// Current root entity of this instance (updated on every reapply).
    pub entity: Entity,
    /// Parent the instance was spawned under (`None` = world root).
    pub parent: Option<Entity>,
    /// Per-instance component overrides applied on top of the manifest.
    pub overrides: Vec<PrefabComponent>,
}

/// Ассет, представляющий загруженный префаб.
///
/// Хранит манифест префаба и список инстансов, созданных из него
/// (для поддержки hot-reload — пересоздания entity при изменении файла).
#[derive(Debug, Clone)]
pub struct PrefabAsset {
    /// Манифест префаба (компоненты + дети).
    pub manifest: PrefabManifest,
    /// Инстансы, созданные из этого префаба.
    /// Заполняется внешним кодом при спавне (см. [`PrefabPlugin::track_entity`]).
    ///
    /// Each entry keeps its own parent/overrides so hot-reload recreates every
    /// instance in place, not a single collapsed one (E4 fix).
    pub spawned_entities: Vec<PrefabInstance>,
}

/// Плагин для интеграции префабов с AssetRegistry.
///
/// * Сканирует директорию на `.prefab.json` файлы
/// * Регистрирует их в `AssetRegistry`
/// * Загружает в `PrefabLoader`
/// * При изменении файла — перезагружает префаб в кеш
pub struct PrefabPlugin {
    /// Внутренний загрузчик префабов с кешем.
    loader: PrefabLoader,
    /// AssetId → имя префаба (для поиска в loader после перезагрузки).
    prefab_names: HashMap<u32, String>,
    /// AssetId → информация о загруженном префабе (опционально).
    assets: HashMap<u32, PrefabAsset>,
}

impl PrefabPlugin {
    pub fn new() -> Self {
        Self {
            loader: PrefabLoader::new(),
            prefab_names: HashMap::new(),
            assets: HashMap::new(),
        }
    }

    /// Загрузить все `.prefab.json` файлы из указанной директории.
    ///
    /// Каждый найденный файл:
    /// 1. Регистрируется в `AssetRegistry` (получает `AssetId`)
    /// 2. Загружается в `PrefabLoader`
    /// 3. Сохраняется маппинг `AssetId → имя префаба`
    pub fn load_directory(
        &mut self,
        dir: &Path,
        registry: &mut AssetRegistry,
    ) -> Result<(), HotReloadError> {
        let read_dir = std::fs::read_dir(dir).map_err(|e| HotReloadError::FileRead {
            path: dir.display().to_string(),
            reason: e.to_string(),
        })?;

        for entry in read_dir {
            let entry = entry.map_err(|e| HotReloadError::FileRead {
                path: dir.display().to_string(),
                reason: e.to_string(),
            })?;

            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.ends_with(".prefab"))
            {
                self.load_file(&path, registry)?;
            }
        }

        Ok(())
    }

    /// Загрузить один `.prefab.json` файл.
    ///
    /// Возвращает `AssetId` зарегистрированного ассета.
    pub fn load_file(
        &mut self,
        path: &Path,
        registry: &mut AssetRegistry,
    ) -> Result<AssetId, HotReloadError> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let id = registry.register(canonical.clone());

        // Читаем и загружаем в PrefabLoader
        let content = std::fs::read_to_string(path).map_err(|e| HotReloadError::FileRead {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;

        let manifest: PrefabManifest =
            serde_json::from_str(&content).map_err(|e| HotReloadError::Deserialize {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;

        let prefab_name = manifest.name.clone();

        // Загружаем в кеш
        self.loader
            .load_json(&content)
            .map_err(|e| HotReloadError::Deserialize {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;

        self.prefab_names.insert(id.0, prefab_name.clone());

        self.assets.insert(
            id.0,
            PrefabAsset {
                manifest,
                spawned_entities: Vec::new(),
            },
        );

        log::debug!(
            "[prefab] loaded `{}` as `{}` (AssetId={})",
            path.display(),
            prefab_name,
            id.0
        );

        Ok(id)
    }

    /// Обработать изменение файла префаба.
    ///
    /// Перезагружает префаб в кеш `PrefabLoader`.
    /// Если префаб был загружен — обновляет манифест в `assets`.
    ///
    /// **Не пересоздаёт entity** — используйте [`reapply_asset`](PrefabPlugin::reapply_asset)
    /// после вызова этого метода, если нужно пересоздать entity из обновлённого префаба.
    pub fn on_asset_changed(&mut self, change: &AssetChange) -> Result<(), HotReloadError> {
        let path = &change.path;

        let content = std::fs::read_to_string(path).map_err(|e| HotReloadError::FileRead {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;

        // Перезагружаем в кеш (load_json перезаписывает существующий префаб)
        self.loader
            .load_json(&content)
            .map_err(|e| HotReloadError::Deserialize {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;

        // Обновляем манифест в assets
        if let Ok(manifest) = serde_json::from_str::<PrefabManifest>(&content) {
            let prefab_name = manifest.name.clone();
            if let Some(asset) = self.assets.get_mut(&change.id.0) {
                asset.manifest = manifest;
            }
            self.prefab_names.insert(change.id.0, prefab_name);
        }

        log::info!(
            "[prefab] reloaded `{}` (AssetId={})",
            path.display(),
            change.id.0
        );

        Ok(())
    }

    /// Получить ссылку на внутренний PrefabLoader.
    pub fn loader(&self) -> &PrefabLoader {
        &self.loader
    }

    /// Получить мутабельную ссылку на внутренний PrefabLoader.
    pub fn loader_mut(&mut self) -> &mut PrefabLoader {
        &mut self.loader
    }

    /// Получить информацию об ассете префаба по AssetId.
    pub fn get_asset(&self, id: AssetId) -> Option<&PrefabAsset> {
        self.assets.get(&id.0)
    }

    /// Получить мутабельную ссылку на ассет префаба.
    pub fn get_asset_mut(&mut self, id: AssetId) -> Option<&mut PrefabAsset> {
        self.assets.get_mut(&id.0)
    }

    /// Имя префаба по AssetId.
    pub fn prefab_name(&self, id: AssetId) -> Option<&str> {
        self.prefab_names.get(&id.0).map(|s| s.as_str())
    }

    /// Количество загруженных префабов.
    pub fn len(&self) -> usize {
        self.loader.len()
    }

    pub fn is_empty(&self) -> bool {
        self.loader.is_empty()
    }

    /// Добавить entity в список отслеживаемых для указанного префаба.
    ///
    /// При вызове [`reapply_asset`](PrefabPlugin::reapply_asset) все отслеживаемые entity
    /// будут деспавнены и созданы заново из обновлённого манифеста.
    ///
    /// This records the instance with no parent and no overrides. When the
    /// instance was spawned under a parent or with per-instance overrides, use
    /// [`track_entity_with`](PrefabPlugin::track_entity_with) so hot-reload can
    /// restore that context (E4).
    pub fn track_entity(&mut self, id: AssetId, entity: Entity) {
        self.track_entity_with(id, entity, None, Vec::new());
    }

    /// Track a spawned instance together with its full spawn context.
    ///
    /// `parent` and `overrides` are the exact arguments the instance was
    /// spawned with (via `PrefabLoader::instantiate`). They are replayed on
    /// [`reapply_asset`](PrefabPlugin::reapply_asset) so every instance is
    /// recreated in place — under its parent and with its overrides — instead
    /// of collapsing all instances into one default root entity (E4 fix).
    pub fn track_entity_with(
        &mut self,
        id: AssetId,
        entity: Entity,
        parent: Option<Entity>,
        overrides: Vec<PrefabComponent>,
    ) {
        if let Some(asset) = self.assets.get_mut(&id.0) {
            asset.spawned_entities.push(PrefabInstance {
                entity,
                parent,
                overrides,
            });
        }
    }

    /// Пересоздать все инстансы префаба по `AssetId`.
    ///
    /// For **every** tracked instance (E4 fix — not just one):
    /// 1. Recursively despawns the old root entity (together with its children).
    /// 2. Re-instantiates from the reloaded manifest **with the instance's own
    ///    parent and overrides**, so it reappears in place.
    ///
    /// The tracking list is rebuilt with the new root entities, preserving each
    /// instance's parent/overrides for the next reload. If the prefab had N
    /// instances (three goblins in different spots), it still has N afterwards —
    /// not one collapsed default at the world root.
    pub fn reapply_asset(&mut self, world: &mut World, id: AssetId) -> Result<(), HotReloadError> {
        let prefab_name = self
            .prefab_names
            .get(&id.0)
            .ok_or_else(|| HotReloadError::Deserialize {
                path: "unknown".to_string(),
                reason: format!("prefab with AssetId={} not found", id.0),
            })?
            .clone();

        // Take the tracked instances out so we can borrow `self.loader` mutably
        // while iterating. Each instance carries its own spawn context.
        let instances = match self.assets.get_mut(&id.0) {
            Some(asset) => std::mem::take(&mut asset.spawned_entities),
            None => Vec::new(),
        };

        // Nothing tracked yet — reloading the manifest is enough; there is no
        // instance to recreate. (Manifest was already refreshed by
        // `on_asset_changed`; the cache holds the latest version.)
        if instances.is_empty() {
            return Ok(());
        }

        // Despawn all old roots first (recursively, together with children).
        for inst in &instances {
            world.despawn_recursive(ChildOf, inst.entity);
        }

        // Recreate every instance with its preserved parent/overrides.
        let mut new_instances = Vec::with_capacity(instances.len());
        for inst in instances {
            // Re-fetch the manifest each iteration: `instantiate` borrows the
            // loader immutably, so we cannot hold a manifest ref across the call.
            let manifest = self.loader.get(&prefab_name).ok_or_else(|| {
                HotReloadError::Deserialize {
                    path: "unknown".to_string(),
                    reason: format!(
                        "prefab `{}` not found in cache after reload",
                        prefab_name
                    ),
                }
            })?;

            let new_entity = self
                .loader
                .instantiate(world, manifest, &inst.overrides, inst.parent, None)
                .map_err(|e| HotReloadError::Deserialize {
                    path: "unknown".to_string(),
                    reason: e.to_string(),
                })?;

            new_instances.push(PrefabInstance {
                entity: new_entity,
                parent: inst.parent,
                overrides: inst.overrides,
            });
        }

        let count = new_instances.len();

        // Store the rebuilt instance list (with fresh root entities).
        if let Some(asset) = self.assets.get_mut(&id.0) {
            asset.spawned_entities = new_instances;
        }

        log::info!(
            "[prefab] reapplied `{}` (AssetId={}) — respawned {} instance(s)",
            prefab_name,
            id.0,
            count,
        );

        Ok(())
    }

    /// Пересоздать все entity из всех зарегистрированных префабов.
    ///
    /// Полезно для полного обновления сцены после массовых изменений.
    pub fn reapply_all(&mut self, world: &mut World) -> Result<(), HotReloadError> {
        let ids: Vec<AssetId> = self.prefab_names.keys().copied().map(AssetId).collect();
        for id in ids {
            self.reapply_asset(world, id)?;
        }
        Ok(())
    }
}

impl Default for PrefabPlugin {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::relations::ChildOf;
    use apex_core::world::World;

    /// Minimal serializable component for the E4 test. Hand-implements
    /// `Component` (the derive macro is not a dependency of this crate).
    /// Its `value` is what we override per-instance and assert survives reload.
    #[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Tag {
        value: i32,
    }
    impl apex_core::component::Component for Tag {}

    /// Build a self-contained `.prefab.json` for `Tag` with a default value.
    /// The `type_name` must equal `std::any::type_name::<Tag>()` (that is how
    /// the registry names hand-registered components).
    fn tag_prefab_json(name: &str, default_value: i32) -> String {
        format!(
            r#"{{ "name": "{name}", "components": [ {{ "type_name": "{ty}", "value": {{ "value": {v} }} }} ] }}"#,
            name = name,
            ty = std::any::type_name::<Tag>(),
            v = default_value,
        )
    }

    /// One `PrefabComponent` override setting `Tag.value`.
    fn tag_override(value: i32) -> PrefabComponent {
        PrefabComponent {
            type_name: std::any::type_name::<Tag>().to_string(),
            value: serde_json::json!({ "value": value }),
        }
    }

    /// E4 regression: hot-reloading a prefab with N tracked instances must
    /// recreate all N (each under its own parent, with its own overrides) —
    /// not collapse them into a single default instance at the world root.
    #[test]
    fn reapply_preserves_all_instances_with_parent_and_overrides() {
        let mut world = World::new();
        world.register_component_serde_json::<Tag>();

        let mut plugin = PrefabPlugin::new();
        let mut registry = AssetRegistry::new();

        // Write the prefab file and register it.
        let dir = std::env::temp_dir().join("apex_prefab_e4_test");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("goblin.prefab.json");
        std::fs::write(&file_path, tag_prefab_json("Goblin", 0)).unwrap();
        let id = plugin.load_file(&file_path, &mut registry).unwrap();

        // Three parents in the world (distinct "spots"), each hosting one goblin
        // with its own override value.
        let parents = [world.spawn(()), world.spawn(()), world.spawn(())];
        let override_vals = [11, 22, 33];

        let manifest = plugin.loader().get("Goblin").unwrap().clone();
        for (parent, &ov) in parents.iter().zip(override_vals.iter()) {
            let overrides = vec![tag_override(ov)];
            let e = plugin
                .loader()
                .instantiate(&mut world, &manifest, &overrides, Some(*parent), None)
                .unwrap();
            plugin.track_entity_with(id, e, Some(*parent), overrides);
        }

        // Sanity: three instances tracked, each with correct parent + value.
        assert_eq!(plugin.get_asset(id).unwrap().spawned_entities.len(), 3);
        for (inst, (&parent, &ov)) in plugin
            .get_asset(id)
            .unwrap()
            .spawned_entities
            .iter()
            .zip(parents.iter().zip(override_vals.iter()))
        {
            assert_eq!(world.target_of(inst.entity, ChildOf), Some(parent));
            assert_eq!(world.get::<Tag>(inst.entity), Some(&Tag { value: ov }));
        }

        // Hot-reload: bump the prefab default (irrelevant — overrides win) and reapply.
        std::fs::write(&file_path, tag_prefab_json("Goblin", 999)).unwrap();
        plugin
            .on_asset_changed(&AssetChange { id, path: file_path.clone() })
            .unwrap();
        plugin.reapply_asset(&mut world, id).unwrap();

        // Still THREE instances (E4: not collapsed to one).
        let instances = &plugin.get_asset(id).unwrap().spawned_entities;
        assert_eq!(instances.len(), 3, "reapply collapsed instances");

        // Each recreated instance kept its parent and its override value.
        let mut seen_vals = Vec::new();
        for inst in instances {
            let parent = world
                .target_of(inst.entity, ChildOf)
                .expect("recreated instance lost its parent");
            assert!(parents.contains(&parent), "recreated under an unknown parent");
            let tag = world.get::<Tag>(inst.entity).expect("recreated instance lost Tag");
            // Override must win over the reloaded default (999).
            assert_ne!(tag.value, 999, "override was lost — reverted to reloaded default");
            seen_vals.push(tag.value);
        }
        seen_vals.sort();
        assert_eq!(seen_vals, vec![11, 22, 33], "per-instance overrides not preserved");
    }

    #[test]
    fn prefab_plugin_new_is_empty() {
        let plugin = PrefabPlugin::new();
        assert!(plugin.is_empty());
        assert_eq!(plugin.len(), 0);
    }

    #[test]
    fn prefab_plugin_load_from_str() {
        let mut plugin = PrefabPlugin::new();
        let mut registry = AssetRegistry::new();

        // Создаём временный .prefab.json файл
        let dir = std::env::temp_dir().join("apex_prefab_test");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test_entity.prefab.json");

        let json = r#"{
            "name": "TestEntity",
            "components": [
                { "type_name": "test::Health", "value": { "current": 100, "max": 100 } }
            ]
        }"#;

        std::fs::write(&file_path, json).unwrap();

        let id = plugin.load_file(&file_path, &mut registry).unwrap();
        assert!(plugin.loader().has("TestEntity"));
        assert_eq!(plugin.prefab_name(id), Some("TestEntity"));

        // Cleanup
        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn prefab_plugin_reload_updates_cache() {
        let mut plugin = PrefabPlugin::new();
        let mut registry = AssetRegistry::new();

        let dir = std::env::temp_dir().join("apex_prefab_reload_test");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("reload_test.prefab.json");

        // Первая версия
        std::fs::write(
            &file_path,
            r#"{"name": "V1", "components": []}"#,
        )
        .unwrap();

        let id = plugin.load_file(&file_path, &mut registry).unwrap();
        assert!(plugin.loader().has("V1"));

        // Вторая версия — перезаписываем файл
        std::fs::write(
            &file_path,
            r#"{"name": "V2", "components": []}"#,
        )
        .unwrap();

        let change = AssetChange {
            id,
            path: file_path.clone(),
        };
        plugin.on_asset_changed(&change).unwrap();

        // После reload — имя должно обновиться
        assert!(plugin.loader().has("V2"));
        assert_eq!(plugin.prefab_name(id), Some("V2"));

        // Cleanup
        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn prefab_plugin_load_directory() {
        let mut plugin = PrefabPlugin::new();
        let mut registry = AssetRegistry::new();

        let dir = std::env::temp_dir().join("apex_prefab_dir_test");
        let _ = std::fs::create_dir_all(&dir);

        // Создаём несколько prefab файлов
        std::fs::write(
            dir.join("a.prefab.json"),
            r#"{"name": "A", "components": []}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.prefab.json"),
            r#"{"name": "B", "components": []}"#,
        )
        .unwrap();

        // Не prefab файл — игнорируется
        std::fs::write(dir.join("not_prefab.json"), r#"{}"#).unwrap();

        plugin.load_directory(&dir, &mut registry).unwrap();
        assert_eq!(plugin.len(), 2);
        assert!(plugin.loader().has("A"));
        assert!(plugin.loader().has("B"));

        // Cleanup
        let _ = std::fs::remove_file(dir.join("a.prefab.json"));
        let _ = std::fs::remove_file(dir.join("b.prefab.json"));
        let _ = std::fs::remove_file(dir.join("not_prefab.json"));
        let _ = std::fs::remove_dir(&dir);
    }
}
