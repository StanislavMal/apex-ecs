//! The current byte forms of a world snapshot and a diff (wire version 4), and the read dispatch
//! over every supported version.
//!
//! # Text document
//! The document a person reads, diffs and, when needed, edits by hand:
//!
//! ```json
//! {
//!   "format": "apex-world",
//!   "version": 4,
//!   "tick": 17,
//!   "entities": [
//!     { "id": 3, "components": { "game::Name": "Door", "game::Health": {"hp":50.0}, "game::Frozen": null } }
//!   ],
//!   "relations": [ { "subject": 3, "target": 1, "kind": "apex_core::relations::ChildOf" } ],
//!   "resources": { "game::Settings": {"volume":0.5} }
//! }
//! ```
//!
//! A JSON component or resource is written as its JSON value, **byte for byte** as its serde fns
//! produced it (`RawValue`): a load followed by a save does not re-print a single float. Bare
//! presence (no bytes) is `null`. A payload of a binary registration has no JSON spelling and goes
//! into `binary_components` / `binary_resources` as bytes. Unknown fields and a type named twice on
//! one entity are errors, not silently dropped data.
//!
//! Up to version 3 the document stored every payload as an array of byte values (a 640 KB scene
//! of which a person could read nothing), and resources as `bincode` bytes.
//!
//! # Binary form
//! The magic word (`APXW` for a snapshot, `APXD` for a diff), then `postcard` 1.x, whose wire
//! format is stable across 1.x by its specification. The first `postcard` field is the version, so
//! a reader knows the layout before it parses the body. Type names are interned into a string
//! table: a scene repeats the same few dozen names thousands of times. Without the magic word a
//! `postcard` version (a variable-length integer) could not be told from the fixed-width leading
//! word of a `bincode` file of version 3 or older.
//!
//! The in-memory structs (`crate::snapshot`) never meet these layouts except here.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::serializer::SerializationError;
use crate::snapshot::{
    ComponentSnapshot, DataFormat, EntitySnapshot, RelationSnapshot, ResourceSnapshot, WorldDiff,
    WorldSnapshot,
};

/// The current wire version of both forms.
pub(crate) const WIRE_VERSION: u32 = 4;
/// The `format` field of a text document.
pub(crate) const TEXT_FORMAT_TAG: &str = "apex-world";
/// The leading word of a binary snapshot.
pub(crate) const SNAPSHOT_MAGIC: [u8; 4] = *b"APXW";
/// The leading word of a binary diff.
pub(crate) const DIFF_MAGIC: [u8; 4] = *b"APXD";

// ── Named map: `{ "type": value, ... }` in document order ────────

/// A JSON object whose keys are type names, kept in the order the snapshot holds them (so a save
/// is deterministic and a diff of two saves shows only what changed). A name repeated in one
/// object is an error: two values for one component cannot both be true.
#[derive(Debug)]
pub(crate) struct Named<T>(pub Vec<(String, T)>);

impl<T> Default for Named<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Named<T> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: Serialize> Serialize for Named<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Named<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NamedVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for NamedVisitor<T> {
            type Value = Named<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of values named by their type")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                let mut seen = HashSet::new();
                while let Some((name, value)) = map.next_entry::<String, T>()? {
                    if !seen.insert(name.clone()) {
                        return Err(de::Error::custom(format!("`{name}` is named twice")));
                    }
                    out.push((name, value));
                }
                Ok(Named(out))
            }
        }
        deserializer.deserialize_map(NamedVisitor(PhantomData))
    }
}

// ── Text document (v4) ───────────────────────────────────────────

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextSnapshot {
    format:   String,
    version:  u32,
    tick:     u32,
    #[serde(default)]
    entities: Vec<TextEntity>,
    #[serde(default)]
    relations: Vec<TextRelation>,
    #[serde(default)]
    resources: Named<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "Named::is_empty")]
    binary_resources: Named<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextEntity {
    id: u32,
    #[serde(default)]
    components: Named<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "Named::is_empty")]
    binary_components: Named<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextRelation {
    subject: u32,
    target:  u32,
    kind:    String,
}

