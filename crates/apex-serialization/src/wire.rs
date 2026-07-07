//! Versioned on-disk wire format for [`WorldSnapshot`](crate::WorldSnapshot).
//!
//! # String table (format v3)
//! The in-memory `WorldSnapshot` stores component/relation/resource type names
//! **inline** — the same fully-qualified Rust path (`"my_crate::Position"`,
//! ~30-50 bytes) is repeated once per instance. In a scene of N entities that
//! each carry the same K component types, that is K distinct strings written N
//! times. The v3 wire format **interns** those names into a `string_table`:
//! each distinct name is stored once and every instance holds a `u32` index.
//!
//! This is a pure **wire concern**. `WorldSnapshot`, the serializer, the diff
//! engine — none of them see the interned form. Interning happens only at the
//! byte boundary (`WorldSnapshot::{to_json,from_json,to_bincode,from_bincode}`):
//! write converts inline → interned, read converts interned → inline. Older
//! (v≤2) files carry inline names and parse directly into `WorldSnapshot`
//! (which is representation-agnostic), then migrate; the version-peek at the
//! read boundary dispatches by the leading `version` field (a `u32`, first in
//! both the legacy and v3 layouts, readable in JSON by key and in bincode as
//! the leading little-endian word).
//!
//! Bincode is positional, so this is a genuine format-version bump (v2 bytes are
//! not v3 bytes) — handled by the version dispatch, not by `serde(default)`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::snapshot::{
    ComponentSnapshot, DataFormat, EntitySnapshot, RelationSnapshot, ResourceSnapshot,
    WorldSnapshot,
};

/// The wire-format version that introduced the string table.
pub(crate) const WIRE_VERSION_V3: u32 = 3;

// ── v3 wire structs (interned) ───────────────────────────────────

#[derive(Serialize, Deserialize)]
pub(crate) struct WireSnapshotV3 {
    /// First field — the version-peek reads this to dispatch the format.
    pub version:      u32,
    pub tick:         u32,
    /// Interned type/kind names; wire records index into this.
    pub string_table: Vec<String>,
    pub entities:     Vec<WireEntity>,
    pub relations:    Vec<WireRelation>,
    #[serde(default)]
    pub resources:    Vec<WireResource>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WireEntity {
    pub original_index: u32,
    pub components:     Vec<WireComponent>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WireComponent {
    /// Index into [`WireSnapshotV3::string_table`] for the component type name.
    pub name_idx: u32,
    pub data:     Vec<u8>,
    pub format:   DataFormat,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WireRelation {
    pub subject_index: u32,
    pub target_index:  u32,
    /// Index into [`WireSnapshotV3::string_table`] for the relation kind name.
    pub kind_name_idx: u32,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WireResource {
    /// Index into [`WireSnapshotV3::string_table`] for the resource type name.
    pub name_idx: u32,
    pub data:     Vec<u8>,
}

// ── Interner (build side) ────────────────────────────────────────

/// Dedupes strings into a dense `Vec<String>` + index map. Insertion order is
/// deterministic (first-seen), so the produced table — and thus the serialized
/// bytes — are stable for a given snapshot.
struct Interner {
    table: Vec<String>,
    map:   HashMap<String, u32>,
}

impl Interner {
    fn new() -> Self {
        Self { table: Vec::new(), map: HashMap::new() }
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&idx) = self.map.get(s) {
            return idx;
        }
        let idx = self.table.len() as u32;
        self.table.push(s.to_string());
        self.map.insert(s.to_string(), idx);
        idx
    }
}

// ── Conversions ──────────────────────────────────────────────────

impl WireSnapshotV3 {
    /// Intern an in-memory snapshot into the v3 wire form.
    pub(crate) fn from_snapshot(snap: &WorldSnapshot) -> Self {
        let mut interner = Interner::new();

        let entities = snap
            .entities
            .iter()
            .map(|e| WireEntity {
                original_index: e.original_index,
                components: e
                    .components
                    .iter()
                    .map(|c| WireComponent {
                        name_idx: interner.intern(&c.type_name),
                        data:     c.data.clone(),
                        format:   c.format,
                    })
                    .collect(),
            })
            .collect();

        let relations = snap
            .relations
            .iter()
            .map(|r| WireRelation {
                subject_index: r.subject_index,
                target_index:  r.target_index,
                kind_name_idx: interner.intern(&r.kind_name),
            })
            .collect();

        let resources = snap
            .resources
            .iter()
            .map(|r| WireResource {
                name_idx: interner.intern(&r.type_name),
                data:     r.data.clone(),
            })
            .collect();

        Self {
            version: WIRE_VERSION_V3,
            tick: snap.tick,
            string_table: interner.table,
            entities,
            relations,
            resources,
        }
    }

    /// Resolve the string table back into an inline in-memory snapshot.
    ///
    /// The on-disk `version` is preserved (not forced to current): a genuine v3
    /// file yields version 3 (loads directly); a future version that happened to
    /// parse as v3 yields its own version, so restore's gate rejects it as
    /// unmigratable rather than silently accepting a lossy parse. An out-of-range
    /// index is a corrupt table — surfaced as an error, never a bogus name.
    pub(crate) fn into_snapshot(self) -> Result<WorldSnapshot, String> {
        let WireSnapshotV3 { version, tick, string_table, entities, relations, resources } = self;

        let name = |idx: u32| -> Result<String, String> {
            string_table
                .get(idx as usize)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "string_table index {idx} out of range (table len {})",
                        string_table.len()
                    )
                })
        };

        let entities = entities
            .into_iter()
            .map(|e| {
                let components = e
                    .components
                    .into_iter()
                    .map(|c| {
                        Ok(ComponentSnapshot {
                            type_name: name(c.name_idx)?,
                            data:      c.data,
                            format:    c.format,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(EntitySnapshot { original_index: e.original_index, components })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let relations = relations
            .into_iter()
            .map(|r| {
                Ok(RelationSnapshot {
                    subject_index: r.subject_index,
                    target_index:  r.target_index,
                    kind_name:     name(r.kind_name_idx)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let resources = resources
            .into_iter()
            .map(|r| {
                Ok(ResourceSnapshot { type_name: name(r.name_idx)?, data: r.data })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(WorldSnapshot { version, tick, entities, relations, resources })
    }
}

// ── Version peek (read dispatch) ─────────────────────────────────

/// Peek the leading `version` of a JSON snapshot without parsing the body.
/// A file with no `version` field (older than versioning ever existed) reads as 0.
pub(crate) fn peek_version_json(data: &[u8]) -> Result<u32, serde_json::Error> {
    #[derive(Deserialize)]
    struct VersionPeek {
        #[serde(default)]
        version: u32,
    }
    let peek: VersionPeek = serde_json::from_slice(data)?;
    Ok(peek.version)
}

/// Peek the leading `version` of a bincode snapshot: the first field is a `u32`
/// in fixed-width little-endian (bincode's default), so it is the leading word.
/// Too-short input reads as 0 and falls through to the legacy parse (which errors).
pub(crate) fn peek_version_bincode(data: &[u8]) -> u32 {
    if data.len() >= 4 {
        u32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        0
    }
}
