//! World snapshot data structures — the in-memory form of a saved world.
//!
//! # Formats
//!
//! These structs carry no byte layout of their own: every byte form lives in [`crate::wire`]
//! (the current version) and [`crate::legacy`] (the read-only forms of older versions), so the
//! in-memory shape can change without breaking a single file on disk.
//!
//! - **Text** ([`WorldSnapshot::to_json`]) — the document a person reads and diffs: a JSON
//!   component or resource is written as its JSON value, named by its type.
//! - **Binary** ([`WorldSnapshot::to_binary`]) — `postcard`, behind a magic word, for compact
//!   saves.
//!
//! A component's bytes are whatever its registered serde fns write ([`DataFormat`]); the
//! container format only decides how those bytes are carried.

use serde::de::DeserializeOwned;

use crate::serializer::SerializationError;

// ── Versioning ───────────────────────────────────────────────────
//
// The wire format has ONE version scheme: the `u32` on the envelope
// ([`WorldSnapshot::version`]) plus the migration chain ([`WorldSnapshot::migrate`]).
// "Compatible" is defined operationally — a snapshot is loadable iff it can be
// migrated up to [`WorldSnapshot::CURRENT_VERSION`] (older versions migrate;
// the current version loads as-is; a newer version is rejected because no
// forward migrator exists). [`WorldDiff`] shares this same wire version — it is a
// delta over the same records, so its version tracks
// [`WorldSnapshot::CURRENT_VERSION`] and is checked on apply.

// ── Payload format ───────────────────────────────────────────────

/// What a component's or resource's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFormat {
    /// JSON text — written into a text document as a readable value.
    Json,
    /// Bytes of a binary registration (`postcard` for the core's own fns).
    Binary,
    /// Bytes in the retired `bincode` encoding, read from a document of wire version 3 or older.
    /// Restore decodes them through the type's legacy reader; writing them into a new document is
    /// refused, because no reader of the new format could decode them. Removed with the
    /// dependency (engine TD-608, 2026-12-17).
    LegacyBincode,
}

// ── WorldSnapshot ────────────────────────────────────────────────

/// A serialized resource (E7).
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSnapshot {
    pub type_name: String,
    pub format:    DataFormat,
    pub data:      Vec<u8>,
}

impl ResourceSnapshot {
    /// The resource's value, decoded by its format — for a reader that needs a resource out of a
    /// document WITHOUT restoring it into a world (a pre-pass that must run before the restore).
    /// Every such reader asks here rather than calling a codec itself: a second decoder of the
    /// same bytes is the one that breaks silently when the format changes.
    pub fn decode<R: DeserializeOwned>(&self) -> Result<R, String> {
        match self.format {
            DataFormat::Json => serde_json::from_slice(&self.data).map_err(|e| e.to_string()),
            DataFormat::Binary => apex_core::binary::from_bytes(&self.data).map_err(|e| e.to_string()),
            DataFormat::LegacyBincode => bincode::deserialize(&self.data)
                .map_err(|e| format!("legacy bincode payload: {e}")),
        }
    }
}

/// A full world snapshot — everything needed to restore state.
///
/// This is the **in-memory** shape: type names are inline. It is representation-agnostic — the
/// byte form is chosen by the (de)serialization methods below.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldSnapshot {
    /// Snapshot format version — the single wire-version scheme (see `migrate`).
    pub version:   u32,
    /// World tick at the moment of the snapshot.
    pub tick:      u32,
    /// All live entities with their components.
    pub entities:  Vec<EntitySnapshot>,
    /// Relations between entities.
    pub relations: Vec<RelationSnapshot>,
    /// Registered resources (E7, added at v2), sorted by type name.
    pub resources: Vec<ResourceSnapshot>,
}

impl WorldSnapshot {
    /// Current wire-format version. v4: the text document holds values named by type, the binary
    /// form is `postcard` behind a magic word; v3 introduced the string table; v2 added
    /// resources; v1 was the original inline format.
    pub const CURRENT_VERSION: u32 = crate::wire::WIRE_VERSION;

    pub fn new(tick: u32) -> Self {
        Self {
            version:   Self::CURRENT_VERSION,
            tick,
            entities:  Vec::new(),
            relations: Vec::new(),
            resources: Vec::new(),
        }
    }

    // ── Text ─────────────────────────────────────────────────────

    /// Serialize the snapshot into the text document (JSON, current wire version).
    ///
    /// A payload still in the retired encoding ([`DataFormat::LegacyBincode`]) is refused: restore
    /// the document into a world and snapshot it again.
    pub fn to_json(&self) -> Result<Vec<u8>, SerializationError> {
        crate::wire::snapshot_to_text(self)
    }

    /// Deserialize the snapshot from a text document of any supported version.
    ///
    /// The leading `version` dispatches: the current version parses the v4 document, an older one
    /// goes through the legacy reader, a newer one is [`SerializationError::VersionMismatch`].
    /// Migration to the current version happens at restore (or `read_from_file`).
    pub fn from_json(data: &[u8]) -> Result<Self, SerializationError> {
        crate::wire::snapshot_from_text(data)
    }

    // ── Binary ───────────────────────────────────────────────────

    /// Serialize the snapshot into the binary form: the magic word `APXW`, then `postcard`
    /// (version first). Same refusal of legacy payloads as [`to_json`](Self::to_json).
    pub fn to_binary(&self) -> Result<Vec<u8>, SerializationError> {
        crate::wire::snapshot_to_binary(self)
    }

