//! Read-only forms of a world snapshot and a diff before wire version 4.
//!
//! **Frozen:** these structs are the byte layouts that versions 0–3 wrote, copied out of the
//! in-memory types they used to BE — nothing here may change, or an old file stops opening.
//!
//! - v0–v2: the in-memory structs serialized directly (type names inline); v2 added resources.
//! - v3: type names interned into a string table.
//!
//! Both were written as JSON or as `bincode` 1.x. Every resource payload was `bincode`, and so was
//! every `Binary` component payload; they are read as [`DataFormat::LegacyBincode`], which restore
//! decodes through each type's legacy reader (`ComponentInfo::legacy_bincode`,
//! `ResourceSerdeFns::legacy_bincode`).
//!
//! `bincode` is unmaintained (RUSTSEC-2025-0141). This module, those readers and the dependency are
//! removed on **2026-12-17** (engine TD-608): a document last saved before wire version 4 must be
//! opened and saved once before that date.

use serde::Deserialize;
#[cfg(test)]
use serde::Serialize;

use crate::serializer::SerializationError;
use crate::snapshot::{
    ComponentSnapshot, DataFormat, EntitySnapshot, RelationSnapshot, ResourceSnapshot, WorldDiff,
    WorldSnapshot,
};

/// The version that introduced the string table.
const INTERNED_VERSION: u32 = 3;

#[derive(Deserialize, Clone, Copy)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) enum LegacyDataFormat {
    Json,
    Binary,
}

impl LegacyDataFormat {
    fn current(self) -> DataFormat {
        match self {
            LegacyDataFormat::Json => DataFormat::Json,
            LegacyDataFormat::Binary => DataFormat::LegacyBincode,
        }
    }
}