fn legacy_payload(what: &'static str, type_name: &str) -> SerializationError {
    SerializationError::LegacyPayload { what, type_name: type_name.to_string() }
}

/// A JSON payload as a raw value: its bytes verbatim, empty bytes as `null`.
fn raw_json(what: &'static str, type_name: &str, data: &[u8]) -> Result<Box<RawValue>, SerializationError> {
    let text = if data.is_empty() {
        "null".to_string()
    } else {
        String::from_utf8(data.to_vec()).map_err(|e| SerializationError::Encode {
            what,
            type_name: type_name.to_string(),
            reason:    format!("JSON payload is not UTF-8: {e}"),
        })?
    };
    RawValue::from_string(text).map_err(|e| SerializationError::Encode {
        what,
        type_name: type_name.to_string(),
        reason:    format!("JSON payload is not a JSON value: {e}"),
    })
}

pub(crate) fn snapshot_to_text(snap: &WorldSnapshot) -> Result<Vec<u8>, SerializationError> {
    let mut entities = Vec::with_capacity(snap.entities.len());
    for e in &snap.entities {
        let mut components = Vec::new();
        let mut binary_components = Vec::new();
        for c in &e.components {
            match c.format {
                DataFormat::Json => {
                    components.push((c.type_name.clone(), raw_json("component", &c.type_name, &c.data)?))
                }
                DataFormat::Binary => binary_components.push((c.type_name.clone(), c.data.clone())),
                DataFormat::LegacyBincode => return Err(legacy_payload("component", &c.type_name)),
            }
        }
        entities.push(TextEntity {
            id: e.original_index,
            components: Named(components),
            binary_components: Named(binary_components),
        });
    }
    let mut resources = Vec::new();
    let mut binary_resources = Vec::new();
    for r in &snap.resources {
        match r.format {
            DataFormat::Json => resources.push((r.type_name.clone(), raw_json("resource", &r.type_name, &r.data)?)),
            DataFormat::Binary => binary_resources.push((r.type_name.clone(), r.data.clone())),
            DataFormat::LegacyBincode => return Err(legacy_payload("resource", &r.type_name)),
        }
    }
    let doc = TextSnapshot {
        format: TEXT_FORMAT_TAG.to_string(),
        version: WIRE_VERSION,
        tick: snap.tick,
        entities,
        relations: snap
            .relations
            .iter()
            .map(|r| TextRelation { subject: r.subject_index, target: r.target_index, kind: r.kind_name.clone() })
            .collect(),
        resources: Named(resources),
        binary_resources: Named(binary_resources),
    };
    Ok(serde_json::to_vec_pretty(&doc)?)
}

fn text_v4_into_snapshot(data: &[u8]) -> Result<WorldSnapshot, SerializationError> {
    let doc: TextSnapshot = serde_json::from_slice(data)?;
    if doc.format != TEXT_FORMAT_TAG {
        return Err(SerializationError::NotADocument {
            reason: format!("`format` is `{}`, expected `{TEXT_FORMAT_TAG}`", doc.format),
        });
    }
    let entities = doc
        .entities
        .into_iter()
        .map(|e| {
            let mut components: Vec<ComponentSnapshot> = e
                .components
                .0
                .into_iter()
                .map(|(name, raw)| ComponentSnapshot::new_json(name, raw.get().as_bytes().to_vec()))
                .collect();
            components.extend(
                e.binary_components.0.into_iter().map(|(name, bytes)| ComponentSnapshot::new_binary(name, bytes)),
            );
            EntitySnapshot { original_index: e.id, components }
        })
        .collect();
    let relations = doc
        .relations
        .into_iter()
        .map(|r| RelationSnapshot { subject_index: r.subject, target_index: r.target, kind_name: r.kind })
        .collect();
    let mut resources: Vec<ResourceSnapshot> = doc
        .resources
        .0
        .into_iter()
        .map(|(type_name, raw)| ResourceSnapshot {
            type_name,
            format: DataFormat::Json,
            data: raw.get().as_bytes().to_vec(),
        })
        .collect();
    resources.extend(doc.binary_resources.0.into_iter().map(|(type_name, data)| ResourceSnapshot {
        type_name,
        format: DataFormat::Binary,
        data,
    }));
    resources.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    Ok(WorldSnapshot { version: doc.version, tick: doc.tick, entities, relations, resources })
}

