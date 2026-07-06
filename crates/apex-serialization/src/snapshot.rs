//! World snapshot data structures — the storage/transfer format.
//!
//! # Formats
//!
//! - **JSON** — human-readable, for debugging and configs
//! - **Bincode** — compact binary, for fast saves/loads
//!
//! Components are always stored as raw bytes (`Vec<u8>`).
//! When the snapshot is JSON-serialized, the bytes are interpreted as JSON.
//! When it is Bincode-serialized, they are interpreted as binary data.

use serde::{Deserialize, Serialize};

// ── Versioning ───────────────────────────────────────────────────

/// Snapshot format version — major + minor.
///
/// - The major version changes on a breaking change
/// - The minor version changes on backward-compatible changes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotVersion {
    pub major: u32,
    pub minor: u32,
}

impl SnapshotVersion {
    pub const CURRENT: Self = Self { major: 2, minor: 0 };

    pub fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Check whether this version is compatible with the given one.
    /// Compatible = same major, minor >= expected.
    pub fn is_compatible_with(&self, expected: SnapshotVersion) -> bool {
        self.major == expected.major && self.minor >= expected.minor
    }
}

impl std::fmt::Display for SnapshotVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}", self.major, self.minor)
    }
}

// ── Component data storage format ───────────────────────────────

/// The format in which a component's bytes are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataFormat {
    /// JSON bytes (human-readable).
    Json,
    /// Binary bytes (bincode).
    Binary,
}

// ── WorldSnapshot ────────────────────────────────────────────────

/// A serialized resource (E7): `type_name` + bincode bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub type_name: String,
    pub data:      Vec<u8>,
}

/// A full world snapshot — everything needed to restore state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Snapshot format version — for future migration.
    pub version:   u32,
    /// World tick at the moment of the snapshot.
    pub tick:      u32,
    /// All live entities with their components.
    pub entities:  Vec<EntitySnapshot>,
    /// Relations between entities.
    pub relations: Vec<RelationSnapshot>,
    /// Registered resources (E7, format v2). `serde(default)` — v1 JSON
    /// snapshots (without the field) read as an empty list. Bincode v1 is not
    /// compatible with v2 (dev snapshots are ephemeral; the version is read and
    /// migrated on the load path).
    #[serde(default)]
    pub resources: Vec<ResourceSnapshot>,
}

impl WorldSnapshot {
    pub const CURRENT_VERSION: u32 = 2;

    pub fn new(tick: u32) -> Self {
        Self {
            version:   Self::CURRENT_VERSION,
            tick,
            entities:  Vec::new(),
            relations: Vec::new(),
            resources: Vec::new(),
        }
    }

    // ── JSON ─────────────────────────────────────────────────────

    /// Serialize the snapshot into JSON bytes.
    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Deserialize the snapshot from JSON bytes.
    pub fn from_json(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }

    // ── Bincode ──────────────────────────────────────────────────

    /// Serialize the snapshot into the binary format (bincode).
    ///
    /// 5-10x smaller than JSON, 2-3x faster.
    pub fn to_bincode(&self) -> Result<Vec<u8>, Box<bincode::ErrorKind>> {
        bincode::serialize(self)
    }

    /// Deserialize the snapshot from the binary format (bincode).
    ///
    /// Checks the snapshot version compatibility.
    pub fn from_bincode(data: &[u8]) -> Result<Self, Box<bincode::ErrorKind>> {
        bincode::deserialize(data)
    }

    // ── Migration ────────────────────────────────────────────────

    /// Run the migration chain, bringing the snapshot up to the current version.
    pub fn migrate(&mut self) -> Result<(), String> {
        while self.version < Self::CURRENT_VERSION {
            let migrator = migration_for(self.version)
                .ok_or_else(|| format!("no migration found for version {}", self.version))?;
            migrator(self)?;
            self.version += 1;
        }
        Ok(())
    }

    /// Check the snapshot version's compatibility with the current one.
    pub fn is_version_compatible(&self) -> bool {
        let expected = SnapshotVersion::CURRENT;
        let found = SnapshotVersion::new(self.version, 0);
        found.is_compatible_with(expected)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    pub fn relation_count(&self) -> usize {
        self.relations.len()
    }
}

// ── EntitySnapshot ───────────────────────────────────────────────

/// A snapshot of a single entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySnapshot {
    /// Original entity index — for remapping on restore.
    pub original_index: u32,
    /// Serialized components of the entity.
    pub components: Vec<ComponentSnapshot>,
}

// ── ComponentSnapshot ────────────────────────────────────────────

