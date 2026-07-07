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
//
// The wire format has ONE version scheme: the `u32` on the envelope
// ([`WorldSnapshot::version`]) plus the migration chain ([`WorldSnapshot::migrate`]).
// "Compatible" is defined operationally — a snapshot is loadable iff it can be
// migrated up to [`WorldSnapshot::CURRENT_VERSION`] (older versions migrate;
// the current version loads as-is; a newer version is rejected because no
// forward migrator exists). There is no separate semver type: a second,
// unused `SnapshotVersion { major, minor }` used to shadow this `u32` with a
// contradictory (range-based) compatibility policy and was removed so the
// version has a single source of truth. [`WorldDiff`] shares this same wire
// version — it is a delta over the same versioned wire structs, so its version
// tracks [`WorldSnapshot::CURRENT_VERSION`] and is checked on apply.

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
///
/// This is the **in-memory** shape: component/relation/resource type names are
/// stored inline. It is representation-agnostic — the on-disk wire form is
/// chosen by the (de)serialization methods below. Since v3 those methods intern
/// the repeated type names into a string table (see [`crate::wire`]); the struct
/// itself is unchanged, so older inline files still parse directly into it.
///
/// The field layout must stay stable: legacy (v≤2) bincode is positional and
/// parses straight into this struct, so reordering/removing a field would break
/// old saves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Snapshot format version — the single wire-version scheme (see `migrate`).
    pub version:   u32,
    /// World tick at the moment of the snapshot.
    pub tick:      u32,
    /// All live entities with their components.
    pub entities:  Vec<EntitySnapshot>,
    /// Relations between entities.
    pub relations: Vec<RelationSnapshot>,
    /// Registered resources (E7, added at v2). `serde(default)` — a v1 JSON
    /// snapshot (without the field) reads as an empty list.
    #[serde(default)]
    pub resources: Vec<ResourceSnapshot>,
}

impl WorldSnapshot {
    /// Current wire-format version. v3 introduced the string table
    /// ([`crate::wire`]); v2 added resources; v1 was the original inline format.
    pub const CURRENT_VERSION: u32 = 3;

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

