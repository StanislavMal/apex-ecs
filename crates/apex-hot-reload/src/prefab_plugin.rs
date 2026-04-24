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
    path::{Path, PathBuf},
};

use apex_core::entity::Entity;
use apex_core::world::World;
use apex_serialization::prefab::{PrefabError, PrefabLoader, PrefabManifest};

use crate::asset_registry::{AssetChange, AssetId, AssetRegistry};
use crate::plugin::HotReloadError;

/// Ассет, представляющий загруженный префаб.
///
/// Хранит манифест префаба и список entity, созданных из него
/// (для поддержки hot-reload — пересоздания entity при изменении файла).
#[derive(Debug, Clone)]
pub struct PrefabAsset {
    /// Манифест префаба (компоненты + дети).
    pub manifest: PrefabManifest,
    /// Entity, созданные из этого префаба.
    /// Заполняется внешним кодом при спавне.
    pub spawned_entities: Vec<Entity>,
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
                    .map_or(false, |s| s.ends_with(".prefab"))
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
    /// **Не пересоздаёт entity** — это ответственность внешнего кода,
    /// который может использовать [`PrefabAsset::spawned_entities`] для этого.
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
    use std::path::PathBuf;

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