    /// Deserialize the snapshot from binary bytes: the current form behind the magic word, or a
    /// `bincode` snapshot of version 3 or older.
    pub fn from_binary(data: &[u8]) -> Result<Self, SerializationError> {
        crate::wire::snapshot_from_binary(data)
    }

    // ── Migration ────────────────────────────────────────────────

    /// Run the migration chain, bringing the snapshot up to the current version.
    ///
    /// A snapshot older than [`Self::CURRENT_VERSION`] is stepped forward one
    /// version at a time; a snapshot at the current version is a no-op; a newer
    /// version is left untouched (restore rejects it). This is the single definition of
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
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySnapshot {
    /// Original entity index — for remapping on restore.
    pub original_index: u32,
    /// Serialized components of the entity.
    pub components: Vec<ComponentSnapshot>,
}

// ── ComponentSnapshot ────────────────────────────────────────────

/// A snapshot of a single component: raw bytes in the format named by `format`.
#[derive(Debug, Clone, PartialEq)]
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
    ///
    /// JSON `null` is stored as NO bytes — bare presence. Restore hands empty JSON bytes to the
    /// type as `null` (core ADR-015), so the two are one value; keeping one spelling makes the
    /// component compare equal to itself after a trip through a text document, which writes
    /// presence as `null` and reads it back as presence.
    pub fn new_json(type_name: impl Into<String>, json_bytes: Vec<u8>) -> Self {
        let data = if json_bytes == b"null" { Vec::new() } else { json_bytes };
        Self {
            type_name: type_name.into(),
            data,
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
#[derive(Debug, Clone, PartialEq)]
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
        // v1 → v2 (E7): `resources` was added; for v1 it reads as empty.
        1 => Some(|_data| Ok(())),
        // v2 → v3: the string table is a WIRE-only change; the in-memory snapshot is unchanged.
        2 => Some(|_data| Ok(())),
        // v3 → v4: text values and `postcard` are WIRE-only changes. What changed for payloads —
        // `bincode` bytes — the legacy reader already marked as `DataFormat::LegacyBincode`, and
        // restore decodes them through the type's legacy reader; nothing to rewrite here.
        3 => Some(|_data| Ok(())),
        _ => None,
    }
}

// ── WorldDiff (incremental changes) ──────────────────────────────

/// The difference between two snapshots, for incremental saving.
///
/// # Byte-level delta (3.1)
/// Components present in both snapshots with the same `type_name` are compared
/// byte-by-byte. If the data matches — the component is not included in the diff.
/// If it differs — the component goes into `modified_components`. Resources are compared the
/// same way.
#[derive(Debug, Clone, PartialEq)]
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
    /// Resources that appeared or whose bytes changed (v4). Before v4 a diff carried no
    /// resources, and an incremental save silently kept the OLD value of every changed one.
    pub changed_resources: Vec<ResourceSnapshot>,
    /// Resources that are gone (type names, v4).
    pub removed_resources: Vec<String>,
}

impl WorldDiff {
    /// A diff is a delta over the same records as a snapshot, so its wire version IS the
    /// snapshot version — they bump together. Checked on
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
            changed_resources: Vec::new(),
            removed_resources: Vec::new(),
        }
    }

    /// Serialize the diff into the binary form: the magic word `APXD`, then `postcard`.
    pub fn to_binary(&self) -> Result<Vec<u8>, SerializationError> {
        crate::wire::diff_to_binary(self)
    }

    /// Deserialize a diff: the current binary form, or a `bincode` diff of version 3 or older.
    pub fn from_binary(data: &[u8]) -> Result<Self, SerializationError> {
        crate::wire::diff_from_binary(data)
    }

    pub fn is_empty(&self) -> bool {
        self.added_entities.is_empty()
            && self.removed_entities.is_empty()
            && self.added_components.is_empty()
            && self.removed_components.is_empty()
            && self.modified_components.is_empty()
            && self.added_relations.is_empty()
            && self.removed_relations.is_empty()
            && self.changed_resources.is_empty()
            && self.removed_resources.is_empty()
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
    /// The text document (`.json`).
    Json,
    /// The binary form (`.bin`).
    Binary,
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_diff_empty() {
        let diff = WorldDiff::new();
        assert!(diff.is_empty());
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
    fn json_null_is_bare_presence() {
        assert!(ComponentSnapshot::new_json("Marker", b"null".to_vec()).data.is_empty());
        assert_eq!(ComponentSnapshot::new_json("Opt", b"0".to_vec()).data, b"0");
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

    #[test]
    fn a_resource_decodes_by_its_own_format() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Config {
            level: u32,
            name: String,
        }
        let value = Config { level: 3, name: "a".into() };
        let json = ResourceSnapshot {
            type_name: "Config".into(),
            format: DataFormat::Json,
            data: serde_json::to_vec(&value).unwrap(),
        };
        let binary = ResourceSnapshot {
            type_name: "Config".into(),
            format: DataFormat::Binary,
            data: apex_core::binary::to_vec(&value).unwrap(),
        };
        let legacy = ResourceSnapshot {
            type_name: "Config".into(),
            format: DataFormat::LegacyBincode,
            data: bincode::serialize(&value).unwrap(),
        };
        assert_eq!(json.decode::<Config>().unwrap(), value);
        assert_eq!(binary.decode::<Config>().unwrap(), value);
        assert_eq!(legacy.decode::<Config>().unwrap(), value);
        // The same bytes read as another format are an error, not a value.
        let wrong = ResourceSnapshot { format: DataFormat::Json, ..binary };
        assert!(wrong.decode::<Config>().is_err());
    }
}