// ── v0–v2: inline ────────────────────────────────────────────────

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InlineComponent {
    pub type_name: String,
    pub data:      Vec<u8>,
    pub format:    LegacyDataFormat,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InlineEntity {
    pub original_index: u32,
    pub components:     Vec<InlineComponent>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InlineRelation {
    pub subject_index: u32,
    pub target_index:  u32,
    pub kind_name:     String,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InlineResource {
    pub type_name: String,
    pub data:      Vec<u8>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InlineSnapshot {
    pub version:   u32,
    pub tick:      u32,
    pub entities:  Vec<InlineEntity>,
    pub relations: Vec<InlineRelation>,
    /// v1 JSON has no such field. (`bincode` is positional and ignores `default`, so a v1
    /// `bincode` file does not parse here — nor did it before v4: this reader keeps what was.)
    #[serde(default)]
    pub resources: Vec<InlineResource>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InlineDiff {
    pub version:             u32,
    pub added_entities:      Vec<InlineEntity>,
    pub removed_entities:    Vec<u32>,
    pub added_components:    Vec<(u32, Vec<InlineComponent>)>,
    pub removed_components:  Vec<(u32, Vec<String>)>,
    pub modified_components: Vec<(u32, Vec<InlineComponent>)>,
    pub added_relations:     Vec<InlineRelation>,
    pub removed_relations:   Vec<InlineRelation>,
}

// ── v3: interned ─────────────────────────────────────────────────

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InternedSnapshot {
    pub version:      u32,
    pub tick:         u32,
    pub string_table: Vec<String>,
    pub entities:     Vec<InternedEntity>,
    pub relations:    Vec<InternedRelation>,
    #[serde(default)]
    pub resources:    Vec<InternedResource>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InternedEntity {
    pub original_index: u32,
    pub components:     Vec<InternedComponent>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InternedComponent {
    pub name_idx: u32,
    pub data:     Vec<u8>,
    pub format:   LegacyDataFormat,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InternedRelation {
    pub subject_index: u32,
    pub target_index:  u32,
    pub kind_name_idx: u32,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct InternedResource {
    pub name_idx: u32,
    pub data:     Vec<u8>,
}

// ── Conversions ──────────────────────────────────────────────────

fn component(c: InlineComponent) -> ComponentSnapshot {
    ComponentSnapshot { type_name: c.type_name, data: c.data, format: c.format.current() }
}

fn entity(e: InlineEntity) -> EntitySnapshot {
    EntitySnapshot {
        original_index: e.original_index,
        components:     e.components.into_iter().map(component).collect(),
    }
}

fn relation(r: InlineRelation) -> RelationSnapshot {
    RelationSnapshot { subject_index: r.subject_index, target_index: r.target_index, kind_name: r.kind_name }
}

fn sorted(mut resources: Vec<ResourceSnapshot>) -> Vec<ResourceSnapshot> {
    resources.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    resources
}

impl InlineSnapshot {
    fn current(self) -> WorldSnapshot {
        WorldSnapshot {
            version:   self.version,
            tick:      self.tick,
            entities:  self.entities.into_iter().map(entity).collect(),
            relations: self.relations.into_iter().map(relation).collect(),
            resources: sorted(
                self.resources
                    .into_iter()
                    .map(|r| ResourceSnapshot {
                        type_name: r.type_name,
                        format:    DataFormat::LegacyBincode,
                        data:      r.data,
                    })
                    .collect(),
            ),
        }
    }
}

impl InternedSnapshot {
    /// The on-disk `version` is preserved (a file that parsed as v3 reports its own), and an
    /// index past the string table is a corrupt file — an error, never a made-up name.
    fn current(self) -> Result<WorldSnapshot, SerializationError> {
        let table = self.string_table;
        let name = |idx: u32| -> Result<String, SerializationError> {
            table.get(idx as usize).cloned().ok_or_else(|| SerializationError::NotADocument {
                reason: format!("string_table index {idx} out of range (table len {})", table.len()),
            })
        };
        let entities = self
            .entities
            .into_iter()
            .map(|e| {
                Ok(EntitySnapshot {
                    original_index: e.original_index,
                    components:     e
                        .components
                        .into_iter()
                        .map(|c| {
                            Ok(ComponentSnapshot { type_name: name(c.name_idx)?, data: c.data, format: c.format.current() })
                        })
                        .collect::<Result<_, SerializationError>>()?,
                })
            })
            .collect::<Result<_, SerializationError>>()?;
        let relations = self
            .relations
            .into_iter()
            .map(|r| {
                Ok(RelationSnapshot {
                    subject_index: r.subject_index,
                    target_index:  r.target_index,
                    kind_name:     name(r.kind_name_idx)?,
                })
            })
            .collect::<Result<_, SerializationError>>()?;
        let resources = self
            .resources
            .into_iter()
            .map(|r| {
                Ok(ResourceSnapshot { type_name: name(r.name_idx)?, format: DataFormat::LegacyBincode, data: r.data })
            })
            .collect::<Result<_, SerializationError>>()?;
        Ok(WorldSnapshot { version: self.version, tick: self.tick, entities, relations, resources: sorted(resources) })
    }
}

// ── Readers ──────────────────────────────────────────────────────

/// A text document of version `version` (< 4), already peeked by the caller.
pub(crate) fn snapshot_from_json(data: &[u8], version: u32) -> Result<WorldSnapshot, SerializationError> {
    if version >= INTERNED_VERSION {
        serde_json::from_slice::<InternedSnapshot>(data)?.current()
    } else {
        Ok(serde_json::from_slice::<InlineSnapshot>(data)?.current())
    }
}

/// A `bincode` snapshot of version 3 or older: its leading word is the version, fixed-width
/// little-endian (`bincode`'s default).
pub(crate) fn snapshot_from_bincode(data: &[u8]) -> Result<WorldSnapshot, SerializationError> {
    let version = match data {
        [a, b, c, d, ..] => u32::from_le_bytes([*a, *b, *c, *d]),
        _ => {
            return Err(SerializationError::NotADocument {
                reason: format!("{} bytes are not a binary world snapshot", data.len()),
            })
        }
    };
    if version > INTERNED_VERSION {
        return Err(SerializationError::NotADocument {
            reason: "neither a current binary snapshot (no `APXW` magic) nor a legacy one (version word above 3)".into(),
        });
    }
    if version == INTERNED_VERSION {
        bincode::deserialize::<InternedSnapshot>(data)?.current()
    } else {
        Ok(bincode::deserialize::<InlineSnapshot>(data)?.current())
    }
}

/// A `bincode` diff of version 3 or older. Its conversion into the in-memory form IS its
/// migration — nothing about a diff changed between v3 and v4 but its bytes, and the payload
/// formats are marked — so it comes back at the current version, applicable to a current base.
pub(crate) fn diff_from_bincode(data: &[u8]) -> Result<WorldDiff, SerializationError> {
    let not_a_diff = || SerializationError::NotADocument {
        reason: "neither a current binary diff (no `APXD` magic) nor a legacy one".into(),
    };
    let d: InlineDiff = bincode::deserialize(data).map_err(|_| not_a_diff())?;
    if d.version > INTERNED_VERSION {
        return Err(not_a_diff());
    }
    let components = |list: Vec<(u32, Vec<InlineComponent>)>| {
        list.into_iter().map(|(idx, comps)| (idx, comps.into_iter().map(component).collect())).collect()
    };
    Ok(WorldDiff {
        version:             WorldDiff::CURRENT_VERSION,
        added_entities:      d.added_entities.into_iter().map(entity).collect(),
        removed_entities:    d.removed_entities,
        added_components:    components(d.added_components),
        removed_components:  d.removed_components,
        modified_components: components(d.modified_components),
        added_relations:     d.added_relations.into_iter().map(relation).collect(),
        removed_relations:   d.removed_relations.into_iter().map(relation).collect(),
        changed_resources:   Vec::new(),
        removed_resources:   Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v3() -> InternedSnapshot {
        InternedSnapshot {
            version:      3,
            tick:         11,
            string_table: vec!["game::Health".into(), "apex_core::relations::ChildOf".into(), "game::Settings".into()],
            entities:     vec![InternedEntity {
                original_index: 4,
                components:     vec![
                    InternedComponent { name_idx: 0, data: br#"{"hp":5.0}"#.to_vec(), format: LegacyDataFormat::Json },
                    InternedComponent { name_idx: 0, data: vec![1, 2, 3, 4], format: LegacyDataFormat::Binary },
                ],
            }],
            relations:    vec![InternedRelation { subject_index: 4, target_index: 1, kind_name_idx: 1 }],
            resources:    vec![InternedResource { name_idx: 2, data: vec![9, 0, 0, 0] }],
        }
    }

    #[test]
    fn a_v3_document_reads_in_both_encodings_with_its_bincode_payloads_marked() {
        for snap in [
            WorldSnapshot::from_json(&serde_json::to_vec(&v3()).unwrap()).unwrap(),
            WorldSnapshot::from_binary(&bincode::serialize(&v3()).unwrap()).unwrap(),
        ] {
            assert_eq!(snap.version, 3, "the version is preserved until migration");
            assert_eq!(snap.tick, 11);
            let comps = &snap.entities[0].components;
            assert_eq!((comps[0].format, comps[0].data.as_slice()), (DataFormat::Json, &br#"{"hp":5.0}"#[..]));
            assert_eq!(comps[1].format, DataFormat::LegacyBincode);
            assert_eq!(snap.relations[0].kind_name, "apex_core::relations::ChildOf");
            assert_eq!(snap.resources[0].format, DataFormat::LegacyBincode, "every v3 resource was bincode");
            assert_eq!(snap.resources[0].type_name, "game::Settings");
        }
    }

    #[test]
    fn a_v2_inline_document_reads_in_both_encodings() {
        let legacy = InlineSnapshot {
            version:   2,
            tick:      7,
            entities:  vec![InlineEntity {
                original_index: 3,
                components:     vec![InlineComponent {
                    type_name: "my_crate::Health".into(),
                    data:      br#"{"hp":50.0}"#.to_vec(),
                    format:    LegacyDataFormat::Json,
                }],
            }],
            relations: vec![InlineRelation { subject_index: 3, target_index: 0, kind_name: "ChildOf".into() }],
            resources: Vec::new(),
        };
        let json = serde_json::to_vec(&legacy).unwrap();
        assert!(String::from_utf8_lossy(&json).contains(r#""type_name":"my_crate::Health""#));
        for snap in [WorldSnapshot::from_json(&json).unwrap(), WorldSnapshot::from_binary(&bincode::serialize(&legacy).unwrap()).unwrap()] {
            assert_eq!(snap.version, 2);
            assert_eq!(snap.entities[0].components[0].type_name, "my_crate::Health");
            assert_eq!(snap.relations[0].kind_name, "ChildOf");
        }
    }

    #[test]
    fn a_v1_json_document_without_resources_reads() {
        let json = br#"{"version":1,"tick":0,"entities":[],"relations":[]}"#;
        let snap = WorldSnapshot::from_json(json).unwrap();
        assert_eq!(snap.version, 1);
        assert!(snap.resources.is_empty());
    }

    #[test]
    fn a_corrupt_v3_string_table_index_is_rejected_in_both_encodings() {
        let mut bad = v3();
        bad.entities[0].components[0].name_idx = 99;
        assert!(WorldSnapshot::from_json(&serde_json::to_vec(&bad).unwrap()).is_err());
        let mut bad = v3();
        bad.entities[0].components[0].name_idx = 99;
        assert!(WorldSnapshot::from_binary(&bincode::serialize(&bad).unwrap()).is_err());
    }

    #[test]
    fn a_v3_bincode_diff_reads_at_the_current_version() {
        let d = InlineDiff {
            version:             3,
            added_entities:      Vec::new(),
            removed_entities:    vec![2],
            added_components:    vec![(1, vec![InlineComponent { type_name: "T".into(), data: vec![5], format: LegacyDataFormat::Binary }])],
            removed_components:  Vec::new(),
            modified_components: Vec::new(),
            added_relations:     Vec::new(),
            removed_relations:   Vec::new(),
        };
        let diff = WorldDiff::from_binary(&bincode::serialize(&d).unwrap()).unwrap();
        assert_eq!(diff.version, WorldDiff::CURRENT_VERSION);
        assert_eq!(diff.removed_entities, vec![2]);
        assert_eq!(diff.added_components[0].1[0].format, DataFormat::LegacyBincode);
    }
}