/// Read a text document of any supported version.
pub(crate) fn snapshot_from_text(data: &[u8]) -> Result<WorldSnapshot, SerializationError> {
    #[derive(Deserialize)]
    struct VersionPeek {
        #[serde(default)]
        version: u32,
    }
    let version = serde_json::from_slice::<VersionPeek>(data)?.version;
    if version > WIRE_VERSION {
        return Err(SerializationError::VersionMismatch { expected: WIRE_VERSION, found: version });
    }
    if version == WIRE_VERSION {
        text_v4_into_snapshot(data)
    } else {
        crate::legacy::snapshot_from_json(data, version)
    }
}

// ── Binary form (v4) ─────────────────────────────────────────────
//
// The layout structs BORROW what they carry: writing builds no copy of a payload or a type name
// (with owned layout structs a 2 000-component diff spent more time copying into the layout than
// encoding it — TD-608 stand), and reading borrows payload bytes out of the input until the
// in-memory record takes its one copy.

#[derive(Serialize, Deserialize, Clone, Copy)]
enum WireFormat {
    Json,
    Binary,
}

/// A payload as ONE block of bytes (`serialize_bytes`), not a sequence of `u8` elements: the same
/// bytes on the wire (a length, then the bytes) without a call per byte on either side — 2.2× faster
/// to write and 1.6× to read on 20 000 payloads of 60 bytes (TD-608 stand).
mod block {
    use std::borrow::Cow;

    use serde::{Deserializer, Serializer};

    // `serialize_with` hands the field by reference; the field is a `Cow`.
    #[allow(clippy::ptr_arg)]
    pub fn serialize<S: Serializer>(value: &Cow<'_, [u8]>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Cow<'de, [u8]>, D::Error> {
        struct BlockVisitor;
        impl<'de> serde::de::Visitor<'de> for BlockVisitor {
            type Value = Cow<'de, [u8]>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a block of bytes")
            }
            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E> {
                Ok(Cow::Borrowed(v))
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(Cow::Owned(v.to_vec()))
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(Cow::Owned(v))
            }
        }
        deserializer.deserialize_bytes(BlockVisitor)
    }
}

#[derive(Serialize, Deserialize)]
struct BinaryPayload<'a> {
    /// Index into the string table.
    name:   u32,
    format: WireFormat,
    #[serde(borrow, with = "block")]
    data:   Cow<'a, [u8]>,
}

#[derive(Serialize, Deserialize)]
struct BinaryEntity<'a> {
    index:      u32,
    #[serde(borrow)]
    components: Vec<BinaryPayload<'a>>,
}

#[derive(Serialize, Deserialize)]
struct BinaryRelation {
    subject: u32,
    target:  u32,
    /// Index into the string table.
    kind:    u32,
}

#[derive(Serialize, Deserialize)]
struct BinarySnapshot<'a> {
    /// First field: read before the body so the layout is known.
    version:      u32,
    tick:         u32,
    #[serde(borrow)]
    string_table: Vec<Cow<'a, str>>,
    #[serde(borrow)]
    entities:     Vec<BinaryEntity<'a>>,
    relations:    Vec<BinaryRelation>,
    #[serde(borrow)]
    resources:    Vec<BinaryPayload<'a>>,
}

