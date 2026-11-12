//! WorldSerializer — world snapshot and restore logic.
//!
//! Supports:
//! - The text document and the binary form (`crate::wire`), older versions read (`crate::legacy`)
//! - Versioning with automatic migration
//! - Incremental diff-saves
//! - File I/O with arbitrary format

use std::collections::{HashMap, HashSet};
use std::path::Path;

use apex_core::{
    component::ComponentId,
    entity::Entity,
    relations::ChildOf,
    world::World,
};

use crate::prefab::{PrefabChild, PrefabComponent, PrefabManifest};
use crate::snapshot::{
    ComponentSnapshot, DataFormat, EntitySnapshot, RelationSnapshot, ResourceSnapshot, SaveFormat,
    WorldDiff, WorldSnapshot,
};
use apex_core::{PayloadKind, SERDE_FORMAT_JSON, SERDE_FORMAT_MSGPACK, SERDE_FORMAT_POSTCARD};

/// How the payloads of a snapshot are written (ADR-024, ADR-025). The format belongs to the
/// WRITE, not to the type: a plain registration gives a type every format, and the caller picks
/// one per snapshot — a document a person reads now, a binary save of the same world a moment
/// later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Payloads {
    /// Each type in the format its registration named.
    #[default]
    AsRegistered,
    /// Every payload as a JSON value.
    Text,
    /// Every payload as binary bytes (`postcard` for a plain registration). `postcard` stores no
    /// field names: such a payload is read back only by the build that wrote it, so this is the
    /// format of a snapshot that lives inside one run, not of a save file (see ADR-024).
    Binary,
    /// Every payload as self-describing binary bytes (`apex_core::named`: MessagePack with struct
    /// fields by name). A later build still reads them — a field added with `#[serde(default)]`,
    /// a field removed, a variant added — so this, in the binary container
    /// ([`WorldSnapshot::to_binary`]), is the format of a binary SAVE (ADR-025).
    Named,
}

impl Payloads {
    /// The resource format name of the choice; `None` — as registered.
    fn resource_format(self) -> Option<&'static str> {
        match self {
            Payloads::AsRegistered => None,
            Payloads::Text => Some(SERDE_FORMAT_JSON),
            Payloads::Binary => Some(SERDE_FORMAT_POSTCARD),
            Payloads::Named => Some(SERDE_FORMAT_MSGPACK),
        }
    }

    /// The payload kind of the choice; `None` — as registered.
    fn kind(self) -> Option<PayloadKind> {
        match self {
            Payloads::AsRegistered => None,
            Payloads::Text => Some(PayloadKind::Text),
            Payloads::Binary => Some(PayloadKind::Binary),
            Payloads::Named => Some(PayloadKind::Named),
        }
    }
}

/// What [`WorldSerializer::snapshot_opts`] takes into a snapshot and how it writes it. `Default`
/// is the plain full snapshot of [`WorldSerializer::snapshot_with`].
#[derive(Default)]
pub struct SnapshotOptions<'a> {
    /// Entities to keep; `None` — every entity (see
    /// [`snapshot_with_filter`](WorldSerializer::snapshot_with_filter)).
    pub keep: Option<&'a dyn Fn(Entity) -> bool>,
    /// The payload format.
    pub payloads: Payloads,
    /// `Some(key)` — a **canonical** snapshot (ADR-024): equal worlds give equal bytes, whatever
    /// order their entities were spawned in and however many were despawned on the way. `key`
    /// names an entity by something the HOST keeps stable (a unit id, an editor id) and must be
    /// unique among the kept entities. Then:
    /// - entities are written in key order and numbered 0, 1, 2… in that order;
    /// - an `Entity` inside a component that registered `MapEntities`, and both ends of a
    ///   relation, are written as those numbers;
    /// - an entity's components are ordered by type name, relations by kind name, then target;
    /// - the tick is 0.
    ///
    /// A reference to an entity outside the snapshot has no history-independent spelling and is
    /// an error ([`SerializationError::CanonicalOutsideRef`]); `Entity::PLACEHOLDER` is a
    /// constant and is kept.
    pub canonical: Option<&'a dyn Fn(Entity) -> u64>,
}

/// One entity's serialized components — the SHARED body of
/// [`WorldSerializer::snapshot_opts`] (archetype scan) and
/// [`WorldSerializer::snapshot_entities`] (random access via `entity_location`): two copies of
/// this loop would drift (§0.2a).
///
/// `numbers` is the canonical numbering (old entity index → number in the snapshot), when the
/// snapshot is canonical.
fn entity_snapshot(
    world: &World,
    arch: &apex_core::archetype::Archetype,
    row: usize,
    entity: Entity,
    ctx: &mut dyn apex_core::SerdeContext,
    payloads: Payloads,
    numbers: Option<&HashMap<u32, u32>>,
) -> Result<EntitySnapshot, SerializationError> {
    let mut entity_snap = EntitySnapshot {
        original_index: entity.index(),
        components:     Vec::new(),
    };

    for col in arch.columns() {
        let cid  = col.id();
        let info = match world.registry().get_info(cid) {
            Some(i) => i,
            None    => continue,
        };

        // Components without serde are skipped
        let registered = match &info.serde {
            Some(s) => s,
            None    => continue,
        };

        // ZST marker (Folder/Model/Hidden/…): no bytes, but its PRESENCE is the data —
        // write an empty snapshot so that restore re-inserts the component. (Previously
        // `continue` silently lost marker components on save/load.)
        if info.size == 0 {
            entity_snap
                .components
                .push(ComponentSnapshot::new_json(info.name.to_string(), Vec::new()));
            continue;
        }

        // The fns of the format this snapshot is written in (ADR-024).
        let serde_fns = match payloads.kind() {
            None => registered,
            Some(kind) => info.serde_as(kind).ok_or_else(|| SerializationError::PayloadFormat {
                what:       "component",
                type_name:  info.name.to_string(),
                wanted:     kind.name(),
                registered: registered.format,
            })?,
        };

        // SAFETY: `row` is a live row of this archetype (the caller took it from the archetype's
        // entity list or from the entity's location).
        let live = unsafe { col.get_raw_ptr(row) };
        let raw_bytes = match (numbers, info.map_entities) {
            // Canonical: the entity references of this component are written as snapshot numbers.
            (Some(numbers), Some(map_fn)) => {
                // SAFETY: `live` is the live value of the type `info` describes; `map_fn` and
                // `serde_fns` were registered for that type.
                unsafe { renumbered_payload(info, serde_fns, map_fn, live, ctx, numbers) }?
            }
            _ => unsafe { (serde_fns.serialize_fn)(live, ctx) }.map_err(|e| {
                SerializationError::SerializeFailed {
                    type_name: info.name.to_string(),
                    reason:    e.to_string(),
                }
            })?,
        };

        // The payload is carried as what its fns wrote.
        let name = info.name.to_string();
        entity_snap.components.push(match PayloadKind::of(serde_fns.format) {
            PayloadKind::Text => ComponentSnapshot::new_json(name, raw_bytes),
            PayloadKind::Binary => ComponentSnapshot::new_binary(name, raw_bytes),
            PayloadKind::Named => ComponentSnapshot::new_named(name, raw_bytes),
        });
    }

    Ok(entity_snap)
}

/// The payload of a component whose `Entity` references are rewritten to canonical numbers.
///
/// The world is not touched (a snapshot reads it), and a component cannot be cloned in general,
/// so the value takes a trip through its own serde: it is written, read back into a private
/// copy, the copy's references are mapped, and the copy is written. This is the trip every
/// restore makes anyway, and only a type that registered `MapEntities` pays for it.
///
/// # Safety
/// `live` points at a live value of the type `info` describes; `serde_fns` and `map_fn` were
/// registered for that type.
unsafe fn renumbered_payload(
    info: &apex_core::component::ComponentInfo,
    serde_fns: &apex_core::ComponentSerdeFns,
    map_fn: apex_core::MapEntitiesFn,
    live: *const u8,
    ctx: &mut dyn apex_core::SerdeContext,
    numbers: &HashMap<u32, u32>,
) -> Result<Vec<u8>, SerializationError> {
    let failed = |reason: String| SerializationError::SerializeFailed {
        type_name: info.name.to_string(),
        reason,
    };
    let bytes = (serde_fns.serialize_fn)(live, ctx).map_err(|e| failed(e.to_string()))?;
    let packed = (serde_fns.deserialize_fn)(&bytes, ctx)
        .map_err(|e| failed(format!("the value does not read back for renumbering: {e}")))?;
    debug_assert_eq!(packed.len(), info.size);

    /// The private copy in an allocation of the type's own alignment (`deserialize_fn` hands the
    /// value over in a byte vector, which promises none); dropped as the type on the way out.
    struct Copy {
        ptr:     *mut u8,
        layout:  std::alloc::Layout,
        drop_fn: unsafe fn(*mut u8),
    }
    impl Drop for Copy {
        fn drop(&mut self) {
            // SAFETY: `ptr` holds one live value of the type `drop_fn` belongs to, in an
            // allocation of `layout`.
            unsafe {
                (self.drop_fn)(self.ptr);
                std::alloc::dealloc(self.ptr, self.layout);
            }
        }
    }
    // The ZST branch of `entity_snapshot` returned earlier, so the size is not zero.
    let layout = std::alloc::Layout::from_size_align(info.size, info.align)
        .map_err(|e| failed(e.to_string()))?;
    let ptr = std::alloc::alloc(layout);
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // The value moves bitwise out of `packed` (plain bytes, dropped as such) into the copy.
    std::ptr::copy_nonoverlapping(packed.as_ptr(), ptr, info.size);
    let copy = Copy { ptr, layout, drop_fn: info.drop_fn };

    let mut outside = None;
    map_fn(copy.ptr, &mut |old: Entity| {
        if old == Entity::PLACEHOLDER {
            return old;
        }
        match numbers.get(&old.index()) {
            Some(&number) => Entity::from_raw_parts(number, 0),
            None => {
                outside.get_or_insert(old.index());
                old
            }
        }
    });
    if let Some(index) = outside {
        return Err(SerializationError::CanonicalOutsideRef { type_name: info.name.to_string(), index });
    }
    (serde_fns.serialize_fn)(copy.ptr, ctx).map_err(|e| failed(e.to_string()))
}