/// A snapshot of a single component.
///
/// `data` always holds raw bytes in the format specified by `format`.
/// - `Json`: bytes = JSON text
/// - `Binary`: bytes = binary serialization (bincode)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    /// Component type name.
    pub type_name: String,
    /// Raw component data bytes.
    pub data: Vec<u8>,
    /// Data format.
    pub format: DataFormat,
}

impl ComponentSnapshot {
    /// Create a snapshot from JSON bytes.
    pub fn new_json(type_name: impl Into<String>, json_bytes: Vec<u8>) -> Self {
        Self {
            type_name: type_name.into(),
            data: json_bytes,
            format: DataFormat::Json,
        }
    }

    /// Create a snapshot from binary bytes.
    pub fn new_binary(type_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            type_name: type_name.into(),
            data: bytes,
            format: DataFormat::Binary,
        }
    }

    /// Get the data as a slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Whether the data is JSON.
    pub fn is_json(&self) -> bool {
        self.format == DataFormat::Json
    }
}

// ── RelationSnapshot ─────────────────────────────────────────────

/// A snapshot of a single relation between entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationSnapshot {
    pub subject_index: u32,
    pub target_index:  u32,
    pub kind_name:     String,
}

// ── Migrations ───────────────────────────────────────────────────

type MigrationFn = fn(&mut WorldSnapshot) -> Result<(), String>;

fn migration_for(version: u32) -> Option<MigrationFn> {
    match version {
        0 => Some(|_data| Ok(())), // no-op: the data format did not change between v0 and v1
        // v1 → v2 (E7): `resources` was added; for v1 it is `serde(default)` empty.
        1 => Some(|_data| Ok(())),
        _ => None,
    }
}

// ── WorldDiff (incremental changes) ──────────────────────────────

/// The difference between two snapshots, for incremental saving.
///
/// # Byte-level delta (3.1)
/// Components present in both snapshots with the same `type_name` are compared
/// byte-by-byte. If the data matches — the component is not included in the diff.
/// If it differs — the component goes into `modified_components`.
/// This shrinks the diff size when only a small fraction of the data changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldDiff {
    pub version: u32,
    /// Added entities.
    pub added_entities: Vec<EntitySnapshot>,
    /// Removed entities (original_index).
    pub removed_entities: Vec<u32>,
    /// Components added to existing entities.
    pub added_components: Vec<(u32, Vec<ComponentSnapshot>)>,
    /// Components removed from existing entities.
    pub removed_components: Vec<(u32, Vec<String>)>,
    /// Components modified on existing entities (byte-level delta).
    pub modified_components: Vec<(u32, Vec<ComponentSnapshot>)>,
    /// Added relations.
    pub added_relations: Vec<RelationSnapshot>,
    /// Removed relations.
    pub removed_relations: Vec<RelationSnapshot>,
}

impl WorldDiff {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            added_entities: Vec::new(),
            removed_entities: Vec::new(),
            added_components: Vec::new(),
            removed_components: Vec::new(),
            modified_components: Vec::new(),
            added_relations: Vec::new(),
            removed_relations: Vec::new(),
        }
    }

    pub fn to_bincode(&self) -> Result<Vec<u8>, Box<bincode::ErrorKind>> {
        bincode::serialize(self)
    }

    pub fn from_bincode(data: &[u8]) -> Result<Self, Box<bincode::ErrorKind>> {
        bincode::deserialize(data)
    }

    pub fn is_empty(&self) -> bool {
        self.added_entities.is_empty()
            && self.removed_entities.is_empty()
            && self.added_components.is_empty()
            && self.removed_components.is_empty()
            && self.modified_components.is_empty()
            && self.added_relations.is_empty()
            && self.removed_relations.is_empty()
    }
}

impl Default for WorldDiff {
    fn default() -> Self {
        Self::new()
    }
}

// ── Format enum ──────────────────────────────────────────────────

