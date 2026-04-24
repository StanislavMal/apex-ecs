//! AssetRegistry — маппинг путей к файлам на зарегистрированные ассеты.

use std::path::PathBuf;

use rustc_hash::FxHashMap;

/// Уникальный идентификатор зарегистрированного ассета.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AssetId(pub u32);

/// Изменение ассета — результат `AssetRegistry::process_changes`.
#[derive(Debug, Clone)]
pub struct AssetChange {
    pub id:   AssetId,
    pub path: PathBuf,
}

/// Маппинг `PathBuf → AssetId` для быстрого поиска при получении file event.
pub struct AssetRegistry {
    /// path → (asset_id, loader_key)
    path_to_asset: FxHashMap<PathBuf, AssetId>,
    /// asset_id → path (для диагностики)
    asset_to_path: FxHashMap<u32, PathBuf>,
    next_id:       u32,
}

impl AssetRegistry {
    pub fn new() -> Self {
        Self {
            path_to_asset: FxHashMap::default(),
            asset_to_path: FxHashMap::default(),
            next_id:       0,
        }
    }

    /// Зарегистрировать путь к файлу, получить AssetId.
    ///
    /// Если путь уже зарегистрирован — возвращает существующий ID.
    pub fn register(&mut self, path: PathBuf) -> AssetId {
        if let Some(&id) = self.path_to_asset.get(&path) {
            return id;
        }
        let id = AssetId(self.next_id);
        self.next_id += 1;
        self.asset_to_path.insert(id.0, path.clone());
        self.path_to_asset.insert(path, id);
        id
    }

    /// Преобразовать список изменённых путей в список изменённых AssetId.
    ///
    /// Пути не зарегистрированные в registry — игнорируются.
    pub fn process_changes<'a>(
        &self,
        changed_paths: impl Iterator<Item = &'a PathBuf>,
    ) -> Vec<AssetChange> {
        changed_paths
            .filter_map(|path| {
                // Нормализуем путь (абсолютный vs относительный)
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                self.path_to_asset
                    .get(&canonical)
                    .or_else(|| self.path_to_asset.get(path))
                    .map(|&id| AssetChange { id, path: path.clone() })
            })
            .collect()
    }

    pub fn path_of(&self, id: AssetId) -> Option<&PathBuf> {
        self.asset_to_path.get(&id.0)
    }

    pub fn len(&self)      -> usize { self.path_to_asset.len() }
    pub fn is_empty(&self) -> bool  { self.path_to_asset.is_empty() }
}

impl Default for AssetRegistry {
    fn default() -> Self { Self::new() }
}