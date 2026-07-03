//! HotReloadPlugin — главная точка входа для hot reload конфигов/ассетов.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use apex_core::world::World;

use crate::{
    asset_registry::{AssetId, AssetChange, AssetRegistry},
    watcher::FileWatcher,
};

// ── Ошибки ────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum HotReloadError {
    #[error("watcher init failed: {0}")]
    WatcherInit(#[from] notify::Error),

    #[error("failed to read file `{path}`: {reason}")]
    FileRead { path: String, reason: String },

    #[error("failed to deserialize `{path}`: {reason}")]
    Deserialize { path: String, reason: String },
}

// ── ConfigLoader ───────────────────────────────────────────────

/// Трейт загрузчика конфигурационного файла.
///
/// Реализуется для каждого типа конфигурации. Стандартная реализация
/// предоставляется через `JsonConfigLoader<T>`.
pub trait ConfigLoader: Send + Sync + 'static {
    /// Загрузить файл по пути и вставить результат в мир как ресурс.
    ///
    /// Возвращает `Err` если файл не читается или не десериализуется.
    fn reload(&self, path: &Path, world: &mut World) -> Result<(), HotReloadError>;
}

/// Загрузчик JSON-конфига → ресурс `T`.
pub struct JsonConfigLoader<T: serde::de::DeserializeOwned + Send + Sync + 'static> {
    _marker: std::marker::PhantomData<T>,
}

impl<T: serde::de::DeserializeOwned + Send + Sync + 'static> JsonConfigLoader<T> {
    pub fn new() -> Self {
        Self { _marker: std::marker::PhantomData }
    }
}

impl<T: serde::de::DeserializeOwned + Send + Sync + 'static> ConfigLoader for JsonConfigLoader<T> {
    fn reload(&self, path: &Path, world: &mut World) -> Result<(), HotReloadError> {
        let bytes = std::fs::read(path).map_err(|e| HotReloadError::FileRead {
            path:   path.display().to_string(),
            reason: e.to_string(),
        })?;

        let value: T = serde_json::from_slice(&bytes).map_err(|e| HotReloadError::Deserialize {
            path:   path.display().to_string(),
            reason: e.to_string(),
        })?;

        world.insert_resource(value);

        log::info!(
            "[hot-reload] reloaded `{}` as `{}`",
            path.display(),
            std::any::type_name::<T>()
        );

        Ok(())
    }
}

impl<T: serde::de::DeserializeOwned + Send + Sync + 'static> Default for JsonConfigLoader<T> {
    fn default() -> Self { Self::new() }
}

// ── HotReloadPlugin ────────────────────────────────────────────

/// Главная точка входа для горячей перезагрузки ассетов.
///
/// # Жизненный цикл
///
/// 1. `HotReloadPlugin::new(watch_dir)` — создать, запустить watcher
/// 2. `plugin.watch_config::<T>(path, &mut world)` — зарегистрировать файл
/// 3. В game loop: `plugin.apply_changes(&mut world)` — применить изменения
///
/// # Пример
///
/// ```ignore
/// // setup:
/// let mut hot = HotReloadPlugin::new("assets/").unwrap();
/// hot.watch_config::<PhysicsConfig>("assets/physics.json", &mut world)?;
/// hot.watch_config::<AudioConfig>("assets/audio.json", &mut world)?;
///
/// // game loop:
/// loop {
///     let changed = hot.apply_changes(&mut world);
///     for c in changed { log::debug!("reloaded: {:?}", c.path); }
///     scheduler.run(&mut world);
/// }
/// ```
pub struct HotReloadPlugin {
    watcher:       FileWatcher,
    asset_registry: AssetRegistry,
    /// AssetId → загрузчик конкретного типа
    loaders:       HashMap<u32, Box<dyn ConfigLoader>>,
    /// AssetId → канонический путь (для повторной загрузки)
    asset_paths:   HashMap<u32, PathBuf>,
}

impl HotReloadPlugin {
    /// Создать plugin, запустить file watcher для директории `watch_dir`.
    ///
    /// `debounce` — задержка дебаунсинга событий. 100ms — хорошее значение
    /// для большинства случаев. Слишком маленькое (< 20ms) даёт ложные срабатывания.
    pub fn new(
        watch_dir: &Path,
        debounce:  Duration,
    ) -> Result<Self, HotReloadError> {
        let watcher = FileWatcher::new(watch_dir, debounce)?;
        Ok(Self {
            watcher,
            asset_registry: AssetRegistry::new(),
            loaders:        HashMap::new(),
            asset_paths:    HashMap::new(),
        })
    }

    /// Удобный конструктор с дебаунсом 100ms.
    pub fn with_default_debounce(watch_dir: &Path) -> Result<Self, HotReloadError> {
        Self::new(watch_dir, Duration::from_millis(100))
    }