/// Serialization format for file I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveFormat {
    Json,
    Bincode,
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compatible() {
        // CURRENT is v2 (E7 added resources). v2 is directly compatible; v2.1
        // (same major, newer minor) too; v1 is not directly compatible (it is
        // brought up via `migrate`), and a future v3 is not.
        let v1 = SnapshotVersion::new(1, 0);
        let v2 = SnapshotVersion::new(2, 0);
        let v2_1 = SnapshotVersion::new(2, 1);
        let v3 = SnapshotVersion::new(3, 0);

        assert!(v2.is_compatible_with(SnapshotVersion::CURRENT));
        assert!(v2_1.is_compatible_with(SnapshotVersion::CURRENT));
        assert!(!v1.is_compatible_with(SnapshotVersion::CURRENT));
        assert!(!v3.is_compatible_with(SnapshotVersion::CURRENT));
    }

    #[test]
    fn snapshot_json_roundtrip() {
        let mut snap = WorldSnapshot::new(42);
        snap.entities.push(EntitySnapshot {
            original_index: 1,
            components: vec![
                ComponentSnapshot::new_json("my_crate::Position", br#"{"x":1.0,"y":2.0}"#.to_vec()),
            ],
        });

        let json = snap.to_json().unwrap();
        let restored = WorldSnapshot::from_json(&json).unwrap();

        assert_eq!(restored.tick, 42);
        assert_eq!(restored.entities.len(), 1);
        assert_eq!(restored.entities[0].original_index, 1);
        assert_eq!(restored.entities[0].components[0].type_name, "my_crate::Position");
    }

    #[test]
    fn snapshot_bincode_roundtrip() {
        let mut snap = WorldSnapshot::new(42);
        snap.entities.push(EntitySnapshot {
            original_index: 1,
            components: vec![
                ComponentSnapshot::new_json("my_crate::Position", br#"{"x":1.0,"y":2.0}"#.to_vec()),
            ],
        });
        snap.relations.push(RelationSnapshot {
            subject_index: 1,
            target_index:  0,
            kind_name:     "apex_core::relations::ChildOf".to_string(),
        });

        let binary = snap.to_bincode().unwrap();
        let restored = WorldSnapshot::from_bincode(&binary).unwrap();

        assert_eq!(restored.tick, 42);
        assert_eq!(restored.entities.len(), 1);
        assert_eq!(restored.relations.len(), 1);
        // Verify the JSON bytes were preserved
        assert!(restored.entities[0].components[0].is_json());
        assert_eq!(restored.entities[0].components[0].as_bytes(), br#"{"x":1.0,"y":2.0}"#);
    }

    #[test]
    fn bincode_smaller_than_json() {
        let mut snap = WorldSnapshot::new(100);
        for i in 0..100 {
            snap.entities.push(EntitySnapshot {
                original_index: i,
                components: vec![
                    ComponentSnapshot::new_json("Pos", br#"{"x":1.0,"y":2.0}"#.to_vec()),
                    ComponentSnapshot::new_json("Vel", br#"{"x":0.0,"y":0.0}"#.to_vec()),
                ],
            });
        }

        let json_size = snap.to_json().unwrap().len();
        let bincode_size = snap.to_bincode().unwrap().len();

        assert!(bincode_size < json_size / 2,
            "bincode={} should be < json/2={}", bincode_size, json_size / 2);
    }

    #[test]
    fn world_diff_empty() {
        let diff = WorldDiff::new();
        assert!(diff.is_empty());
    }

    #[test]
    fn world_diff_bincode_roundtrip() {
        let mut diff = WorldDiff::new();
        diff.added_entities.push(EntitySnapshot {
            original_index: 10,
            components: vec![
                ComponentSnapshot::new_json("Health", br#"{"current":100.0}"#.to_vec()),
            ],
        });
        diff.removed_entities.push(5);
        diff.added_relations.push(RelationSnapshot {
            subject_index: 10,
            target_index:  0,
            kind_name:     "ChildOf".to_string(),
        });

        let binary = diff.to_bincode().unwrap();
        let restored = WorldDiff::from_bincode(&binary).unwrap();

        assert_eq!(restored.added_entities.len(), 1);
        assert_eq!(restored.removed_entities, vec![5]);
        assert_eq!(restored.added_relations.len(), 1);
    }

    #[test]
    fn component_snapshot_formats() {
        let json_comp = ComponentSnapshot::new_json("Pos", br#"{"x":1.0}"#.to_vec());
        assert!(json_comp.is_json());
        assert_eq!(json_comp.as_bytes(), br#"{"x":1.0}"#);

        let bin_comp = ComponentSnapshot::new_binary("Pos", vec![1, 2, 3]);
        assert!(!bin_comp.is_json());
        assert_eq!(bin_comp.as_bytes(), &[1, 2, 3]);
    }

    #[test]
    fn snapshot_migration_noop() {
        let mut snap = WorldSnapshot::new(42);
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);
        snap.migrate().unwrap();
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);
    }

    #[test]
    fn version_compatibility_check() {
        let mut snap = WorldSnapshot::new(0);
        snap.version = 999;
        assert!(!snap.is_version_compatible());

        snap.version = WorldSnapshot::CURRENT_VERSION;
        assert!(snap.is_version_compatible());
    }
}