#[derive(Serialize, Deserialize)]
struct BinaryDiff<'a> {
    version:             u32,
    #[serde(borrow)]
    string_table:        Vec<Cow<'a, str>>,
    #[serde(borrow)]
    added_entities:      Vec<BinaryEntity<'a>>,
    removed_entities:    Vec<u32>,
    #[serde(borrow)]
    added_components:    Vec<(u32, Vec<BinaryPayload<'a>>)>,
    removed_components:  Vec<(u32, Vec<u32>)>,
    #[serde(borrow)]
    modified_components: Vec<(u32, Vec<BinaryPayload<'a>>)>,
    added_relations:     Vec<BinaryRelation>,
    removed_relations:   Vec<BinaryRelation>,
    #[serde(borrow)]
    changed_resources:   Vec<BinaryPayload<'a>>,
    removed_resources:   Vec<u32>,
}

/// Dedupes names into a dense table. Insertion order is first-seen, so the table — and the bytes —
/// are stable for a given snapshot.
#[derive(Default)]
struct Interner<'a> {
    table: Vec<Cow<'a, str>>,
    map:   rustc_hash::FxHashMap<&'a str, u32>,
}

impl<'a> Interner<'a> {
    fn intern(&mut self, s: &'a str) -> u32 {
        if let Some(&idx) = self.map.get(s) {
            return idx;
        }
        let idx = self.table.len() as u32;
        self.table.push(Cow::Borrowed(s));
        self.map.insert(s, idx);
        idx
    }

    fn payload(
        &mut self,
        what: &'static str,
        name: &'a str,
        format: DataFormat,
        data: &'a [u8],
    ) -> Result<BinaryPayload<'a>, SerializationError> {
        let format = match format {
            DataFormat::Json => WireFormat::Json,
            DataFormat::Binary => WireFormat::Binary,
            DataFormat::LegacyBincode => return Err(legacy_payload(what, name)),
        };
        Ok(BinaryPayload { name: self.intern(name), format, data: Cow::Borrowed(data) })
    }

    fn components(&mut self, comps: &'a [ComponentSnapshot]) -> Result<Vec<BinaryPayload<'a>>, SerializationError> {
        comps.iter().map(|c| self.payload("component", &c.type_name, c.format, &c.data)).collect()
    }

    fn resources(&mut self, res: &'a [ResourceSnapshot]) -> Result<Vec<BinaryPayload<'a>>, SerializationError> {
        res.iter().map(|r| self.payload("resource", &r.type_name, r.format, &r.data)).collect()
    }

    fn entity(&mut self, e: &'a EntitySnapshot) -> Result<BinaryEntity<'a>, SerializationError> {
        Ok(BinaryEntity { index: e.original_index, components: self.components(&e.components)? })
    }

    fn relation(&mut self, r: &'a RelationSnapshot) -> BinaryRelation {
        BinaryRelation { subject: r.subject_index, target: r.target_index, kind: self.intern(&r.kind_name) }
    }
}

/// Resolves string-table indices; an index past the table is a corrupt file, never a made-up name.
struct Names<'a>(Vec<Cow<'a, str>>);

impl Names<'_> {
    fn get(&self, idx: u32) -> Result<String, SerializationError> {
        self.0.get(idx as usize).map(|s| s.to_string()).ok_or_else(|| SerializationError::NotADocument {
            reason: format!("string table index {idx} out of range (table len {})", self.0.len()),
        })
    }

    fn component(&self, p: BinaryPayload) -> Result<ComponentSnapshot, SerializationError> {
        let name = self.get(p.name)?;
        Ok(match p.format {
            WireFormat::Json => ComponentSnapshot::new_json(name, p.data.into_owned()),
            WireFormat::Binary => ComponentSnapshot::new_binary(name, p.data.into_owned()),
        })
    }

    fn components(&self, list: Vec<BinaryPayload>) -> Result<Vec<ComponentSnapshot>, SerializationError> {
        list.into_iter().map(|c| self.component(c)).collect()
    }

    fn resource(&self, p: BinaryPayload) -> Result<ResourceSnapshot, SerializationError> {
        Ok(ResourceSnapshot {
            type_name: self.get(p.name)?,
            format:    match p.format {
                WireFormat::Json => DataFormat::Json,
                WireFormat::Binary => DataFormat::Binary,
            },
            data:      p.data.into_owned(),
        })
    }

    fn entity(&self, e: BinaryEntity) -> Result<EntitySnapshot, SerializationError> {
        Ok(EntitySnapshot { original_index: e.index, components: self.components(e.components)? })
    }

    fn relation(&self, r: BinaryRelation) -> Result<RelationSnapshot, SerializationError> {
        Ok(RelationSnapshot { subject_index: r.subject, target_index: r.target, kind_name: self.get(r.kind)? })
    }
}