    /// Serialize the snapshot into JSON bytes (current wire format = v3, interned).
    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(&crate::wire::WireSnapshotV3::from_snapshot(self))
    }

    /// Deserialize the snapshot from JSON bytes.
    ///
    /// The leading `version` is peeked to dispatch: v3+ parses the interned wire
    /// form and resolves the string table; an older (v≤2) file parses directly
    /// into the inline struct. Migration to the current version happens at restore.
    pub fn from_json(data: &[u8]) -> Result<Self, serde_json::Error> {
        if crate::wire::peek_version_json(data)? >= crate::wire::WIRE_VERSION_V3 {
            let wire: crate::wire::WireSnapshotV3 = serde_json::from_slice(data)?;
            wire.into_snapshot().map_err(serde::de::Error::custom)
        } else {
            serde_json::from_slice(data)
        }
    }

    // ── Bincode ──────────────────────────────────────────────────

    /// Serialize the snapshot into the binary format (bincode; current wire
    /// format = v3, interned).
    ///
    /// 5-10x smaller than JSON, 2-3x faster.
    pub fn to_bincode(&self) -> Result<Vec<u8>, Box<bincode::ErrorKind>> {
        bincode::serialize(&crate::wire::WireSnapshotV3::from_snapshot(self))
    }

    /// Deserialize the snapshot from the binary format (bincode).
    ///
    /// The leading `u32` version word dispatches: v3+ parses the interned wire
    /// form, an older (v≤2) file parses directly into the inline struct.
    pub fn from_bincode(data: &[u8]) -> Result<Self, Box<bincode::ErrorKind>> {
        if crate::wire::peek_version_bincode(data) >= crate::wire::WIRE_VERSION_V3 {
            let wire: crate::wire::WireSnapshotV3 = bincode::deserialize(data)?;
            wire.into_snapshot()
                .map_err(|e| Box::new(bincode::ErrorKind::Custom(e)))
        } else {
            bincode::deserialize(data)
        }
    }

    // ── Migration ────────────────────────────────────────────────

    /// Run the migration chain, bringing the snapshot up to the current version.
    ///
    /// A snapshot older than [`Self::CURRENT_VERSION`] is stepped forward one
    /// version at a time; a snapshot at the current version is a no-op; a newer
    /// (unmigratable) version returns an error. This is the single definition of
    /// version compatibility — callers migrate then restore rather than
    /// consulting a separate compatibility predicate.
    pub fn migrate(&mut self) -> Result<(), String> {
        while self.version < Self::CURRENT_VERSION {
            let migrator = migration_for(self.version)
                .ok_or_else(|| format!("no migration found for version {}", self.version))?;
            migrator(self)?;
            self.version += 1;
        }
        Ok(())
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
        // v2 → v3: the string table is a WIRE-only change (see `crate::wire`); the
        // in-memory `WorldSnapshot` is representation-agnostic and unchanged, so
        // there is nothing to migrate on the parsed struct — bump only.
        2 => Some(|_data| Ok(())),
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
    /// A diff is a delta over the same versioned wire structs as a snapshot
    /// (`EntitySnapshot`/`ComponentSnapshot`/`RelationSnapshot`), so its wire
    /// version IS the snapshot version — they bump together. Checked on
    /// [`WorldSerializer::apply_diff_to_snapshot`](crate::WorldSerializer::apply_diff_to_snapshot).
    pub const CURRENT_VERSION: u32 = WorldSnapshot::CURRENT_VERSION;

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
    fn migrate_steps_old_version_up_to_current() {
        // An older version migrates up through the chain to CURRENT.
        let mut snap = WorldSnapshot::new(0);
        snap.version = 1; // pretend v1 (pre-resources)
        snap.migrate().unwrap();
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);
    }

    #[test]
    fn migrate_is_noop_on_future_version() {
        // `migrate` only steps UP toward CURRENT; a future version has nothing to
        // step to, so it is left untouched (no error). Rejecting an unmigratable
        // future version is the RESTORE gate's job, not migrate's — see the
        // `future_version_is_rejected_by_restore` integration test.
        let mut snap = WorldSnapshot::new(0);
        snap.version = WorldSnapshot::CURRENT_VERSION + 1;
        snap.migrate().unwrap();
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION + 1);
    }

    #[test]
    fn world_diff_version_tracks_snapshot_version() {
        // The diff wire version is the snapshot wire version — one source of truth.
        assert_eq!(WorldDiff::CURRENT_VERSION, WorldSnapshot::CURRENT_VERSION);
    }

    // ── v3 string table (wire format) ────────────────────────────

    #[test]
    fn v3_json_interns_repeated_type_names() {
        // 100 entities, all carrying the same component type: the fully-qualified
        // type name must appear in the serialized bytes EXACTLY ONCE (interned in
        // the string table), not 100 times inline.
        let mut snap = WorldSnapshot::new(0);
        for i in 0..100 {
            snap.entities.push(EntitySnapshot {
                original_index: i,
                components: vec![ComponentSnapshot::new_json(
                    "my_crate::components::Position",
                    br#"{"x":1.0,"y":2.0}"#.to_vec(),
                )],
            });
        }
        let json = String::from_utf8(snap.to_json().unwrap()).unwrap();
        let occurrences = json.matches("my_crate::components::Position").count();
        assert_eq!(occurrences, 1, "type name must be interned once, saw {occurrences}");

        // And it round-trips back to the inline in-memory form intact.
        let restored = WorldSnapshot::from_json(json.as_bytes()).unwrap();
        assert_eq!(restored.entities.len(), 100);
        assert_eq!(
            restored.entities[42].components[0].type_name,
            "my_crate::components::Position"
        );
        assert_eq!(restored.version, WorldSnapshot::CURRENT_VERSION);
    }

    #[test]
    fn v3_interning_shrinks_repeated_names_in_bincode() {
        // A snapshot dominated by repeated type names is smaller interned than the
        // pre-v3 inline shape would be (proxy: many identical names → the table
        // stores one copy, records store a 4-byte index).
        let mut snap = WorldSnapshot::new(0);
        for i in 0..200 {
            snap.entities.push(EntitySnapshot {
                original_index: i,
                components: vec![ComponentSnapshot::new_binary(
                    "some_game_crate::gameplay::components::Transform",
                    vec![0u8; 4],
                )],
            });
        }
        // Inline size = serialize the struct directly (the pre-v3 wire shape).
        let inline = bincode::serialize(&snap).unwrap().len();
        let interned = snap.to_bincode().unwrap().len();
        assert!(
            interned < inline,
            "interned bincode ({interned}) must be smaller than inline ({inline})"
        );
    }

    #[test]
    fn reads_legacy_v2_inline_json_fixture() {
        // A v2 file predates the string table: type names are inline and the
        // envelope version is 2. `from_json` must peek version 2, take the legacy
        // parse path, and yield the data intact (backward compatibility — the
        // whole point of the versioned wire format). Migration to current happens
        // at restore, so the parsed version stays 2 here.
        let mut legacy = WorldSnapshot::new(7);
        legacy.version = 2; // pretend it was written by the pre-v3 code
        legacy.entities.push(EntitySnapshot {
            original_index: 3,
            components: vec![ComponentSnapshot::new_json(
                "my_crate::Health",
                br#"{"hp":50.0}"#.to_vec(),
            )],
        });
        // Serialize the struct DIRECTLY (inline) to emulate a real v2 file — this
        // is exactly the shape the old `to_json` produced (no string table).
        let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
        assert!(
            String::from_utf8_lossy(&legacy_bytes).contains(r#""type_name":"my_crate::Health""#),
            "fixture must carry the inline type_name"
        );

        let parsed = WorldSnapshot::from_json(&legacy_bytes).unwrap();
        assert_eq!(parsed.version, 2, "legacy version preserved until restore migrates");
        assert_eq!(parsed.tick, 7);
        assert_eq!(parsed.entities.len(), 1);
        assert_eq!(parsed.entities[0].original_index, 3);
        assert_eq!(parsed.entities[0].components[0].type_name, "my_crate::Health");
    }

    #[test]
    fn reads_legacy_v2_inline_bincode_fixture() {
        // Same, for bincode: the leading u32 version word (2) routes to the legacy
        // positional parse; the interned v3 parse is not attempted.
        let mut legacy = WorldSnapshot::new(1);
        legacy.version = 2;
        legacy.relations.push(RelationSnapshot {
            subject_index: 1,
            target_index:  0,
            kind_name:     "apex_core::relations::ChildOf".to_string(),
        });
        let legacy_bytes = bincode::serialize(&legacy).unwrap();
        let parsed = WorldSnapshot::from_bincode(&legacy_bytes).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.relations.len(), 1);
        assert_eq!(parsed.relations[0].kind_name, "apex_core::relations::ChildOf");
    }

    #[test]
    fn v3_corrupt_string_table_index_is_rejected() {
        // A record pointing past the string table is a corrupt file — restore must
        // never fabricate a name; from_json/from_bincode surface an error.
        let wire = crate::wire::WireSnapshotV3 {
            version: WorldSnapshot::CURRENT_VERSION,
            tick: 0,
            string_table: vec!["only_one".to_string()],
            entities: vec![crate::wire::WireEntity {
                original_index: 0,
                components: vec![crate::wire::WireComponent {
                    name_idx: 5, // out of range
                    data: Vec::new(),
                    format: DataFormat::Json,
                }],
            }],
            relations: vec![],
            resources: vec![],
        };
        let json = serde_json::to_vec(&wire).unwrap();
        assert!(WorldSnapshot::from_json(&json).is_err());
        let bin = bincode::serialize(&wire).unwrap();
        assert!(WorldSnapshot::from_bincode(&bin).is_err());
    }
}