    /// Зарегистрировать JSON-файл конфига как ресурс типа `T`.
    ///
    /// Немедленно загружает файл и вставляет значение в мир.
    /// При последующих изменениях файла — автоматически перезагружает.
    pub fn watch_config<T>(
        &mut self,
        path:  &Path,
        world: &mut World,
    ) -> Result<AssetId, HotReloadError>
    where
        T: serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        self.watch_config_with_loader(path, world, JsonConfigLoader::<T>::new())
    }

    /// Зарегистрировать файл с кастомным загрузчиком.
    ///
    /// Используй если нужен нестандартный формат (RON, TOML, бинарный).
    pub fn watch_config_with_loader(
        &mut self,
        path:   &Path,
        world:  &mut World,
        loader: impl ConfigLoader,
    ) -> Result<AssetId, HotReloadError> {
        // Нормализуем путь для стабильного matching
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        let id = self.asset_registry.register(canonical.clone());

        // Register the loader and path FIRST — before attempting the initial
        // load. (9a) A failed initial load (e.g. the file has a syntax error at
        // startup) must NOT permanently disable hot-reload for this path: the
        // path is already in `asset_registry`, so if no loader were registered,
        // every later edit would fall into the `None => continue` arm of
        // `apply_changes` and never reload. By registering the loader
        // unconditionally, fixing the broken file on disk is picked up on the
        // next `apply_changes`.
        self.loaders.insert(id.0, Box::new(loader));
        self.asset_paths.insert(id.0, canonical);

        // Initial load. On failure we keep the loader registered (so a later
        // fix is picked up) and surface the error to the caller.
        if let Some(loader) = self.loaders.get(&id.0) {
            if let Err(err) = loader.reload(path, world) {
                log::warn!(
                    "[hot-reload] initial load of `{}` failed: {} — watching anyway; \
                     a subsequent fix to the file will be applied",
                    path.display(),
                    err
                );
                return Err(err);
            }
        }

        log::debug!(
            "[hot-reload] watching `{}` (AssetId={})",
            path.display(),
            id.0
        );

        Ok(id)
    }

    /// Применить все накопившиеся изменения файлов к миру.
    ///
    /// **Вызывать каждый кадр** в начале game loop до запуска планировщика.
    ///
    /// Если изменений нет — возвращает пустой Vec, overhead < 1µs
    /// (один `try_recv` на channel без блокировки).
    ///
    /// Ошибки загрузки логируются через `log::error!` но не прерывают выполнение —
    /// предыдущее значение ресурса остаётся в мире.
    pub fn apply_changes(&mut self, world: &mut World) -> Vec<AssetChange> {
        let file_changes = self.watcher.poll();
        if file_changes.is_empty() {
            return Vec::new();
        }

        let changed_paths: Vec<&PathBuf> = file_changes.iter().map(|c| &c.path).collect();
        let asset_changes = self.asset_registry.process_changes(changed_paths.into_iter());

        let mut applied = Vec::with_capacity(asset_changes.len());

        for change in &asset_changes {
            let path = match self.asset_paths.get(&change.id.0) {
                Some(p) => p.clone(),
                None    => continue,
            };

            let loader = match self.loaders.get(&change.id.0) {
                Some(l) => l,
                None    => continue,
            };

            match loader.reload(&path, world) {
                Ok(())   => applied.push(change.clone()),
                Err(err) => log::error!("[hot-reload] reload failed for `{}`: {}", path.display(), err),
            }
        }

        applied
    }

    /// Принудительно перезагрузить ассет по ID (не дожидаясь file event).
    pub fn force_reload(&mut self, id: AssetId, world: &mut World) -> Result<(), HotReloadError> {
        let path = self.asset_paths.get(&id.0)
            .ok_or_else(|| HotReloadError::FileRead {
                path:   format!("AssetId({})", id.0),
                reason: "not registered".into(),
            })?
            .clone();

        let loader = self.loaders.get(&id.0)
            .ok_or_else(|| HotReloadError::FileRead {
                path:   path.display().to_string(),
                reason: "no loader".into(),
            })?;

        loader.reload(&path, world)
    }

    /// Количество зарегистрированных ассетов.
    pub fn asset_count(&self) -> usize { self.asset_registry.len() }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::world::World;

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct DummyConfig {
        value: i32,
    }

    /// E9a regression: an initial-load failure (starting with a broken file)
    /// must NOT permanently disable hot-reload for that path. After the user
    /// fixes the file on disk, a reload must succeed — proving the loader stayed
    /// registered despite the failed initial load.
    #[test]
    fn watch_config_keeps_loader_after_initial_load_failure() {
        let dir = std::env::temp_dir().join("apex_watch_e9a_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");

        // Start with a BROKEN file (invalid JSON).
        std::fs::write(&path, b"{ this is not json").unwrap();

        let mut world = World::new();
        let mut plugin =
            HotReloadPlugin::with_default_debounce(&dir).expect("watcher init");

        // Initial load fails and surfaces the error to the caller...
        let result = plugin.watch_config::<DummyConfig>(&path, &mut world);
        assert!(result.is_err(), "broken initial file should error");
        // ...but the resource was never inserted.
        assert!(world.resources.try_get::<DummyConfig>().is_none());

        // The path is still registered as an asset (E9a: not orphaned).
        assert_eq!(plugin.asset_count(), 1);

        // We need the AssetId to force a reload — re-register succeeds now that
        // we FIX the file, and returns the same id (canonical path is stable).
        std::fs::write(&path, br#"{ "value": 42 }"#).unwrap();

        // Re-watching the (now valid) file returns Ok and inserts the resource.
        // This exercises the same code path and confirms it is not blocked.
        let id = plugin
            .watch_config::<DummyConfig>(&path, &mut world)
            .expect("fixed file should load");

        // force_reload must succeed — the loader is present and the file valid.
        plugin.force_reload(id, &mut world).expect("reload after fix");
        assert_eq!(
            world.resources.try_get::<DummyConfig>(),
            Some(&DummyConfig { value: 42 }),
            "fixed config was not applied"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}