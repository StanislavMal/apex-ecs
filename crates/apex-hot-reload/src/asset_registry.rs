//! AssetRegistry — maps file paths to registered assets.

use std::path::PathBuf;

use rustc_hash::FxHashMap;

/// Unique identifier of a registered asset.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AssetId(pub u32);

/// An asset change — the result of `AssetRegistry::process_changes`.
#[derive(Debug, Clone)]
pub struct AssetChange {
    pub id:   AssetId,
    pub path: PathBuf,
}

/// Maps `PathBuf → AssetId` for fast lookup when a file event is received.
pub struct AssetRegistry {
    /// path → (asset_id, loader_key)
    path_to_asset: FxHashMap<PathBuf, AssetId>,
    /// asset_id → path (for diagnostics)
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

    /// Register a file path, obtaining an AssetId.
    ///
    /// If the path is already registered — returns the existing ID.
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

    /// Convert a list of changed paths into a list of changed AssetIds.
    ///
    /// Paths not registered in the registry are ignored.
    pub fn process_changes<'a>(
        &self,
        changed_paths: impl Iterator<Item = &'a PathBuf>,
    ) -> Vec<AssetChange> {
        changed_paths
            .filter_map(|path| {
                // Normalize the path (absolute vs relative)
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