/// The magic word, then the body: a `[u8; 4]` is four raw bytes in `postcard`, so the pair is
/// written in one sized pass.
fn with_magic(magic: [u8; 4], body: &impl Serialize) -> Result<Vec<u8>, SerializationError> {
    Ok(apex_core::binary::to_vec(&(magic, body))?)
}

pub(crate) fn snapshot_to_binary(snap: &WorldSnapshot) -> Result<Vec<u8>, SerializationError> {
    let mut names = Interner::default();
    let entities = snap.entities.iter().map(|e| names.entity(e)).collect::<Result<_, _>>()?;
    let relations = snap.relations.iter().map(|r| names.relation(r)).collect();
    let resources = names.resources(&snap.resources)?;
    let body = BinarySnapshot {
        version: WIRE_VERSION,
        tick: snap.tick,
        string_table: names.table,
        entities,
        relations,
        resources,
    };
    with_magic(SNAPSHOT_MAGIC, &body)
}

/// The version behind a magic word, checked before the body is parsed: a newer layout is a
/// version mismatch, not a garbled parse.
fn binary_version(body: &[u8]) -> Result<(), SerializationError> {
    let (version, _) = apex_core::binary::take_from_bytes::<u32>(body)?;
    if version != WIRE_VERSION {
        return Err(SerializationError::VersionMismatch { expected: WIRE_VERSION, found: version });
    }
    Ok(())
}

pub(crate) fn snapshot_from_binary(data: &[u8]) -> Result<WorldSnapshot, SerializationError> {
    let Some(body) = data.strip_prefix(&SNAPSHOT_MAGIC) else {
        return crate::legacy::snapshot_from_bincode(data);
    };
    binary_version(body)?;
    let doc: BinarySnapshot = apex_core::binary::from_bytes(body)?;
    let names = Names(doc.string_table);
    let entities = doc.entities.into_iter().map(|e| names.entity(e)).collect::<Result<_, _>>()?;
    let relations = doc.relations.into_iter().map(|r| names.relation(r)).collect::<Result<_, _>>()?;
    let resources = doc.resources.into_iter().map(|r| names.resource(r)).collect::<Result<_, _>>()?;
    Ok(WorldSnapshot { version: doc.version, tick: doc.tick, entities, relations, resources })
}