// ── Errors ────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    #[error("component `{type_name}` serialize failed: {reason}")]
    SerializeFailed { type_name: String, reason: String },

    #[error("component `{type_name}` deserialize failed: {reason}")]
    DeserializeFailed { type_name: String, reason: String },

    #[error("component `{type_name}` not registered in world")]
    ComponentNotRegistered { type_name: String },

    #[error("snapshot version {found} is not supported (expected {expected})")]
    VersionMismatch { expected: u32, found: u32 },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("binary format error: {0}")]
    Binary(#[from] apex_core::binary::Error),

    #[error("legacy bincode document: {0}")]
    LegacyBincode(#[from] Box<bincode::ErrorKind>),

    #[error("not an apex world document: {reason}")]
    NotADocument { reason: String },

    #[error("{what} `{type_name}` holds a payload in the retired bincode encoding; restore the document into a world and snapshot it again before writing it")]
    LegacyPayload { what: &'static str, type_name: String },

    #[error("{what} `{type_name}` cannot be written: {reason}")]
    Encode { what: &'static str, type_name: String, reason: String },

    #[error("resource snapshot failed: {0}")]
    ResourceSerializeFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("diff error: {reason}")]
    DiffError { reason: String },

    #[error("migration error: {0}")]
    Migration(String),

    #[error("entity with index {index} not found in world")]
    EntityNotFound { index: u32 },

    #[error("{what} `{type_name}` cannot be written as {wanted}: its registration gives only {registered}")]
    PayloadFormat { what: &'static str, type_name: String, wanted: &'static str, registered: &'static str },

    #[error("canonical snapshot: entities {first} and {second} share the key {key} — the key must name one entity")]
    CanonicalKeyTwice { key: u64, first: u32, second: u32 },

    #[error("canonical snapshot: component `{type_name}` refers to entity {index}, which is not in the snapshot — such a reference has no history-independent spelling")]
    CanonicalOutsideRef { type_name: String, index: u32 },
}

// ── RestoreEntityMap ───────────────────────────────────────────

/// Mapping of old index → new Entity, returned from `restore`.
pub type RestoreEntityMap = HashMap<u32, Entity>;

// ── WorldSerializer ────────────────────────────────────────────

pub struct WorldSerializer;

impl WorldSerializer {
    // ── Snapshot ───────────────────────────────────────────────

    /// Create a full world snapshot. Plain (de)serialization without resolving external references — for
    /// scenes with components that reference assets/entities, use [`snapshot_with`](Self::snapshot_with).
    pub fn snapshot(world: &World) -> Result<WorldSnapshot, SerializationError> {
        Self::snapshot_with(world, &mut apex_core::NoContext)
    }

    /// Snapshot with a **(de)serialization context** (TD-44): context-dependent components (asset Handle,
    /// Entity reference) resolve their references through `ctx` (implemented by the engine/editor). Plain
    /// components ignore `ctx` ⇒ the result is identical to [`snapshot`](Self::snapshot).
    pub fn snapshot_with(
        world: &World,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<WorldSnapshot, SerializationError> {
        Self::snapshot_with_filter(world, ctx, &|_| true)
    }

    /// [`snapshot_with`](Self::snapshot_with), restricted to the entities `keep` returns `true` for.
    /// The caller decides what belongs in the document — e.g. a scene editor keeps only entities that
    /// carry its stable-id component, excluding editor infrastructure (camera/grid) that shares the
    /// scene world but is not saved content. apex-ecs stays policy-free: `keep` is host-supplied.
    pub fn snapshot_with_filter(
        world: &World,
        ctx: &mut dyn apex_core::SerdeContext,
        keep: &dyn Fn(apex_core::Entity) -> bool,
    ) -> Result<WorldSnapshot, SerializationError> {
        Self::snapshot_opts(world, ctx, &SnapshotOptions { keep: Some(keep), ..Default::default() })
    }

    /// The full snapshot with every choice the caller has (ADR-024): which entities
    /// ([`SnapshotOptions::keep`]), which payload format ([`SnapshotOptions::payloads`]), and
    /// whether the snapshot is canonical ([`SnapshotOptions::canonical`]). With
    /// `SnapshotOptions::default()` this is [`snapshot_with`](Self::snapshot_with).
    pub fn snapshot_opts(
        world: &World,
        ctx: &mut dyn apex_core::SerdeContext,
        opts: &SnapshotOptions<'_>,
    ) -> Result<WorldSnapshot, SerializationError> {
        let tick = if opts.canonical.is_some() { 0 } else { world.current_tick().0 };
        let mut snap = WorldSnapshot::new(tick);

        // ── Which entities, in which order ─────────────────────
        let mut picked: Vec<(&apex_core::archetype::Archetype, usize, Entity)> = Vec::new();
        for arch in world.archetypes() {
            if arch.is_empty() { continue; }

            for (row, &entity) in arch.entities().iter().enumerate() {
                if opts.keep.is_none_or(|keep| keep(entity)) {
                    picked.push((arch, row, entity));
                }
            }
        }
        // Canonical: key order, and the numbering every reference is rewritten to.
        let numbers: Option<HashMap<u32, u32>> = match opts.canonical {
            None => None,
            Some(key) => {
                let mut keyed: Vec<(u64, (&apex_core::archetype::Archetype, usize, Entity))> =
                    picked.drain(..).map(|p| (key(p.2), p)).collect();
                keyed.sort_by_key(|(k, _)| *k);
                if let Some(pair) = keyed.windows(2).find(|pair| pair[0].0 == pair[1].0) {
                    return Err(SerializationError::CanonicalKeyTwice {
                        key:    pair[0].0,
                        first:  pair[0].1 .2.index(),
                        second: pair[1].1 .2.index(),
                    });
                }
                picked.extend(keyed.into_iter().map(|(_, p)| p));
                Some(picked.iter().enumerate().map(|(i, p)| (p.2.index(), i as u32)).collect())
            }
        };
        // Entity indices that made it into the snapshot — used to drop relations whose subject/target was
        // filtered out (e.g. a scene instance's inner nodes), which would otherwise warn + skip on restore.
        let kept: HashSet<u32> = picked.iter().map(|p| p.2.index()).collect();

        // ── Entities + Components ──────────────────────────────
        snap.entities.reserve(picked.len());
        for &(arch, row, entity) in &picked {
            let mut entity_snap =
                entity_snapshot(world, arch, row, entity, ctx, opts.payloads, numbers.as_ref())?;
            if let Some(numbers) = &numbers {
                entity_snap.original_index = numbers[&entity.index()];
                // Column order follows component ids, which follow registration order.
                entity_snap.components.sort_by(|a, b| a.type_name.cmp(&b.type_name));
            }
            snap.entities.push(entity_snap);
        }

        // ── Relations ──────────────────────────────────────────
        // Target-major DETERMINISTIC order (kinds ↑, targets ↑, subjects in sibling
        // order): restore re-adds relations in list order, so sibling order (core
        // ADR-008 — UI z-order, editor child order) round-trips exactly. The indexes
        // are cleaned up on despawn, so only relations of live entities are yielded.
        for (subject, kind_idx, target_index) in world.iter_relations_target_major() {
            let subject_index = subject.index();
            // Only keep relations between snapshotted entities — a relation to a filtered-out entity
            // (e.g. a scene instance's inner node) can't restore, so don't save it (avoids restore warns).
            if !kept.contains(&subject_index) || !kept.contains(&target_index) {
                continue;
            }
            let kind_name = world.relation_registry()
                .get_name(kind_idx)
                .unwrap_or("<unknown>")
                .to_string();

            snap.relations.push(RelationSnapshot {
                subject_index,
                target_index,
                kind_name,
            });
        }
        if let Some(numbers) = &numbers {
            for rel in &mut snap.relations {
                rel.subject_index = numbers[&rel.subject_index];
                rel.target_index = numbers[&rel.target_index];
            }
            // Kinds by NAME (their index is registration order), targets by number; the sort is
            // stable, so the subjects of one target keep their sibling order (ADR-008).
            snap.relations.sort_by(|a, b| {
                (a.kind_name.as_str(), a.target_index).cmp(&(b.kind_name.as_str(), b.target_index))
            });
        }

        // ── Resources (E7) ─────────────────────────────────────
        // Only resources opted in via `register_resource_serde` (a world may
        // hold non-serializable resources — GPU handles etc.).
        for r in world
            .snapshot_resources_serde_as(opts.payloads.resource_format())
            .map_err(SerializationError::ResourceSerializeFailed)?
        {
            snap.resources.push(ResourceSnapshot {
                type_name: r.type_name,
                format:    DataFormat::of(r.format),
                data:      r.data,
            });
        }

        Ok(snap)
    }

    /// Snapshot ONLY the given entities (plus the relations among them) — **O(|entities|), not
    /// O(world)**. The editor's despawn capture is the consumer (engine PERF_ECONOMY PE.2): with
    /// [`snapshot_with_filter`](Self::snapshot_with_filter) each captured subtree paid a full
    /// archetype scan AND a sorted dump of every relation in the world, which made deleting N
    /// models quadratic in practice.
    ///
    /// Semantics match the filter path exactly per entity (same component serialization — one
    /// shared helper — and the same deterministic relation order: kinds ascending, targets by
    /// index ascending, subjects in sibling order). Differences, both deliberate: the entity ORDER
    /// in the snapshot is the caller's list order (restore does not depend on it), and
    /// **resources are NOT included** — an entity-scoped capture is not a world save (full saves
    /// go through `snapshot_with*`). Dead entities in the list are skipped.
    ///
    /// # A snapshot owns only what it captured
    ///
    /// **Relations with an end OUTSIDE `entities` are NOT captured** — a snapshot is
    /// self-contained, and an edge to an entity it does not own has nothing to restore against
    /// (the stored target is an index, which the allocator may have handed to a stranger by
    /// restore time). This is not a filtering detail: it means a captured entity's link to a
    /// SURVIVING parent is the CALLER's to preserve. The engine editor's structural undo learned
    /// this the hard way — deleting a nested object and undoing brought it back detached at the
    /// scene root, and with a nested transform that made it visibly the wrong size; it now records
    /// the parent by its own stable identity alongside the snapshot (`apex-editor`
    /// `SubtreeCapture::parent`). A caller that restores into a world where those outside ends
    /// still live must do the same.
    pub fn snapshot_entities(
        world: &World,
        ctx: &mut dyn apex_core::SerdeContext,
        entities: &[Entity],
    ) -> Result<WorldSnapshot, SerializationError> {
        let tick = world.current_tick().0;
        let mut snap = WorldSnapshot::new(tick);
        let mut kept: HashSet<u32> = HashSet::new();
        for &e in entities {
            if world.is_alive(e) {
                kept.insert(e.index());
            }
        }

        for &entity in entities {
            let Some(loc) = world.entity_location(entity) else { continue };
            let arch = &world.archetypes()[loc.archetype_id.as_usize()];
            snap.entities.push(entity_snapshot(
                world, arch, loc.row as usize, entity, ctx, Payloads::AsRegistered, None,
            )?);
        }

        let targets: Vec<u32> = kept.iter().copied().collect();
        for (subject, kind_idx, target_index) in
            world.iter_relations_target_major_among(&targets)
        {
            if !kept.contains(&subject.index()) {
                continue;
            }
            let kind_name = world
                .relation_registry()
                .get_name(kind_idx)
                .unwrap_or("<unknown>")
                .to_string();
            snap.relations.push(RelationSnapshot { subject_index: subject.index(), target_index, kind_name });
        }

        Ok(snap)
    }

    // ── Restore ────────────────────────────────────────────────

    /// Restore a world from a snapshot. Plain (de)serialization — for components with external references
    /// see [`restore_with`](Self::restore_with).
    ///
    /// Older snapshot versions are migrated automatically (see `restore_with`).
    pub fn restore(
        world:    &mut World,
        snapshot: &WorldSnapshot,
    ) -> Result<RestoreEntityMap, SerializationError> {
        Self::restore_with(world, snapshot, &mut apex_core::NoContext)
    }

    /// Restore a world from a snapshot with a **(de)serialization context** (TD-44): context-dependent
    /// components resolve external references through `ctx`. Plain components ignore `ctx`.
    ///
    /// # Versioning (single choke point)
    /// Migration is centralised here — the universal path every consumer passes through — so no loader
    /// can skip it. An older-version snapshot is migrated up to [`WorldSnapshot::CURRENT_VERSION`] on a
    /// throwaway working copy (the caller's snapshot is not mutated; the clone happens only on the rare
    /// old-version path — a current-version snapshot restores with no copy). A newer, unmigratable
    /// version is rejected with [`SerializationError::VersionMismatch`]. Previously the check was a strict
    /// equality that rejected old versions too, and migration only ran in `read_from_file` — so the
    /// editor's `from_json` + `restore_with` byte path silently bypassed it.
    ///
    /// Every snapshot entity is created fresh; to instead REUSE existing entities
    /// (apply-back / merge by an external key), see [`restore_merge_with`](Self::restore_merge_with).
    pub fn restore_with(
        world:    &mut World,
        snapshot: &WorldSnapshot,
        ctx:      &mut dyn apex_core::SerdeContext,
    ) -> Result<RestoreEntityMap, SerializationError> {
        // No resolver ⇒ every snapshot entity is spawned fresh (classic restore).
        Self::restore_impl(world, snapshot, ctx, &mut |_| None)
    }

    /// Restore a snapshot into `world`, but **reuse existing entities** where the
    /// caller's `resolve` maps a snapshot's `original_index` to an entity already
    /// present in `world`; unresolved entities are spawned fresh. This is the
    /// golden-path **apply-back / merge** primitive (В4): a subtree edited in a
    /// forked world can be committed back onto the source world without
    /// duplicating entities — the consumer keys `resolve` on its stable identity
    /// (e.g. the editor's `EditorId`), so component values land on the SAME
    /// entities rather than clones.
    ///
    /// Merge semantics: a resolved entity's components named in the snapshot are
    /// overwritten (insert overwrites); components it has that are NOT in the
    /// snapshot are left untouched (additive/update, not replace). Relations in
    /// the snapshot are added as usual. Entity references inside components are
    /// remapped through the resulting old→entity map, so cross-references land on
    /// resolved-or-created targets consistently.
    pub fn restore_merge_with(
        world:    &mut World,
        snapshot: &WorldSnapshot,
        ctx:      &mut dyn apex_core::SerdeContext,
        resolve:  &mut dyn FnMut(u32) -> Option<Entity>,
    ) -> Result<RestoreEntityMap, SerializationError> {
        Self::restore_impl(world, snapshot, ctx, resolve)
    }

    /// Shared restore worker: version-resolves the snapshot, then creates or
    /// reuses each entity via `resolve`, inserts components, remaps Entity refs
    /// (E6), and restores relations + resources.
    fn restore_impl(
        world:    &mut World,
        snapshot: &WorldSnapshot,
        ctx:      &mut dyn apex_core::SerdeContext,
        resolve:  &mut dyn FnMut(u32) -> Option<Entity>,
    ) -> Result<RestoreEntityMap, SerializationError> {
        // Bring the snapshot to the current version (migrate old; reject future). The migrated copy is
        // only allocated when the version is actually behind; at the current version `snapshot` is used
        // directly, so the common path is allocation-free.
        let migrated;
        let snapshot = if snapshot.version == WorldSnapshot::CURRENT_VERSION {
            snapshot
        } else if snapshot.version < WorldSnapshot::CURRENT_VERSION {
            let mut working = snapshot.clone();
            working.migrate().map_err(SerializationError::Migration)?;
            migrated = working;
            &migrated
        } else {
            return Err(SerializationError::VersionMismatch {
                expected: WorldSnapshot::CURRENT_VERSION,
                found:    snapshot.version,
            });
        };

        let mut entity_map: RestoreEntityMap = HashMap::with_capacity(snapshot.entities.len());
        // Mark all restored data as **freshly changed at load time**: bump the change tick and stamp the
        // inserted components with the new current tick. Otherwise restore stamped them with the SAVED
        // snapshot tick (older), which a change-detecting consumer (transform propagation, render
        // extract, picking) whose `last_run` is already past it would skip — e.g. transforms staying at
        // the world origin (and lights mis-aimed) after loading a scene into a long-running world. This
        // touches only the (rare) load path; the per-frame change-detection model is unchanged.
        world.advance_change_tick();
        let tick = world.current_tick();

        // Build a type_name → ComponentId mapping from the registered components.
        let name_to_id: HashMap<String, ComponentId> = world
            .registry()
            .iter()
            .map(|info| (info.name.to_string(), info.id))
            .collect();

        // ── Step 1: Entity + components ───────────────────────
        for entity_snap in &snapshot.entities {
            // Reuse an existing entity if the caller resolves this index (apply-back),
            // otherwise spawn a fresh one (classic restore).
            let new_entity = resolve(entity_snap.original_index)
                .unwrap_or_else(|| world.spawn(()));
            entity_map.insert(entity_snap.original_index, new_entity);

            for comp_snap in &entity_snap.components {
                let component_id = match name_to_id.get(&comp_snap.type_name) {
                    Some(&id) => id,
                    None      => return Err(SerializationError::ComponentNotRegistered {
                        type_name: comp_snap.type_name.clone(),
                    }),
                };

                // ZST marker: no bytes, restore by presence (empty insert), without calling
                // deserialize_fn (data is empty — nothing to parse). Paired with the ZST branch in snapshot.
                if world.registry().get_info(component_id).map(|i| i.size).unwrap_or(0) == 0 {
                    world.insert_dyn(new_entity, component_id, Vec::new(), tick);
                    continue;
                }

                // Deserialize in a separate scope
                let component_bytes = {
                    let info = world.registry().get_info(component_id).unwrap();
                    let serde_fns = match &info.serde {
                        Some(s) => s,
                        // §0.2a (E8): the type is registered but has no serde
                        // functions, so its snapshot bytes are silently dropped
                        // on restore — the entity comes back missing this
                        // component. Surface it through the world's ErrorHandler
                        // (throttled by default) so the data loss is visible;
                        // register serde for the type to fix.
                        None => {
                            apex_core::anomaly!(
                                world, apex_core::Severity::Warn, "WorldSerializer::restore",
                                None, Some(comp_snap.type_name.as_str()),
                                "component registered without serde functions — snapshot data dropped, entity restored without it"
                            );
                            continue;
                        }
                    };

                    // **Bare presence reads as `null`** (ADR-015). A snapshot written while this
                    // type was a zero-sized marker holds NO bytes (the ZST branch of `snapshot`),
                    // and the prefab form of the same presence is already `null`. The day a marker
                    // grows a field, those documents are still on disk: handing their empty bytes
                    // to serde is `EOF at column 0` and the WHOLE document fails to open (found by
                    // the user on `UiWidgetRoot`, 2026-08-13, and answered then by forbidding the
                    // marker ever to grow). Here the type decides what bare presence means — a
                    // marker that grew reads it as its defaults — and a type that cannot read
                    // `null` still fails loudly, naming itself.
                    match comp_snap.format {
                        // A document of wire v3 or older: decode through the type's legacy reader.
                        DataFormat::LegacyBincode => {
                            let legacy = info.legacy_bincode.ok_or_else(|| SerializationError::DeserializeFailed {
                                type_name: comp_snap.type_name.clone(),
                                reason:    "the payload is in the retired bincode encoding, and the type is registered with context-dependent serde fns, which never wrote it".into(),
                            })?;
                            legacy(&comp_snap.data).map_err(|e| SerializationError::DeserializeFailed {
                                type_name: comp_snap.type_name.clone(),
                                reason:    e.to_string(),
                            })?
                        }
                        // The payload is read by the type's fns of the payload's own format,
                        // whichever format the registration named (ADR-024). A type that has no
                        // reader for it (context-dependent fns exist in one format): name both,
                        // rather than let the wrong reader report a parse error at column 1.
                        format => {
                            let text = format == DataFormat::Json;
                            let reader = format.kind().and_then(|kind| info.serde_as(kind)).ok_or_else(|| SerializationError::DeserializeFailed {
                                type_name: comp_snap.type_name.clone(),
                                reason:    format!(
                                    "the payload is {format:?}, the type is registered as {}",
                                    serde_fns.format
                                ),
                            })?;
                            let raw: &[u8] = if comp_snap.data.is_empty() && text {
                                b"null"
                            } else {
                                &comp_snap.data
                            };

                            (reader.deserialize_fn)(raw, ctx)
                                .map_err(|e| SerializationError::DeserializeFailed {
                                    type_name: comp_snap.type_name.clone(),
                                    reason:    e.to_string(),
                                })?
                        }
                    }
                };

                world.insert_dyn(new_entity, component_id, component_bytes, tick);
            }
        }

        // ── Step 1.5: E6 — remap Entity references inside components ───
        // The old→new map is now COMPLETE (all entities created, forward refs too),
        // so we update the Entity fields of components (e.g. `Target(Entity)`) that
        // registered `MapEntities`. External references (not from the snapshot)
        // stay as-is.
        {
            let new_entities: Vec<apex_core::Entity> = entity_map.values().copied().collect();
            let mut remap = |old: apex_core::Entity| -> apex_core::Entity {
                entity_map.get(&old.index()).copied().unwrap_or(old)
            };
            for e in new_entities {
                world.map_entity_refs(e, &mut remap);
            }
        }

        // ── Step 2: Relations ──────────────────────────────────
        for rel_snap in &snapshot.relations {
            let subject = match entity_map.get(&rel_snap.subject_index) {
                Some(&e) => e,
                None     => {
                    log::warn!(
                        "restore: subject {} not in entity_map, skipping",
                        rel_snap.subject_index
                    );
                    continue;
                }
            };
            let target = match entity_map.get(&rel_snap.target_index) {
                Some(&e) => e,
                None     => {
                    log::warn!(
                        "restore: target {} not in entity_map, skipping relation '{}'",
                        rel_snap.target_index, rel_snap.kind_name
                    );
                    continue;
                }
            };

            if let Some(kind_idx) = world.relation_registry().get_idx_by_name(&rel_snap.kind_name) {
                world.add_relation_by_kind_idx(subject, kind_idx, target);
            } else {
                log::warn!(
                    "restore: relation kind '{}' not registered, skipping",
                    rel_snap.kind_name
                );
            }
        }

        // ── Step 3: Resources (E7) ─────────────────────────────
        for res in &snapshot.resources {
            let restored = match res.format {
                DataFormat::Json => world.restore_resource_serde(&res.type_name, SERDE_FORMAT_JSON, &res.data),
                DataFormat::Binary => world.restore_resource_serde(&res.type_name, SERDE_FORMAT_POSTCARD, &res.data),
                DataFormat::Named => world.restore_resource_serde(&res.type_name, SERDE_FORMAT_MSGPACK, &res.data),
                DataFormat::LegacyBincode => world.restore_resource_serde_legacy_bincode(&res.type_name, &res.data),
            };
            match restored {
                Ok(true) => {}
                // §0.2a: a resource in the snapshot whose type is not registered
                // for serde on this world is silently lost otherwise.
                Ok(false) => log::warn!(
                    "restore: resource '{}' not registered for serde — dropped",
                    res.type_name
                ),
                // A resource that does not read is the same failure as a component that does not
                // read: the document is not this world (ADR-024). It used to be a line in the log,
                // and the world loaded with the resource at whatever it held before — a save that
                // "loaded" with an empty journal.
                Err(e) => {
                    return Err(SerializationError::DeserializeFailed {
                        type_name: res.type_name.clone(),
                        reason:    e,
                    })
                }
            }
        }

        Ok(entity_map)
    }

    // ── Diff ──────────────────────────────────────────────────

    /// Compute the difference between an old snapshot and the current world state.
    ///
    /// Useful for incremental saves: instead of a full snapshot,
    /// only the diff is saved, which can be applied later.
    pub fn diff(
        old_snapshot: &WorldSnapshot,
        new_world:    &World,
    ) -> Result<WorldDiff, SerializationError> {
        Self::diff_with(old_snapshot, new_world, &mut apex_core::NoContext)
    }

    /// `diff` with a **(de)serialization context** (TD-44): the internal snapshot is built via
    /// [`snapshot_with`](Self::snapshot_with), so context-dependent components resolve external
    /// references in the incremental save too — consistent with full `snapshot_with` (no silent `NoContext`).
    pub fn diff_with(
        old_snapshot: &WorldSnapshot,
        new_world:    &World,
        ctx:          &mut dyn apex_core::SerdeContext,
    ) -> Result<WorldDiff, SerializationError> {
        let new_snapshot = Self::snapshot_with(new_world, ctx)?;
        Self::diff_snapshots(old_snapshot, &new_snapshot)
    }

    /// Compute the difference between two snapshots.
    pub fn diff_snapshots(
        old: &WorldSnapshot,
        new: &WorldSnapshot,
    ) -> Result<WorldDiff, SerializationError> {
        let mut diff = WorldDiff::new();

        // Old entities keyed by original_index for fast lookup
        let old_entities: HashMap<u32, &EntitySnapshot> = old.entities
            .iter()
            .map(|e| (e.original_index, e))
            .collect();

        let new_entities: HashMap<u32, &EntitySnapshot> = new.entities
            .iter()
            .map(|e| (e.original_index, e))
            .collect();

        // Removed entities
        for old_entity in &old.entities {
            if !new_entities.contains_key(&old_entity.original_index) {
                diff.removed_entities.push(old_entity.original_index);
            }
        }

        // Added and modified entities
        for new_entity in &new.entities {
            match old_entities.get(&new_entity.original_index) {
                None => {
                    // New entity — add it in full
                    diff.added_entities.push(new_entity.clone());
                }
                Some(old_entity) => {
                    // Existing — compare components with a byte-level delta
                    let old_comps: HashMap<&str, &ComponentSnapshot> = old_entity.components
                        .iter()
                        .map(|c| (c.type_name.as_str(), c))
                        .collect();

                    let mut added = Vec::new();
                    let mut removed = Vec::new();
                    let mut modified = Vec::new();

                    for new_comp in &new_entity.components {
                        match old_comps.get(new_comp.type_name.as_str()) {
                            None => added.push(new_comp.clone()),
                            Some(old_comp) => {
                                // Byte-level delta: compare component data
                                if new_comp.data != old_comp.data {
                                    modified.push(new_comp.clone());
                                }
                                // If the data matches — don't include it in the diff
                            }
                        }
                    }

                    for old_comp in &old_entity.components {
                        if !new_entity.components.iter().any(|c| c.type_name == old_comp.type_name) {
                            removed.push(old_comp.type_name.clone());
                        }
                    }

                    if !added.is_empty() {
                        diff.added_components.push((new_entity.original_index, added));
                    }
                    if !removed.is_empty() {
                        diff.removed_components.push((new_entity.original_index, removed));
                    }
                    if !modified.is_empty() {
                        diff.modified_components.push((new_entity.original_index, modified));
                    }
                }
            }
        }

        // Relations — HashSet membership makes the added/removed scan O(R)
        // instead of O(R²) (`Vec::contains` was linear inside each loop).
        //
        // KNOWN LIMIT (loud, core TECH_DEBT): the diff is EDGE-SET based, so a pure
        // sibling REORDER (same edges, different order — core ADR-008) produces an
        // empty relation diff and `apply_diff` keeps the old order. Full snapshots
        // round-trip order correctly; only the diff/patch path is order-blind.
        let old_relations: HashSet<(u32, u32, &str)> = old.relations.iter()
            .map(|r| (r.subject_index, r.target_index, r.kind_name.as_str()))
            .collect();

        let new_relations: HashSet<(u32, u32, &str)> = new.relations.iter()
            .map(|r| (r.subject_index, r.target_index, r.kind_name.as_str()))
            .collect();

        for rel in &new.relations {
            if !old_relations.contains(&(rel.subject_index, rel.target_index, rel.kind_name.as_str())) {
                diff.added_relations.push(rel.clone());
            }
        }

        for rel in &old.relations {
            if !new_relations.contains(&(rel.subject_index, rel.target_index, rel.kind_name.as_str())) {
                diff.removed_relations.push(rel.clone());
            }
        }

        // Resources — by type name, the same byte-level delta as components.
        let old_resources: HashMap<&str, &ResourceSnapshot> =
            old.resources.iter().map(|r| (r.type_name.as_str(), r)).collect();
        for res in &new.resources {
            if old_resources.get(res.type_name.as_str()).is_none_or(|o| o.data != res.data || o.format != res.format) {
                diff.changed_resources.push(res.clone());
            }
        }
        for res in &old.resources {
            if !new.resources.iter().any(|r| r.type_name == res.type_name) {
                diff.removed_resources.push(res.type_name.clone());
            }
        }

        Ok(diff)
    }

    /// Apply a diff to a base snapshot, producing a new snapshot.
    ///
    /// This is a snapshot-level operation: it does not require direct access to `World`.
    /// The result can be saved or restored via `restore()`.
    pub fn apply_diff_to_snapshot(
        base: &WorldSnapshot,
        diff: &WorldDiff,
    ) -> Result<WorldSnapshot, SerializationError> {
        // The diff embeds the same versioned wire structs as the base snapshot, so
        // its wire version must match the base's. Mismatched versions would splice
        // structs of a different shape — reject loudly rather than corrupt (§0.2a).
        // (Previously `WorldDiff::version` was serialized but never checked.)
        if diff.version != base.version {
            return Err(SerializationError::VersionMismatch {
                expected: base.version,
                found:    diff.version,
            });
        }

        let mut result = base.clone();

        // Remove entities
        for idx in &diff.removed_entities {
            result.entities.retain(|e| e.original_index != *idx);
        }

        // Remove relations
        for rel in &diff.removed_relations {
            result.relations.retain(|r| {
                !(r.subject_index == rel.subject_index
                    && r.target_index == rel.target_index
                    && r.kind_name == rel.kind_name)
            });
        }

        // Remove components
        for (entity_idx, type_names) in &diff.removed_components {
            if let Some(entity) = result.entities.iter_mut().find(|e| e.original_index == *entity_idx) {
                entity.components.retain(|c| !type_names.contains(&c.type_name));
            }
        }

        // Add entities
        let max_index = result.entities.iter()
            .map(|e| e.original_index)
            .max()
            .unwrap_or(0);

        for (i, entity_snap) in diff.added_entities.iter().enumerate() {
            let mut snap = entity_snap.clone();
            // Assign a new index if it conflicts
            if result.entities.iter().any(|e| e.original_index == snap.original_index) {
                snap.original_index = max_index + 1 + i as u32;
            }
            result.entities.push(snap);
        }

        // Add components to existing entities
        for (entity_idx, components) in &diff.added_components {
            if let Some(entity) = result.entities.iter_mut().find(|e| e.original_index == *entity_idx) {
                entity.components.extend(components.clone());
            }
        }

        // Apply modified components (byte-level delta) — replace old versions with new
        for (entity_idx, components) in &diff.modified_components {
            if let Some(entity) = result.entities.iter_mut().find(|e| e.original_index == *entity_idx) {
                for new_comp in components {
                    if let Some(old) = entity.components.iter_mut().find(|c| c.type_name == new_comp.type_name) {
                        old.data = new_comp.data.clone();
                        old.format = new_comp.format;
                    }
                }
            }
        }

        // Add relations
        result.relations.extend(diff.added_relations.clone());

        // Resources
        result.resources.retain(|r| !diff.removed_resources.contains(&r.type_name));
        for res in &diff.changed_resources {
            match result.resources.iter_mut().find(|r| r.type_name == res.type_name) {
                Some(old) => *old = res.clone(),
                None => result.resources.push(res.clone()),
            }
        }
        result.resources.sort_by(|a, b| a.type_name.cmp(&b.type_name));

        Ok(result)
    }

    // ── Saving to disk ────────────────────────────────────────

    /// Atomically write `data` to `path` (§0.2a hygiene).
    ///
    /// A plain `fs::write` truncates the target first, so a crash or interrupt
    /// mid-write leaves a corrupt (partial) save. Instead write to a sibling
    /// `.tmp` file, flush it, then rename over the target — a reader never
    /// observes a half-written file, and rename is atomic within one directory.
    fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        // std::fs::rename replaces an existing destination atomically on both
        // Unix and Windows (MOVEFILE_REPLACE_EXISTING).
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp); // don't leak the temp on failure
            return Err(e);
        }
        Ok(())
    }

    /// Save a snapshot to a file in the given format.
    pub fn write_to_file(
        path:   &Path,
        snap:   &WorldSnapshot,
        format: SaveFormat,
    ) -> Result<(), SerializationError> {
        let data = match format {
            SaveFormat::Json => snap.to_json()?,
            SaveFormat::Binary => snap.to_binary()?,
        };
        Self::atomic_write(path, &data)?;
        Ok(())
    }

    /// Read a snapshot from a file, auto-detecting the format by extension.
    ///
    /// Supported extensions:
    /// - `.json` → the text document
    /// - `.bin` → the binary form
    pub fn read_from_file(path: &Path) -> Result<WorldSnapshot, SerializationError> {
        let data = std::fs::read(path)?;
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("json");

        let mut snap = match ext {
            "json" => WorldSnapshot::from_json(&data)?,
            "bin" => WorldSnapshot::from_binary(&data)?,
            _ => {
                // Try the text document, then the binary form
                if let Ok(snap) = WorldSnapshot::from_json(&data) {
                    snap
                } else if let Ok(snap) = WorldSnapshot::from_binary(&data) {
                    snap
                } else {
                    return Err(SerializationError::Migration(format!(
                        "unknown file extension '{ext}' and couldn't detect format"
                    )));
                }
            }
        };

        // Return a current-version snapshot for callers that inspect or re-save it
        // WITHOUT restoring (restore migrates internally, so a restore-only caller
        // does not depend on this). `migrate()` is idempotent — a current-version
        // read is a no-op — and is the single migration routine shared with restore.
        snap.migrate().map_err(SerializationError::Migration)?;
        Ok(snap)
    }

    /// Save a diff to a file (always in binary format).
    pub fn write_diff_to_file(path: &Path, diff: &WorldDiff) -> Result<(), SerializationError> {
        let data = diff.to_binary()?;
        Self::atomic_write(path, &data)?;
        Ok(())
    }

    /// Read a diff from a file.
    pub fn read_diff_from_file(path: &Path) -> Result<WorldDiff, SerializationError> {
        let data = std::fs::read(path)?;
        let diff = WorldDiff::from_binary(&data)?;
        Ok(diff)
    }

    // ── Prefab export ─────────────────────────────────────────────

    /// Create a [`PrefabManifest`] from a single entity.
    ///
    /// Only components registered via `world.register_component_serde::<T>()` are serialized.
    /// Relation components are skipped; ZST markers are saved by presence (`value: null`) and
    /// re-inserted on instantiate.
    pub fn entity_to_prefab(
        world: &World,
        entity: Entity,
    ) -> Result<PrefabManifest, SerializationError> {
        Self::entity_to_prefab_with(world, entity, &mut apex_core::NoContext)
    }

    /// `entity_to_prefab` with a **(de)serialization context** (TD-44): components with external references
    /// resolve them into the prefab through `ctx` — consistent with snapshots. Plain components ignore `ctx`.
    pub fn entity_to_prefab_with(
        world: &World,
        entity: Entity,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<PrefabManifest, SerializationError> {
        let location = world
            .entity_allocator()
            .get_location(entity)
            .ok_or(SerializationError::EntityNotFound { index: entity.index() })?;

        let arch = &world.archetypes()[location.archetype_id.as_usize()];
        let mut components = Vec::new();

        for col in arch.columns() {
            let cid: apex_core::component::ComponentId = col.id();
            let info = match world.registry().get_info(cid) {
                Some(i) => i,
                None => continue,
            };

            // Components without serde are skipped
            let serde_fns = match &info.serde {
                Some(s) => s,
                None => continue,
            };

            // ZST marker (Folder/Model/Hidden/…): no data, but presence is meaningful — write `null`
            // so that instantiate re-inserts the component (its deserialize_fn produces a unit from `null`).
            // Previously `continue` silently lost markers on the prefab/capture path (as in snapshots).
            if info.size == 0 {
                components.push(PrefabComponent {
                    type_name: info.name.to_string(),
                    value: serde_json::Value::Null,
                });
                continue;
            }

            // Serialize the component's raw data into bytes through the context (TD-44) — consistent with snapshots.
            let raw_bytes =
                unsafe { (serde_fns.serialize_fn)(col.get_raw_ptr(location.row as usize), ctx) }
                    .map_err(|e| SerializationError::SerializeFailed {
                        type_name: info.name.to_string(),
                        reason: e.to_string(),
                    })?;

            // Parse the JSON bytes into a serde_json::Value
            let json_value: serde_json::Value = serde_json::from_slice(&raw_bytes)?;

            components.push(PrefabComponent {
                type_name: info.name.to_string(),
                value: json_value,
            });
        }

        Ok(PrefabManifest {
            version: PrefabManifest::CURRENT_VERSION,
            name: format!("entity_{}", entity.index()),
            components,
            children: Vec::new(),
        })
    }

    /// Create a [`PrefabManifest`] from an entity and its entire hierarchy of children.
    ///
    /// Recursively traverses children through the `ChildOf` relation, creating
    /// nested `PrefabChild` records.
    pub fn hierarchy_to_prefab(
        world: &World,
        root: Entity,
    ) -> Result<PrefabManifest, SerializationError> {
        Self::hierarchy_to_prefab_with(world, root, &mut apex_core::NoContext)
    }

    /// `hierarchy_to_prefab` with a **(de)serialization context** (TD-44): the context is threaded through
    /// the entire hierarchy (each node — via [`entity_to_prefab_with`](Self::entity_to_prefab_with)).
    pub fn hierarchy_to_prefab_with(
        world: &World,
        root: Entity,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<PrefabManifest, SerializationError> {
        let mut manifest = Self::entity_to_prefab_with(world, root, ctx)?;

        // Recursively collect children as INLINE (embedded) subtrees — this makes the prefab self-contained:
        // a single file instantiates without preloading sub-prefabs (previously only the child's name was
        // stored, and the sub-manifest itself was lost ⇒ `instantiate` failed with `SubPrefabNotFound`).
        let children: Vec<Entity> = world.targets_of(ChildOf, root).collect();
        for child in children {
            let child_manifest = Self::hierarchy_to_prefab_with(world, child, ctx)?;
            manifest.children.push(PrefabChild::Inline(child_manifest));
        }

        Ok(manifest)
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::prelude::*;
    use serde::{Deserialize, Serialize};
    

    #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
    struct Position { x: f32, y: f32 }

    #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
    struct Health { current: f32, max: f32 }

    #[derive(Component)]
    #[allow(dead_code)]
    struct RenderHandle(u64);

    /// Zero-sized marker (like the editor's `Folder`/`Model`/`Hidden`): presence is the only state.
    #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Marker;

    /// The same marker after it GREW a field: bare presence means its defaults.
    #[derive(Component, Clone, Copy, Debug, Serialize, PartialEq)]
    struct GrownMarker {
        strength: f32,
    }

    impl<'de> Deserialize<'de> for GrownMarker {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            struct Fields {
                strength: f32,
            }
            Ok(Option::<Fields>::deserialize(d)?
                .map_or(GrownMarker { strength: 1.0 }, |f| GrownMarker { strength: f.strength }))
        }
    }

    /// **A document that stored a marker by PRESENCE still opens after the marker grows a field**
    /// (ADR-015). The artifact is the real one: a snapshot taken while the type was zero-sized,
    /// restored into a world where the same name is a sized type that reads `null` as its defaults.
    /// The control restores a written value, so the gate cannot pass by defaulting everything.
    #[test]
    fn a_marker_that_grew_a_field_reads_its_presence_only_documents() {
        let mut world = World::new();
        world.register_component_serde_json::<Marker>();
        let e = world.spawn((Marker,));
        let mut snap = WorldSerializer::snapshot(&world).unwrap();
        let grown = std::any::type_name::<GrownMarker>().to_string();
        let mut renamed = 0;
        for es in &mut snap.entities {
            for c in &mut es.components {
                if c.type_name == std::any::type_name::<Marker>() {
                    assert!(c.data.is_empty(), "a zero-sized marker is written as presence only");
                    c.type_name = grown.clone();
                    renamed += 1;
                }
            }
        }
        assert_eq!(renamed, 1, "the snapshot must hold the marker to rename");

        let mut w2 = World::new();
        w2.register_component_serde_json::<GrownMarker>();
        let map = WorldSerializer::restore(&mut w2, &snap)
            .unwrap_or_else(|err| panic!("a presence-only document must open: {err}"));
        assert_eq!(
            w2.get::<GrownMarker>(map[&e.index()]).copied(),
            Some(GrownMarker { strength: 1.0 }),
            "bare presence reads as the grown marker's defaults"
        );

        // Control: a value that was written is the value that comes back.
        let mut w3 = World::new();
        w3.register_component_serde_json::<GrownMarker>();
        let written = w3.spawn((GrownMarker { strength: 0.25 },));
        let snap3 = WorldSerializer::snapshot(&w3).unwrap();
        let mut w4 = World::new();
        w4.register_component_serde_json::<GrownMarker>();
        let map4 = WorldSerializer::restore(&mut w4, &snap3).unwrap();
        assert_eq!(
            w4.get::<GrownMarker>(map4[&written.index()]).map(|m| m.strength),
            Some(0.25)
        );
    }

    #[test]
    fn zst_marker_survives_snapshot_restore() {
        let mut world = World::new();
        world.register_component_serde_json::<Marker>();
        world.register_component_serde_json::<Position>();
        let e = world.spawn((Marker, Position { x: 1.0, y: 2.0 }));

        let snap = WorldSerializer::snapshot(&world).unwrap();
        let mut w2 = World::new();
        w2.register_component_serde_json::<Marker>();
        w2.register_component_serde_json::<Position>();
        let map = WorldSerializer::restore(&mut w2, &snap).unwrap();

        let restored = map[&e.index()];
        assert!(
            w2.get::<Marker>(restored).is_some(),
            "a zero-sized marker component must survive snapshot/restore by presence"
        );
        assert_eq!(w2.get::<Position>(restored).map(|p| p.x), Some(1.0));
    }

    /// §0.2a (E8): if a component's type is registered on the restore side but
    /// WITHOUT serde functions (e.g. version skew), its snapshot bytes are
    /// dropped and the entity comes back missing it. The drop is loud — routed
    /// through the world's `ErrorHandler` (§0.2a) rather than silent; here we
    /// pin the data-loss contract — restore still succeeds, serde-backed
    /// components come back, the serde-less one is absent.
    #[test]
    fn restore_drops_component_registered_without_serde() {
        let mut world = World::new();
        world.register_component_serde_json::<Position>();
        world.register_component_serde_json::<Health>();
        let e = world.spawn((
            Position { x: 5.0, y: 6.0 },
            Health { current: 50.0, max: 100.0 },
        ));
        let snap = WorldSerializer::snapshot(&world).unwrap();

        // Restore side: Position has serde, Health is registered as a plain
        // component (no serde) — the E8 branch.
        let mut w2 = World::new();
        w2.register_component_serde_json::<Position>();
        w2.register_component::<Health>();
        let map = WorldSerializer::restore(&mut w2, &snap).unwrap();

        let restored = map[&e.index()];
        assert_eq!(
            w2.get::<Position>(restored),
            Some(&Position { x: 5.0, y: 6.0 }),
            "serde-backed component must restore",
        );
        assert!(
            w2.get::<Health>(restored).is_none(),
            "component registered without serde must be dropped on restore (E8)",
        );
    }

    fn setup_world() -> World {
        let mut world = World::new();
        world.register_component::<RenderHandle>();
        world.register_component_serde_json::<Position>();
        world.register_component_serde_json::<Health>();

        let e1 = world.spawn((
            Position { x: 10.0, y: 20.0 },
            Health { current: 100.0, max: 100.0 },
        ));
        world.insert(e1, RenderHandle(42));

        let e2 = world.spawn((
            Position { x: 30.0, y: 40.0 },
        ));

        world.add_relation(e2, apex_core::relations::ChildOf, e1);

        world
    }

    #[test]
    fn snapshot_restore_json_roundtrip() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();
        let json = snap.to_json().unwrap();

        let mut restored_world = World::new();
        restored_world.register_component::<RenderHandle>();
        restored_world.register_component_serde_json::<Position>();
        restored_world.register_component_serde_json::<Health>();

        // Register ChildOf so that restore finds the kind
        let p = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        restored_world.add_relation(c, apex_core::relations::ChildOf, p);

        let restored_snap = WorldSnapshot::from_json(&json).unwrap();
        let entity_map = WorldSerializer::restore(&mut restored_world, &restored_snap).unwrap();

        assert!(!entity_map.is_empty());

        // Verify that Position was restored for the first entity
        let new_e1 = entity_map[&0u32]; // original_index of the first created entity
        let pos = restored_world.get::<Position>(new_e1).unwrap();
        assert!((pos.x - 10.0).abs() < 1e-6);
        assert!((pos.y - 20.0).abs() < 1e-6);
    }

    #[test]
    fn snapshot_binary_roundtrip() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();
        let binary = snap.to_binary().unwrap();

        let mut restored_world = World::new();
        restored_world.register_component::<RenderHandle>();
        restored_world.register_component_serde_json::<Position>();
        restored_world.register_component_serde_json::<Health>();

        let p = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        restored_world.add_relation(c, apex_core::relations::ChildOf, p);

        let restored_snap = WorldSnapshot::from_binary(&binary).unwrap();
        let entity_map = WorldSerializer::restore(&mut restored_world, &restored_snap).unwrap();

        assert!(!entity_map.is_empty());
        let new_e1 = entity_map[&0u32];
        let pos = restored_world.get::<Position>(new_e1).unwrap();
        assert!((pos.x - 10.0).abs() < 1e-6);
    }

    #[test]
    fn binary_smaller_than_json() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();
        let json_size = snap.to_json().unwrap().len();
        let binary_size = snap.to_binary().unwrap().len();

        assert!(binary_size < json_size,
            "binary={} should be < json={}", binary_size, json_size);
    }

    #[test]
    fn diff_add_entity() {
        let mut world = setup_world();

        // Old snapshot
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Add an entity
        let _e3 = world.spawn((
            Position { x: 50.0, y: 60.0 },
            Health { current: 50.0, max: 50.0 },
        ));

        // Compute the diff
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();

        assert_eq!(diff.added_entities.len(), 1);
        assert!(diff.removed_entities.is_empty());
    }

    #[test]
    fn diff_remove_entity() {
        let mut world = World::new();
        world.register_component_serde::<Position>();

        // Spawn entities, remember the index
        let e1 = world.spawn((Position { x: 1.0, y: 2.0 },));
        let e1_idx = e1.index();
        let _e2 = world.spawn((Position { x: 3.0, y: 4.0 },));

        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Remove e1
        world.despawn(e1);

        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert_eq!(diff.removed_entities, vec![e1_idx]);
    }

    #[test]
    fn write_read_file() {
        
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();

        let dir = apex_test_dir::TestDir::new("apex_serialization");

        // JSON
        let json_path = dir.join("test_save.json");
        WorldSerializer::write_to_file(&json_path, &snap, SaveFormat::Json).unwrap();
        let loaded = WorldSerializer::read_from_file(&json_path).unwrap();
        assert_eq!(loaded.entities.len(), snap.entities.len());

        // Binary
        let bin_path = dir.join("test_save.bin");
        WorldSerializer::write_to_file(&bin_path, &snap, SaveFormat::Binary).unwrap();
        let loaded_bin = WorldSerializer::read_from_file(&bin_path).unwrap();
        assert_eq!(loaded_bin.entities.len(), snap.entities.len());

        // The binary file must be smaller
        let json_meta = std::fs::metadata(&json_path).unwrap();
        let bin_meta = std::fs::metadata(&bin_path).unwrap();
        assert!(bin_meta.len() < json_meta.len(),
            "bin={} should be < json={}", bin_meta.len(), json_meta.len());
    }

    /// Atomic save: replacing an existing file works and leaves no `.tmp`
    /// sibling behind (the write goes through temp+rename).
    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();

        let dir = apex_test_dir::TestDir::new("apex_serialization_atomic");
        let path = dir.join("save.json");

        // Pre-existing content that must be atomically replaced.
        std::fs::write(&path, b"STALE PARTIAL DATA").unwrap();
        WorldSerializer::write_to_file(&path, &snap, SaveFormat::Json).unwrap();

        let loaded = WorldSerializer::read_from_file(&path).unwrap();
        assert_eq!(loaded.entities.len(), snap.entities.len());
        assert!(
            !dir.join("save.json.tmp").exists(),
            "atomic write must not leave a .tmp file behind on success"
        );
    }

    /// §0.2a (E7): versioning is centralised on the load path — read_from_file
    /// runs migrate(), so an older snapshot comes back at CURRENT_VERSION and is
    /// directly restorable. Previously migrate() never ran here and the loaded
    /// snapshot kept its stale version, only to be rejected later by restore.
    #[test]
    fn read_from_file_runs_migrate_on_load() {
        let dir = apex_test_dir::TestDir::new("apex_serialization_migrate");
        let path = dir.join("old.json");

        // A v0 document (the inline form, written before versions existed in any other shape).
        std::fs::write(&path, br#"{"version":0,"tick":0,"entities":[],"relations":[]}"#).unwrap();

        let loaded = WorldSerializer::read_from_file(&path).unwrap();
        assert_eq!(
            loaded.version,
            WorldSnapshot::CURRENT_VERSION,
            "read_from_file must migrate an older snapshot to the current version on load"
        );
    }

    #[test]
    fn diff_apply_roundtrip() {
        let mut world = setup_world();
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Modify the world
        let _e3 = world.spawn((
            Position { x: 100.0, y: 200.0 },
        ));

        // Diff
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert_eq!(diff.added_entities.len(), 1);

        // Save the diff and load it back
        let diff_bytes = diff.to_binary().unwrap();
        let loaded_diff = WorldDiff::from_binary(&diff_bytes).unwrap();
        assert_eq!(loaded_diff.added_entities.len(), 1);
    }

    #[test]
    fn diff_byte_level_delta_modified_component() {
        

        let mut world = setup_world();
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Find the entity with Health by iterating over archetypes
        let health_id = world.registry().get_id::<Health>().unwrap();
        let e1 = *world.archetypes().iter()
            .filter(|a| a.column_index(health_id).is_some())
            .flat_map(|a| a.entities().iter())
            .next()
            .unwrap();
        *world.get_mut::<Health>(e1).unwrap() = Health { current: 50.0, max: 100.0 };

        // Diff — the component must land in modified_components, NOT in added_components
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert!(diff.added_entities.is_empty(), "no new entities");
        assert!(diff.added_components.is_empty(), "no added components");
        assert_eq!(diff.modified_components.len(), 1, "one entity with a modified component");
        assert_eq!(diff.modified_components[0].1.len(), 1, "one modified component");
        assert_eq!(diff.modified_components[0].1[0].type_name, "apex_serialization::serializer::tests::Health");

        // Apply the diff to the base snapshot — the data must update
        let new_snap = WorldSerializer::apply_diff_to_snapshot(&old_snap, &diff).unwrap();
        let health_snap = new_snap.entities.iter()
            .find(|e| e.original_index == diff.modified_components[0].0)
            .and_then(|e| e.components.iter().find(|c| c.type_name == "apex_serialization::serializer::tests::Health"))
            .unwrap();
        // Verify that the data changed: old data (100.0) → new (50.0)
        let health_str = String::from_utf8_lossy(&health_snap.data);
        assert!(health_str.contains("50.0"), "Health.current must be 50.0, got: {health_str}");
    }

    #[test]
    fn diff_unchanged_component_excluded() {
        let world = setup_world();
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Change nothing — the diff must be empty (all components match)
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert!(diff.is_empty(), "diff must be empty for an unchanged world");
        assert!(diff.modified_components.is_empty(), "no modified components");
    }

    /// F7: `#[derive(Component)]` and `#[derive(Bundle)]` work on GENERIC types
    /// (impl uses split_for_impl; the linkme registrar is dropped for generics,
    /// which register lazily on first use of a concrete substitution).
    #[test]
    fn f7_generic_derive_component_and_bundle() {
        #[derive(Component, Clone, Copy, Debug, PartialEq)]
        struct Wrapper<T: Send + Sync + 'static>(T);

        #[derive(Component, Clone, Copy, Debug, PartialEq)]
        struct Tag;

        #[derive(Bundle)]
        struct GenBundle<T: Send + Sync + 'static> {
            w: Wrapper<T>,
            t: Tag,
        }

        let mut world = World::new();
        let e = world.spawn(GenBundle {
            w: Wrapper(42u32),
            t: Tag,
        });
        assert_eq!(world.get::<Wrapper<u32>>(e), Some(&Wrapper(42u32)));
        assert!(world.get::<Tag>(e).is_some());

        // A different substitution is a distinct component type.
        let e2 = world.spawn((Wrapper(1.5f32),));
        assert_eq!(world.get::<Wrapper<f32>>(e2), Some(&Wrapper(1.5f32)));
        assert!(world.get::<Wrapper<u32>>(e2).is_none());
    }

    /// A float read from a text document is the float that was written — to the last bit, however
    /// many saves and loads it goes through. Found re-saving a real scene (TD-608): a transform
    /// coordinate stored as `5.5955240441107883e-14` (`0x1.f7fffffffffffp-45`, the correct shortest
    /// spelling) was read one ulp lower, because `serde_json` parses floats approximately unless its
    /// `float_roundtrip` feature is on, and the next save wrote the drifted value. The values are
    /// the drifting one and extremes of both widths.
    #[test]
    fn a_float_survives_the_text_document_to_the_last_bit() {
        #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize)]
        struct Precise {
            wide: [f64; 6],
            narrow: [f32; 3],
        }
        let written = Precise {
            wide: [
                f64::from_bits(0x3d2f_7fff_ffff_ffff), // 5.5955240441107883e-14
                0.1,
                1.0 / 3.0,
                f64::MIN_POSITIVE,
                f64::MAX,
                -2.2250738585072014e-308,
            ],
            narrow: [0.1, f32::MIN_POSITIVE, 16_777_217.0],
        };
        assert_eq!(written.wide[0].to_string(), "0.000000000000055955240441107883");

        let mut world = World::new();
        world.register_component_serde_json::<Precise>();
        world.spawn((written,));
        let mut bytes = WorldSerializer::snapshot(&world).unwrap().to_json().unwrap();
        for _ in 0..3 {
            let mut w2 = World::new();
            w2.register_component_serde_json::<Precise>();
            let map = WorldSerializer::restore(&mut w2, &WorldSnapshot::from_json(&bytes).unwrap()).unwrap();
            let read = *w2.get::<Precise>(*map.values().next().unwrap()).unwrap();
            for (a, b) in written.wide.iter().zip(read.wide) {
                assert_eq!(a.to_bits(), b.to_bits(), "f64 {a:e} came back as {b:e}");
            }
            for (a, b) in written.narrow.iter().zip(read.narrow) {
                assert_eq!(a.to_bits(), b.to_bits(), "f32 {a:e} came back as {b:e}");
            }
            bytes = WorldSerializer::snapshot(&w2).unwrap().to_json().unwrap();
        }
    }

    /// E7: a resource opted into serde survives snapshot/restore into a fresh
    /// world; a non-registered resource is simply absent (no panic).
    #[test]
    fn e7_resource_survives_snapshot_restore() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Config {
            level: u32,
            name: String,
        }

        let mut world = World::new();
        world.register_resource_serde::<Config>();
        world.insert_resource(Config {
            level: 7,
            name: "prod".into(),
        });
        let snap = WorldSerializer::snapshot(&world).unwrap();
        assert_eq!(snap.resources.len(), 1, "resource included in snapshot");

        let mut w2 = World::new();
        w2.register_resource_serde::<Config>();
        WorldSerializer::restore(&mut w2, &snap).unwrap();
        assert_eq!(
            w2.try_resource::<Config>(),
            Some(&Config {
                level: 7,
                name: "prod".into()
            })
        );
    }

    /// A resource that changed between two snapshots rides the diff, and applying the diff yields
    /// the new value. Before wire v4 a diff carried no resources: the incremental save kept the
    /// OLD value of every changed resource, and a removed one came back.
    #[test]
    fn a_changed_or_removed_resource_rides_the_diff() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Config {
            level: u32,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Session(u64);

        let mut world = World::new();
        world.register_resource_serde_json::<Config>();
        world.register_resource_serde::<Session>();
        world.insert_resource(Config { level: 1 });
        world.insert_resource(Session(9));
        let old = WorldSerializer::snapshot(&world).unwrap();

        world.insert_resource(Config { level: 2 });
        world.remove_resource::<Session>();
        let diff = WorldSerializer::diff(&old, &world).unwrap();
        assert_eq!(diff.changed_resources.len(), 1, "{diff:?}");
        assert_eq!(diff.removed_resources.len(), 1, "{diff:?}");

        let diff = WorldDiff::from_binary(&diff.to_binary().unwrap()).unwrap();
        let patched = WorldSerializer::apply_diff_to_snapshot(&old, &diff).unwrap();
        let mut w2 = World::new();
        w2.register_resource_serde_json::<Config>();
        w2.register_resource_serde::<Session>();
        WorldSerializer::restore(&mut w2, &patched).unwrap();
        assert_eq!(w2.try_resource::<Config>(), Some(&Config { level: 2 }));
        assert_eq!(w2.try_resource::<Session>(), None, "the removed resource stays removed");

        // An unchanged world diffs to nothing, resources included.
        let still = WorldSerializer::snapshot(&w2).unwrap();
        assert!(WorldSerializer::diff(&still, &w2).unwrap().is_empty());
    }

    /// A document written before wire v4 — `bincode` resource and binary component payloads —
    /// restores through each type's legacy reader, whatever the type is registered as today, and
    /// the next save writes the current format.
    #[test]
    fn a_v3_document_with_bincode_payloads_restores_and_saves_as_v4() {
        #[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
        struct Packed {
            a: u32,
            b: f32,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
        struct Materials {
            by_name: std::collections::BTreeMap<String, String>,
        }
        let materials = Materials { by_name: [("m1".to_string(), "{}".to_string())].into() };
        let packed = Packed { a: 7, b: 0.5 };

        let v3 = crate::legacy::InternedSnapshot {
            version:      3,
            tick:         1,
            string_table: vec![std::any::type_name::<Packed>().into(), std::any::type_name::<Materials>().into()],
            entities:     vec![crate::legacy::InternedEntity {
                original_index: 0,
                components:     vec![crate::legacy::InternedComponent {
                    name_idx: 0,
                    data:     bincode::serialize(&packed).unwrap(),
                    format:   crate::legacy::LegacyDataFormat::Binary,
                }],
            }],
            relations:    Vec::new(),
            resources:    vec![crate::legacy::InternedResource { name_idx: 1, data: bincode::serialize(&materials).unwrap() }],
        };

        for bytes in [serde_json::to_vec_pretty(&v3).unwrap(), bincode::serialize(&v3).unwrap()] {
            let snap = WorldSnapshot::from_json(&bytes).or_else(|_| WorldSnapshot::from_binary(&bytes)).unwrap();
            let mut world = World::new();
            world.register_component_serde::<Packed>();
            world.register_resource_serde_json::<Materials>();
            let map = WorldSerializer::restore(&mut world, &snap).unwrap();
            assert_eq!(world.get::<Packed>(map[&0]), Some(&packed));
            assert_eq!(world.try_resource::<Materials>(), Some(&materials));
            // The pre-pass reader of a resource gets the same value without a world.
            assert_eq!(snap.resources[0].decode::<Materials>().unwrap(), materials);

            let saved = WorldSerializer::snapshot(&world).unwrap();
            let text = String::from_utf8(saved.to_json().unwrap()).unwrap();
            assert!(text.contains(r#""version": 4"#), "{text}");
            assert!(text.contains(r#"{"by_name":{"m1":"{}"}}"#), "the resource is a readable value now: {text}");
        }
    }

    /// A JSON-only type: context-dependent fns exist in the one format their author wrote.
    #[derive(Component, Debug, PartialEq)]
    struct JsonOnly(u64);

    fn json_only_fns() -> apex_core::ComponentSerdeFns {
        apex_core::ComponentSerdeFns {
            serialize_fn: |ptr, _ctx| {
                let v = unsafe { &*(ptr as *const JsonOnly) };
                Ok(serde_json::to_vec(&v.0).unwrap())
            },
            deserialize_fn: |bytes, _ctx| {
                let id: u64 = serde_json::from_slice(bytes).map_err(|e| {
                    apex_core::component::ComponentSerdeError::DeserializationFailed(e.to_string())
                })?;
                Ok(id.to_le_bytes().to_vec())
            },
            format: SERDE_FORMAT_JSON,
        }
    }

    /// A payload in a format its type has NO reader for is named as such (ADR-018). Since ADR-024
    /// a plain registration reads both formats, so the mismatch is left to the types that exist in
    /// one: context-dependent fns.
    #[test]
    fn a_payload_of_the_other_format_names_both_formats() {
        let mut world = World::new();
        world.register_component_serde_with::<JsonOnly>(json_only_fns());
        world.spawn((JsonOnly(7),));
        let mut snap = WorldSerializer::snapshot(&world).unwrap();
        assert_eq!(snap.entities[0].components[0].format, DataFormat::Json);
        // The same bytes claimed to be binary, of either binary kind: the type reads neither.
        for (claimed, named_as) in [(DataFormat::Binary, "Binary"), (DataFormat::Named, "Named")] {
            snap.entities[0].components[0].format = claimed;

            let mut w2 = World::new();
            w2.register_component_serde_with::<JsonOnly>(json_only_fns());
            let err = WorldSerializer::restore(&mut w2, &snap).unwrap_err().to_string();
            assert!(err.contains(named_as) && err.contains("json") && err.contains("JsonOnly"), "{err}");
        }
    }

    /// The same type cannot be WRITTEN in the format it does not have either: the snapshot names
    /// it instead of putting the registered format into a snapshot that was asked for the other.
    #[test]
    fn a_type_with_one_format_is_refused_in_the_other() {
        let mut world = World::new();
        world.register_component_serde_with::<JsonOnly>(json_only_fns());
        world.register_component_serde::<Position>();
        world.spawn((JsonOnly(7), Position { x: 1.0, y: 2.0 }));

        for (payloads, wanted) in [(Payloads::Binary, "binary"), (Payloads::Named, "msgpack")] {
            let opts = SnapshotOptions { payloads, ..Default::default() };
            let err = WorldSerializer::snapshot_opts(&world, &mut apex_core::NoContext, &opts)
                .unwrap_err()
                .to_string();
            assert!(err.contains("JsonOnly") && err.contains(wanted) && err.contains("json"), "{err}");
        }

        // In its own format the snapshot is written, the plain type following the request.
        let text = SnapshotOptions { payloads: Payloads::Text, ..Default::default() };
        let snap = WorldSerializer::snapshot_opts(&world, &mut apex_core::NoContext, &text).unwrap();
        assert!(snap.entities[0].components.iter().all(|c| c.format == DataFormat::Json));
    }

    /// ADR-024, ADR-025: the payload format is chosen when the snapshot is written, not when the
    /// type is registered — and a world registered either way reads a payload of any of the three
    /// formats, through both container forms.
    #[test]
    fn a_snapshot_is_written_in_the_format_the_caller_names() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Level(u32);

        fn build(json: bool) -> World {
            let mut world = World::new();
            if json {
                world.register_component_serde_json::<Position>();
                world.register_component_serde_json::<Marker>();
                world.register_resource_serde_json::<Level>();
            } else {
                world.register_component_serde::<Position>();
                world.register_component_serde::<Marker>();
                world.register_resource_serde::<Level>();
            }
            world
        }
        let position = Position { x: 1.5, y: -2.0 };

        for registered_json in [true, false] {
            let mut world = build(registered_json);
            let e = world.spawn((position, Marker));
            world.insert_resource(Level(3));
            let as_registered = WorldSerializer::snapshot(&world).unwrap();

            let registered = if registered_json { DataFormat::Json } else { DataFormat::Binary };
            for (payloads, format) in [
                (Payloads::Text, DataFormat::Json),
                (Payloads::Binary, DataFormat::Binary),
                (Payloads::Named, DataFormat::Named),
            ] {
                let opts = SnapshotOptions { payloads, ..Default::default() };
                let snap = WorldSerializer::snapshot_opts(&world, &mut apex_core::NoContext, &opts).unwrap();
                let payload = snap.entities[0]
                    .components
                    .iter()
                    .find(|c| c.type_name.ends_with("Position"))
                    .unwrap();
                assert_eq!(payload.format, format);
                assert_eq!(snap.resources[0].format, format);
                // The bytes are the bytes of that format's one road.
                let expected = match format {
                    DataFormat::Json => apex_core::text::to_vec(&position).unwrap(),
                    DataFormat::Named => apex_core::named::to_vec(&position).unwrap(),
                    _ => apex_core::binary::to_vec(&position).unwrap(),
                };
                assert_eq!(payload.data, expected);
                // Asking for the registered format is the plain snapshot.
                if format == registered {
                    assert_eq!(snap, as_registered);
                } else {
                    assert_ne!(snap, as_registered);
                }

                let forms = [
                    WorldSnapshot::from_json(&snap.to_json().unwrap()).unwrap(),
                    WorldSnapshot::from_binary(&snap.to_binary().unwrap()).unwrap(),
                ];
                for form in &forms {
                    for reader_json in [true, false] {
                        let mut w2 = build(reader_json);
                        let map = WorldSerializer::restore(&mut w2, form).unwrap();
                        let restored = map[&e.index()];
                        assert_eq!(w2.get::<Position>(restored), Some(&position));
                        assert!(w2.get::<Marker>(restored).is_some(), "a marker is its presence");
                        assert_eq!(w2.try_resource::<Level>().map(|l| l.0), Some(3));
                    }
                }
            }
        }
    }

    /// A resource whose payload does not read fails the restore, as a component's does — it is
    /// not dropped with a line in the log, leaving the world with whatever the resource held.
    #[test]
    fn a_resource_that_does_not_read_fails_the_restore() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Level(u32);
        let mut world = World::new();
        world.register_resource_serde_json::<Level>();
        world.insert_resource(Level(3));
        let mut snap = WorldSerializer::snapshot(&world).unwrap();
        snap.resources[0].data = br#""three""#.to_vec();

        let mut w2 = World::new();
        w2.register_resource_serde_json::<Level>();
        w2.insert_resource(Level(1));
        let err = WorldSerializer::restore(&mut w2, &snap).unwrap_err();
        match &err {
            SerializationError::DeserializeFailed { type_name, .. } => {
                assert!(type_name.ends_with("Level"), "{type_name}")
            }
            other => panic!("{other}"),
        }
        // A resource the world does not know is still skipped: a type removed by a newer build
        // must not make its old documents unreadable.
        let mut w3 = World::new();
        snap.resources[0].data = b"3".to_vec();
        WorldSerializer::restore(&mut w3, &snap).unwrap();
        assert!(w3.try_resource::<Level>().is_none());
    }

    // ── A binary save (ADR-025) ──────────────────────────────────

    /// One type as two releases know it. `then` wrote the save; `now` reads it — with a field
    /// added, a field removed, the rest moved, and a variant put in front of the others.
    mod then {
        use super::*;

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        pub enum Order {
            Idle,
            Go(u32),
        }
        #[derive(Component, Serialize, Deserialize, Debug, PartialEq)]
        pub struct Fleet {
            pub id: u32,
            pub gone: String,
            pub order: Order,
            pub t: f64,
        }
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        pub struct Clock {
            pub day: u32,
        }
    }
    mod now {
        use super::*;

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        pub enum Order {
            Wait { s: f64 },
            Idle,
            Go(u32),
        }
        #[derive(Component, Serialize, Deserialize, Debug, PartialEq)]
        pub struct Fleet {
            #[serde(default)]
            pub added: Vec<u8>,
            pub t: f64,
            pub order: Order,
            pub id: u32,
        }
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        pub struct Clock {
            #[serde(default)]
            pub hour: u8,
            pub day: u32,
        }
    }

    /// The save of `then` as `now` finds it on disk. A type keeps its name across releases; the
    /// test needs two Rust types for one, so it writes the name the reader knows.
    fn saved_then(payloads: Payloads) -> Vec<u8> {
        let mut world = World::new();
        world.register_component_serde::<then::Fleet>();
        world.register_resource_serde::<then::Clock>();
        world.spawn((then::Fleet { id: 7, gone: "x".into(), order: then::Order::Go(3), t: f64::INFINITY },));
        world.insert_resource(then::Clock { day: 12 });
        let opts = SnapshotOptions { payloads, ..Default::default() };
        let mut snap = WorldSerializer::snapshot_opts(&world, &mut apex_core::NoContext, &opts).unwrap();
        snap.entities[0].components[0].type_name = std::any::type_name::<now::Fleet>().to_string();
        snap.resources[0].type_name = std::any::type_name::<now::Clock>().to_string();
        snap.to_binary().unwrap()
    }

    /// ADR-025: a binary save whose payloads carry field names opens after its types changed —
    /// the property a save file needs and the compact payloads do not have.
    #[test]
    fn a_binary_save_with_named_payloads_is_read_after_its_types_changed() {
        let read = |payloads: Payloads| {
            let mut world = World::new();
            world.register_component_serde::<now::Fleet>();
            world.register_resource_serde::<now::Clock>();
            let snap = WorldSnapshot::from_binary(&saved_then(payloads)).unwrap();
            WorldSerializer::restore(&mut world, &snap).map(|map| (world, map))
        };
        let (world, map) = read(Payloads::Named).unwrap();
        let fleet = world.get::<now::Fleet>(*map.values().next().unwrap()).unwrap();
        assert_eq!(
            fleet,
            &now::Fleet { added: Vec::new(), t: f64::INFINITY, order: now::Order::Go(3), id: 7 }
        );
        assert_eq!(world.try_resource::<now::Clock>().map(|c| (c.hour, c.day)), Some((0, 12)));

        // The compact payloads of the same world hold positions, not names: the newer types do
        // not read them — which is why `Payloads::Binary` is not the format of a save.
        assert!(matches!(read(Payloads::Binary), Err(SerializationError::DeserializeFailed { .. })));
    }

    // ── Canonical snapshot (ADR-024) ─────────────────────────────

    /// The host's stable name of an entity.
    #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
    struct Id(u64);

    /// Entity references of every shape, next to heap data: the renumbering works on a private
    /// copy of the value, which has to be dropped as the type it is.
    #[derive(Component, Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct Squad {
        leader:  Entity,
        members: Vec<Entity>,
        name:    String,
        reserve: Entity,
    }

    impl apex_core::MapEntities for Squad {
        fn map_entities(&mut self, f: &mut dyn FnMut(Entity) -> Entity) {
            self.leader = f(self.leader);
            for m in &mut self.members {
                *m = f(*m);
            }
            self.reserve = f(self.reserve);
        }
    }

    fn register_squads(world: &mut World, reversed: bool) {
        // Registration order decides component ids, and with them the column order of an archetype.
        if reversed {
            world.register_component_serde_json::<Squad>();
            world.register_component_serde_json::<Position>();
            world.register_component_serde_json::<Id>();
        } else {
            world.register_component_serde_json::<Id>();
            world.register_component_serde_json::<Position>();
            world.register_component_serde_json::<Squad>();
        }
        world.register_map_entities::<Squad>();
    }

    /// The same five units and the same links between them, reached by a different history:
    /// `order` is the spawn order, `junk` entities are spawned first and half of them despawned.
    fn squads(order: &[u64], junk: usize, reversed: bool) -> (World, HashMap<u64, Entity>) {
        let mut world = World::new();
        register_squads(&mut world, reversed);
        let scrap: Vec<Entity> = (0..junk).map(|_| world.spawn((Position { x: 0.0, y: 0.0 },))).collect();
        for e in scrap.iter().step_by(2) {
            world.despawn(*e);
        }
        let mut by_id = HashMap::new();
        for &id in order {
            let e = world.spawn((Id(id), Position { x: id as f32 * 0.1, y: -(id as f32) }));
            by_id.insert(id, e);
        }
        for e in scrap.iter().skip(1).step_by(2) {
            world.despawn(*e);
        }
        world.insert(
            by_id[&1],
            Squad {
                leader:  by_id[&2],
                members: vec![by_id[&3], by_id[&5], by_id[&1]],
                name:    "first".into(),
                reserve: Entity::PLACEHOLDER,
            },
        );
        world.insert(
            by_id[&4],
            Squad { leader: by_id[&1], members: Vec::new(), name: "fourth".into(), reserve: by_id[&5] },
        );
        for (child, parent) in [(2, 1), (3, 1), (5, 1), (4, 2)] {
            world.add_relation(by_id[&child], apex_core::relations::ChildOf, by_id[&parent]);
        }
        (world, by_id)
    }

    fn canonical(world: &World, payloads: Payloads) -> Result<WorldSnapshot, SerializationError> {
        let key = |e: Entity| world.get::<Id>(e).map_or(u64::MAX, |id| id.0);
        let opts = SnapshotOptions { canonical: Some(&key), payloads, ..Default::default() };
        WorldSerializer::snapshot_opts(world, &mut apex_core::NoContext, &opts)
    }

    /// ADR-024: a canonical snapshot is a function of the world's state, not of its history.
    #[test]
    fn equal_worlds_of_different_history_give_equal_canonical_bytes() {
        let (a, a_ids) = squads(&[1, 2, 3, 4, 5], 0, false);
        let (b, _) = squads(&[5, 3, 1, 4, 2], 7, true);

        // The control: the plain snapshots of the two worlds differ.
        assert_ne!(
            WorldSerializer::snapshot(&a).unwrap().to_json().unwrap(),
            WorldSerializer::snapshot(&b).unwrap().to_json().unwrap()
        );

        for payloads in [Payloads::AsRegistered, Payloads::Binary, Payloads::Named] {
            let (sa, sb) = (canonical(&a, payloads).unwrap(), canonical(&b, payloads).unwrap());
            assert_eq!(
                String::from_utf8(sa.to_json().unwrap()).unwrap(),
                String::from_utf8(sb.to_json().unwrap()).unwrap()
            );
            assert_eq!(sa.to_binary().unwrap(), sb.to_binary().unwrap());
        }

        // What "canonical" means, read off the snapshot.
        let snap = canonical(&b, Payloads::AsRegistered).unwrap();
        assert_eq!(snap.tick, 0);
        assert_eq!(snap.entities.iter().map(|e| e.original_index).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        let payload = |entity: usize, ty: &str| -> serde_json::Value {
            let c = snap.entities[entity].components.iter().find(|c| c.type_name.ends_with(ty)).unwrap();
            serde_json::from_slice(&c.data).unwrap()
        };
        assert_eq!(payload(0, "::Id"), serde_json::json!(1));
        assert_eq!(payload(4, "::Id"), serde_json::json!(5));
        // Unit 1's squad: leader 2 → number 1, members 3, 5, 1 → numbers 2, 4, 0, the placeholder kept.
        let squad = payload(0, "::Squad");
        assert_eq!(squad["leader"], serde_json::json!({ "index": 1, "generation": 0 }));
        assert_eq!(
            squad["members"],
            serde_json::json!([
                { "index": 2, "generation": 0 },
                { "index": 4, "generation": 0 },
                { "index": 0, "generation": 0 }
            ])
        );
        assert_eq!(squad["reserve"], serde_json::to_value(Entity::PLACEHOLDER).unwrap());
        let names: Vec<&str> = snap.entities[0].components.iter().map(|c| c.type_name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "components by type name");
        // Relations: by target, the children of one parent in the order they were attached.
        assert_eq!(
            snap.relations.iter().map(|r| (r.subject_index, r.target_index)).collect::<Vec<_>>(),
            [(1, 0), (2, 0), (4, 0), (3, 1)]
        );

        // The snapshot read the world and left it alone: the live references are the live entities.
        let live = a.get::<Squad>(a_ids[&1]).unwrap();
        assert_eq!((live.leader, live.members[1]), (a_ids[&2], a_ids[&5]));

        // Save → load → save: the restored world gives the same bytes, with its references
        // pointing at its own entities.
        let mut loaded = World::new();
        register_squads(&mut loaded, false);
        // A relation kind is part of the schema: restore finds it by name.
        loaded.relation_registry_mut().get_or_register::<apex_core::relations::ChildOf>();
        loaded.spawn((Position { x: 9.0, y: 9.0 },)); // a different allocator state again
        let map = WorldSerializer::restore(&mut loaded, &snap).unwrap();
        let (unit1, unit2) = (map[&0], map[&1]);
        assert_eq!(loaded.get::<Id>(unit2), Some(&Id(2)));
        assert_eq!(loaded.get::<Squad>(unit1).unwrap().leader, unit2);
        let keep = |e: Entity| loaded.get::<Id>(e).is_some();
        let key = |e: Entity| loaded.get::<Id>(e).unwrap().0;
        let opts = SnapshotOptions { keep: Some(&keep), canonical: Some(&key), ..Default::default() };
        let again = WorldSerializer::snapshot_opts(&loaded, &mut apex_core::NoContext, &opts).unwrap();
        assert_eq!(
            String::from_utf8(again.to_json().unwrap()).unwrap(),
            String::from_utf8(snap.to_json().unwrap()).unwrap()
        );
    }

    /// A key that names two entities cannot order them: an error, not an order that depends on
    /// which archetype came first.
    #[test]
    fn a_canonical_key_shared_by_two_entities_is_an_error() {
        let (world, ids) = squads(&[1, 2, 3, 4, 5], 0, false);
        let key = |e: Entity| if e == ids[&3] || e == ids[&5] { 77 } else { world.get::<Id>(e).unwrap().0 };
        let opts = SnapshotOptions { canonical: Some(&key), ..Default::default() };
        let err = WorldSerializer::snapshot_opts(&world, &mut apex_core::NoContext, &opts).unwrap_err();
        match err {
            SerializationError::CanonicalKeyTwice { key: 77, first, second } => {
                let mut pair = [first, second];
                pair.sort();
                let mut expected = [ids[&3].index(), ids[&5].index()];
                expected.sort();
                assert_eq!(pair, expected);
            }
            other => panic!("{other}"),
        }
    }

    /// A reference to an entity the snapshot does not hold — filtered out, or despawned and left
    /// dangling — has no canonical spelling: an error naming the component, not a raw index that
    /// differs between two equal worlds.
    #[test]
    fn a_canonical_reference_to_an_entity_outside_the_snapshot_is_an_error() {
        // Filtered out: unit 2 is the leader of unit 1's squad.
        let (world, ids) = squads(&[1, 2, 3, 4, 5], 0, false);
        let keep = |e: Entity| e != ids[&2];
        let key = |e: Entity| world.get::<Id>(e).unwrap().0;
        let opts = SnapshotOptions { keep: Some(&keep), canonical: Some(&key), ..Default::default() };
        let err = WorldSerializer::snapshot_opts(&world, &mut apex_core::NoContext, &opts).unwrap_err();
        match &err {
            SerializationError::CanonicalOutsideRef { type_name, index } => {
                assert!(type_name.ends_with("::Squad"), "{type_name}");
                assert_eq!(*index, ids[&2].index());
            }
            other => panic!("{other}"),
        }
        // The same world without the canonical option is written as before: the reference stays raw.
        let plain = SnapshotOptions { keep: Some(&keep), ..Default::default() };
        assert!(WorldSerializer::snapshot_opts(&world, &mut apex_core::NoContext, &plain).is_ok());

        // Dangling: the referenced entity is gone.
        let (mut world, ids) = squads(&[1, 2, 3, 4, 5], 0, false);
        world.despawn(ids[&5]);
        let err = canonical(&world, Payloads::AsRegistered).unwrap_err();
        assert!(matches!(err, SerializationError::CanonicalOutsideRef { .. }), "{err}");
    }

    /// E6: a component holding an `Entity` ref is remapped on restore — the ref
    /// points at the NEW entity, not the stale snapshot id.
    #[test]
    fn e6_map_entities_remaps_entity_refs_on_restore() {
        use apex_core::{Entity, MapEntities};

        #[derive(Component, Clone, Copy, Serialize, Deserialize)]
        struct Target(Entity);
        impl MapEntities for Target {
            fn map_entities(&mut self, f: &mut dyn FnMut(Entity) -> Entity) {
                self.0 = f(self.0);
            }
        }

        let mut world = World::new();
        world.register_component_serde_json::<Target>();
        world.register_map_entities::<Target>();
        let a = world.spawn(());
        let b = world.spawn(());
        world.insert(a, Target(b)); // a.target -> b
        let snap = WorldSerializer::snapshot(&world).unwrap();

        // Restore into a world whose fresh ids differ (decoys shift them).
        let mut w2 = World::new();
        w2.register_component_serde_json::<Target>();
        w2.register_map_entities::<Target>();
        for _ in 0..3 {
            w2.spawn(());
        }
        let map = WorldSerializer::restore(&mut w2, &snap).unwrap();

        let a2 = map[&a.index()];
        let b2 = map[&b.index()];
        let target = w2.get::<Target>(a2).expect("Target restored");
        assert_eq!(target.0, b2, "Entity ref must be remapped to the new b");
        assert_ne!(
            target.0, b,
            "the stale snapshot id must not survive (decoys shifted ids)"
        );
    }

    /// Relations diff (O(R) HashSet path): a removed and an added relation are
    /// both detected, and unchanged relations are excluded.
    #[test]
    fn diff_detects_added_and_removed_relations() {
        let mut world = World::new();
        world.register_component_serde_json::<Position>();
        let a = world.spawn((Position { x: 0.0, y: 0.0 },));
        let b = world.spawn((Position { x: 1.0, y: 1.0 },));
        let c = world.spawn((Position { x: 2.0, y: 2.0 },));
        world.add_relation(a, apex_core::relations::ChildOf, b);
        let old = WorldSerializer::snapshot(&world).unwrap();

        // Swap the relation target: drop a->b, add a->c.
        world.remove_relation(a, apex_core::relations::ChildOf, b);
        world.add_relation(a, apex_core::relations::ChildOf, c);
        let diff = WorldSerializer::diff(&old, &world).unwrap();

        assert_eq!(diff.removed_relations.len(), 1, "a->b must be removed");
        assert_eq!(diff.added_relations.len(), 1, "a->c must be added");
        assert_eq!(diff.removed_relations[0].target_index, b.index());
        assert_eq!(diff.added_relations[0].target_index, c.index());
    }

    #[test]
    fn restored_components_are_added_and_changed_for_a_prior_base() {
        // Picking/extract consumers gate rebuilds on Changed<T>/Added<T> with a base captured before a
        // scene load. Loading OVER a same-count scene leaves the cheap "count" signal unchanged, so
        // correctness hinges on restore making the loaded data look freshly Added+Changed against that
        // prior base (TD-52 restore-tick fix). Mirror of the picking BVH gate, minus the BVH.
        use apex_core::query::{Added, Changed, Query};

        let mut world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();

        // Long-running editor: a consumer's base sits high before the load.
        for _ in 0..10 {
            world.advance_change_tick();
        }
        let base = world.current_tick();

        // File→Open over the current scene: clear + restore (same component set ⇒ same count).
        world.clear_entities();
        WorldSerializer::restore(&mut world, &snap).unwrap();

        let changed = Query::<(Changed<Position>,)>::new_with_tick(&world, base).iter().count();
        let added = Query::<(Added<Position>,)>::new_with_tick(&world, base).iter().count();
        assert!(changed > 0, "restored components must be Changed for a base predating the load");
        assert!(added > 0, "restored components must be Added for a base predating the load");
    }

    #[test]
    fn restore_with_migration() {
        let world = setup_world();
        let mut snap = WorldSerializer::snapshot(&world).unwrap();

        // Simulate an old version
        snap.version = 1;

        // Migration (v1 → v1 — no-op, since v1 is CURRENT)
        snap.migrate().unwrap();
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);

        // Restore after migration
        let mut restored_world = World::new();
        restored_world.register_component::<RenderHandle>();
        restored_world.register_component_serde_json::<Position>();
        restored_world.register_component_serde_json::<Health>();
        let p = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        restored_world.add_relation(c, apex_core::relations::ChildOf, p);

        let result = WorldSerializer::restore(&mut restored_world, &snap);
        assert!(result.is_ok());
    }

    /// TD-44: a component holding an **external reference** (here an id remapped by the host) is
    /// (de)serialised through a host-provided [`SerdeContext`] — the mechanism a scene editor uses to
    /// turn a `Handle<Asset>` into a stable path and back. apex-ecs stays asset-agnostic: the context
    /// type lives here (== the engine/editor in production); the core only threads `&mut dyn SerdeContext`.
    #[test]
    fn context_aware_component_resolves_via_serde_context() {
        use apex_core::{ComponentSerdeFns, SerdeContext};
        use std::any::Any;

        // Host-side context: remaps the external id by an offset (stands in for Handle↔path resolution).
        struct OffsetCtx {
            offset: u64,
        }
        impl SerdeContext for OffsetCtx {
            fn as_any(&self) -> &dyn Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn Any {
                self
            }
        }

        #[derive(Component, Debug, PartialEq)]
        struct ExternalRef(u64);

        let fns = ComponentSerdeFns {
            serialize_fn: |ptr, ctx| {
                let r = unsafe { &*(ptr as *const ExternalRef) };
                let off = ctx.as_any().downcast_ref::<OffsetCtx>().map_or(0, |c| c.offset);
                Ok((r.0 + off).to_le_bytes().to_vec()) // store the host-resolved id
            },
            deserialize_fn: |bytes, ctx| {
                let off = ctx.as_any().downcast_ref::<OffsetCtx>().map_or(0, |c| c.offset);
                let stored = u64::from_le_bytes(bytes.try_into().unwrap());
                let r = ExternalRef(stored - off); // resolve back via the same context
                let size = std::mem::size_of::<ExternalRef>();
                let mut buf = vec![0u8; size];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &r as *const ExternalRef as *const u8,
                        buf.as_mut_ptr(),
                        size,
                    );
                }
                std::mem::forget(r);
                Ok(buf)
            },
            format: "raw-le-u64",
        };

        let mut world = World::new();
        world.register_component_serde_with::<ExternalRef>(fns.clone());
        let e = world.spawn((ExternalRef(7),));

        // Snapshot WITH a context that offsets by 1000 ⇒ the stored value is the resolved id (1007),
        // proving the context reached the component's serialize_fn.
        let mut sctx = OffsetCtx { offset: 1000 };
        let snap = WorldSerializer::snapshot_with(&world, &mut sctx).unwrap();
        let stored = u64::from_le_bytes(snap.entities[0].components[0].data[..8].try_into().unwrap());
        assert_eq!(stored, 1007, "serialize_fn saw the SerdeContext (resolved 7 → 1007)");

        // Restore WITH the same context ⇒ resolves back to 7 (context threaded both directions).
        let mut rworld = World::new();
        rworld.register_component_serde_with::<ExternalRef>(fns);
        let mut rctx = OffsetCtx { offset: 1000 };
        let map = WorldSerializer::restore_with(&mut rworld, &snap, &mut rctx).unwrap();
        let restored = map[&e.index()];
        assert_eq!(
            rworld.get::<ExternalRef>(restored),
            Some(&ExternalRef(7)),
            "restore_with threaded the context ⇒ external ref resolved back to 7"
        );
    }
}