pub(crate) fn diff_to_binary(diff: &WorldDiff) -> Result<Vec<u8>, SerializationError> {
    let mut names = Interner::default();
    let added_entities = diff.added_entities.iter().map(|e| names.entity(e)).collect::<Result<_, _>>()?;
    let added_components = diff
        .added_components
        .iter()
        .map(|(idx, comps)| Ok((*idx, names.components(comps)?)))
        .collect::<Result<_, SerializationError>>()?;
    let removed_components = diff
        .removed_components
        .iter()
        .map(|(idx, types)| (*idx, types.iter().map(|t| names.intern(t)).collect()))
        .collect();
    let modified_components = diff
        .modified_components
        .iter()
        .map(|(idx, comps)| Ok((*idx, names.components(comps)?)))
        .collect::<Result<_, SerializationError>>()?;
    let added_relations = diff.added_relations.iter().map(|r| names.relation(r)).collect();
    let removed_relations = diff.removed_relations.iter().map(|r| names.relation(r)).collect();
    let changed_resources = names.resources(&diff.changed_resources)?;
    let removed_resources = diff.removed_resources.iter().map(|t| names.intern(t)).collect();
    let body = BinaryDiff {
        version: WIRE_VERSION,
        string_table: names.table,
        added_entities,
        removed_entities: diff.removed_entities.clone(),
        added_components,
        removed_components,
        modified_components,
        added_relations,
        removed_relations,
        changed_resources,
        removed_resources,
    };
    with_magic(DIFF_MAGIC, &body)
}

pub(crate) fn diff_from_binary(data: &[u8]) -> Result<WorldDiff, SerializationError> {
    let Some(body) = data.strip_prefix(&DIFF_MAGIC) else {
        return crate::legacy::diff_from_bincode(data);
    };
    binary_version(body)?;
    let doc: BinaryDiff = apex_core::binary::from_bytes(body)?;
    let names = Names(doc.string_table);
    let components = |list: Vec<(u32, Vec<BinaryPayload>)>| {
        list.into_iter()
            .map(|(idx, comps)| Ok((idx, names.components(comps)?)))
            .collect::<Result<Vec<_>, SerializationError>>()
    };
    let added_components = components(doc.added_components)?;
    let modified_components = components(doc.modified_components)?;
    Ok(WorldDiff {
        version: doc.version,
        added_entities: doc.added_entities.into_iter().map(|e| names.entity(e)).collect::<Result<_, _>>()?,
        removed_entities: doc.removed_entities,
        added_components,
        removed_components: doc
            .removed_components
            .into_iter()
            .map(|(idx, types)| {
                Ok((idx, types.into_iter().map(|t| names.get(t)).collect::<Result<Vec<_>, SerializationError>>()?))
            })
            .collect::<Result<_, SerializationError>>()?,
        modified_components,
        added_relations: doc.added_relations.into_iter().map(|r| names.relation(r)).collect::<Result<_, _>>()?,
        removed_relations: doc.removed_relations.into_iter().map(|r| names.relation(r)).collect::<Result<_, _>>()?,
        changed_resources: doc.changed_resources.into_iter().map(|r| names.resource(r)).collect::<Result<_, _>>()?,
        removed_resources: doc.removed_resources.into_iter().map(|t| names.get(t)).collect::<Result<_, _>>()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind of record and payload a snapshot can hold, in the order a text document keeps
    /// them (JSON components of an entity first, binary ones after).
    fn full_snapshot() -> WorldSnapshot {
        let mut snap = WorldSnapshot::new(42);
        snap.entities.push(EntitySnapshot {
            original_index: 7,
            components: vec![
                // Fields in declaration order, not sorted — the order a struct's serde writes, which a
                // re-print through `serde_json::Value` would sort.
                ComponentSnapshot::new_json("game::Health", br#"{"max":100.0,"hp":50.25}"#.to_vec()),
                ComponentSnapshot::new_json("game::Name", br#""Door \"A\"""#.to_vec()),
                ComponentSnapshot::new_json("game::Frozen", Vec::new()),
                ComponentSnapshot::new_binary("game::Packed", vec![0, 255, 7, 1]),
            ],
        });
        snap.entities.push(EntitySnapshot { original_index: 9, components: Vec::new() });
        snap.relations.push(RelationSnapshot {
            subject_index: 7,
            target_index:  9,
            kind_name:     "apex_core::relations::ChildOf".to_string(),
        });
        snap.resources.push(ResourceSnapshot {
            type_name: "game::Blob".into(),
            format:    DataFormat::Binary,
            data:      vec![1, 2, 3],
        });
        snap.resources.push(ResourceSnapshot {
            type_name: "game::Settings".into(),
            format:    DataFormat::Json,
            data:      br#"{"volume":0.1}"#.to_vec(),
        });
        snap
    }

    #[test]
    fn the_text_document_round_trips_every_payload_byte_for_byte() {
        let snap = full_snapshot();
        let text = snap.to_json().unwrap();
        assert_eq!(WorldSnapshot::from_json(&text).unwrap(), snap);
        // And a second save of the loaded document is the same document.
        assert_eq!(WorldSnapshot::from_json(&text).unwrap().to_json().unwrap(), text);
    }

    #[test]
    fn the_text_document_holds_values_named_by_type_not_bytes() {
        let text = String::from_utf8(full_snapshot().to_json().unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["format"], "apex-world");
        assert_eq!(v["version"], WIRE_VERSION);
        let comps = &v["entities"][0]["components"];
        assert_eq!(comps["game::Health"]["hp"], 50.25, "a JSON component is its value: {text}");
        assert_eq!(comps["game::Name"], "Door \"A\"");
        assert!(comps["game::Frozen"].is_null(), "bare presence is null");
        assert_eq!(v["entities"][0]["binary_components"]["game::Packed"], serde_json::json!([0, 255, 7, 1]));
        assert_eq!(v["resources"]["game::Settings"]["volume"], 0.1);
        assert_eq!(v["relations"][0]["kind"], "apex_core::relations::ChildOf");
        assert!(!text.contains("\"data\""), "no payload is spelled as bytes: {text}");
        // The value is written verbatim: not re-printed through a float parser.
        assert!(text.contains(r#""game::Health": {"max":100.0,"hp":50.25}"#), "{text}");
    }

    #[test]
    fn the_binary_form_round_trips_behind_its_magic_word() {
        let snap = full_snapshot();
        let bytes = snap.to_binary().unwrap();
        assert_eq!(&bytes[..4], b"APXW");
        assert_eq!(WorldSnapshot::from_binary(&bytes).unwrap(), snap);
        assert_eq!(WorldSnapshot::from_binary(&bytes).unwrap().to_binary().unwrap(), bytes);
    }

    #[test]
    fn the_binary_form_interns_repeated_type_names() {
        let mut snap = WorldSnapshot::new(0);
        for i in 0..100 {
            snap.entities.push(EntitySnapshot {
                original_index: i,
                components: vec![ComponentSnapshot::new_binary("my_crate::components::Position", vec![1, 2])],
            });
        }
        let bytes = snap.to_binary().unwrap();
        let name = b"my_crate::components::Position";
        let count = bytes.windows(name.len()).filter(|w| w == name).count();
        assert_eq!(count, 1, "a type name is stored once");
    }

    #[test]
    fn a_diff_round_trips_with_its_resources() {
        let snap = full_snapshot();
        let mut diff = WorldDiff::new();
        diff.added_entities = snap.entities.clone();
        diff.removed_entities = vec![3, 4];
        diff.added_components = vec![(9, vec![ComponentSnapshot::new_json("game::Name", br#""x""#.to_vec())])];
        diff.removed_components = vec![(7, vec!["game::Frozen".into()])];
        diff.modified_components = vec![(7, vec![ComponentSnapshot::new_binary("game::Packed", vec![9])])];
        diff.added_relations = snap.relations.clone();
        diff.removed_relations = snap.relations.clone();
        diff.changed_resources = snap.resources.clone();
        diff.removed_resources = vec!["game::Gone".into()];
        let bytes = diff.to_binary().unwrap();
        assert_eq!(&bytes[..4], b"APXD");
        assert_eq!(WorldDiff::from_binary(&bytes).unwrap(), diff);
    }

    #[test]
    fn a_newer_version_is_a_mismatch_in_both_forms() {
        let text = br#"{"format":"apex-world","version":5,"tick":0}"#;
        assert!(matches!(
            WorldSnapshot::from_json(text),
            Err(SerializationError::VersionMismatch { expected: 4, found: 5 })
        ));
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.extend(apex_core::binary::to_vec(&5u32).unwrap());
        bytes.extend([0u8; 16]);
        assert!(matches!(
            WorldSnapshot::from_binary(&bytes),
            Err(SerializationError::VersionMismatch { expected: 4, found: 5 })
        ));
    }

    #[test]
    fn a_legacy_payload_is_refused_on_write_not_written_as_garbage() {
        let mut snap = full_snapshot();
        snap.resources[1].format = DataFormat::LegacyBincode;
        assert!(matches!(snap.to_json(), Err(SerializationError::LegacyPayload { what: "resource", .. })));
        assert!(matches!(snap.to_binary(), Err(SerializationError::LegacyPayload { what: "resource", .. })));
        let mut snap = full_snapshot();
        snap.entities[0].components[3].format = DataFormat::LegacyBincode;
        assert!(matches!(snap.to_json(), Err(SerializationError::LegacyPayload { what: "component", .. })));
    }

    #[test]
    fn a_hand_edited_document_with_a_typo_or_a_twice_named_type_is_an_error() {
        let typo = br#"{"format":"apex-world","version":4,"tick":0,
            "entities":[{"id":1,"component":{"game::Name":"a"}}]}"#;
        assert!(WorldSnapshot::from_json(typo).is_err(), "an unknown field is not silently dropped");
        let twice = br#"{"format":"apex-world","version":4,"tick":0,
            "entities":[{"id":1,"components":{"game::Name":"a","game::Name":"b"}}]}"#;
        let err = WorldSnapshot::from_json(twice).unwrap_err().to_string();
        assert!(err.contains("named twice"), "{err}");
        let foreign = br#"{"format":"something-else","version":4,"tick":0}"#;
        assert!(matches!(WorldSnapshot::from_json(foreign), Err(SerializationError::NotADocument { .. })));
    }

    #[test]
    fn a_payload_block_is_the_bytes_a_sequence_of_u8_would_write() {
        #[derive(Serialize)]
        struct AsSequence {
            name:   u32,
            format: WireFormat,
            data:   Vec<u8>,
        }
        let data: Vec<u8> = (0..=255).collect();
        let block = BinaryPayload { name: 3, format: WireFormat::Binary, data: Cow::Borrowed(&data) };
        let seq = AsSequence { name: 3, format: WireFormat::Binary, data: data.clone() };
        assert_eq!(apex_core::binary::to_vec(&block).unwrap(), apex_core::binary::to_vec(&seq).unwrap());
    }

    #[test]
    fn a_corrupt_string_table_index_is_rejected() {
        let body = BinarySnapshot {
            version:      WIRE_VERSION,
            tick:         0,
            string_table: vec![Cow::Borrowed("only_one")],
            entities:     vec![BinaryEntity {
                index:      0,
                components: vec![BinaryPayload { name: 5, format: WireFormat::Json, data: Cow::Borrowed(&[]) }],
            }],
            relations:    Vec::new(),
            resources:    Vec::new(),
        };
        let bytes = with_magic(SNAPSHOT_MAGIC, &body).unwrap();
        assert!(matches!(WorldSnapshot::from_binary(&bytes), Err(SerializationError::NotADocument { .. })));
    }

    #[test]
    fn neither_form_is_mistaken_for_the_other_or_for_noise() {
        let snap = full_snapshot();
        assert!(WorldSnapshot::from_binary(&snap.to_json().unwrap()).is_err());
        assert!(WorldSnapshot::from_json(&snap.to_binary().unwrap()).is_err());
        assert!(WorldSnapshot::from_binary(b"\xff\xff\xff\xff garbage").is_err());
        assert!(WorldDiff::from_binary(&snap.to_binary().unwrap()).is_err(), "a snapshot is not a diff");
    }
}
