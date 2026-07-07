//! Cross-world entity transfer with reference remapping (В4 exchange protocol).
//!
//! IsolatedWorld gives *isolation* (each world owns its entities); this module
//! gives the missing half — a first-class way to **move entities between worlds
//! while remapping their `Entity` references**, so a fork can be committed back
//! without hand-rolling identity maps. Both real consumers previously did this
//! by hand (the renderer stamps a `MainEntity` back-reference; the editor keeps
//! an `EditorIdMap`); this is the shared, tested primitive underneath.
//!
//! It is built ON TOP of the snapshot machinery ([`apex_serialization`]), not a
//! second implementation: export = snapshot (optionally filtered to a subtree),
//! import = restore (which already remaps `Entity` fields via E6/`MapEntities`),
//! apply-back = merge-restore (reuse existing entities by a caller key).
//!
//! Prerequisite: the source and destination worlds must share the same serde
//! **schema** (registered component (de)serializers). Use a [`WorldRegistrar`]
//! (see [`crate::registrar`]) to register both worlds from one recipe instead of
//! duplicating registrations by hand.

use apex_core::{relations::ChildOf, Entity, World};
use apex_serialization::{RestoreEntityMap, SerializationError, WorldSerializer, WorldSnapshot};
use std::collections::HashSet;

/// Collect `roots` plus every entity reachable from them through `ChildOf`
/// (their descendant subtree). Transferring a whole subtree keeps parent/child
/// relations internally consistent — a relation whose other end was left behind
/// would be dropped on restore.
fn subtree_closure(world: &World, roots: &[Entity]) -> HashSet<u32> {
    let mut set: HashSet<u32> = HashSet::new();
    let mut stack: Vec<Entity> = roots.to_vec();
    while let Some(e) = stack.pop() {
        if !set.insert(e.index()) {
            continue; // already visited
        }
        for child in world.targets_of(ChildOf, e) {
            stack.push(child);
        }
    }
    set
}

/// Export a subtree (`roots` + their `ChildOf` descendants) of `src` as a snapshot.
///
/// Context-dependent components (asset handles, entity refs) resolve through `ctx`
/// exactly as a normal snapshot would; pass [`apex_core::NoContext`] for plain data.
pub fn export_entities(
    src:   &World,
    roots: &[Entity],
    ctx:   &mut dyn apex_core::SerdeContext,
) -> Result<WorldSnapshot, SerializationError> {
    let keep = subtree_closure(src, roots);
    WorldSerializer::snapshot_with_filter(src, ctx, &|e| keep.contains(&e.index()))
}

/// Export the ENTIRE `src` world as a snapshot (e.g. to fork a document into a
/// simulation/preview world).
pub fn export_world(
    src: &World,
    ctx: &mut dyn apex_core::SerdeContext,
) -> Result<WorldSnapshot, SerializationError> {
    WorldSerializer::snapshot_with(src, ctx)
}

/// Import `snapshot` into `dst`, spawning FRESH entities and remapping `Entity`
/// references so cross-references within the transferred set point at the new
/// entities. Returns the old-index → new-`Entity` map (the caller's bridge to
/// reconcile identity across the boundary).
pub fn import(
    dst:      &mut World,
    snapshot: &WorldSnapshot,
    ctx:      &mut dyn apex_core::SerdeContext,
) -> Result<RestoreEntityMap, SerializationError> {
    WorldSerializer::restore_with(dst, snapshot, ctx)
}

/// Apply `snapshot` back onto `dst`, **reusing** entities the caller's `resolve`
/// maps to (by an external stable key), and spawning the rest. This is the
/// golden-path apply-back: a subtree edited in a fork is committed to the source
/// without duplicating entities — component values land on the SAME entities.
/// See [`WorldSerializer::restore_merge_with`] for the merge semantics.
pub fn apply_back(
    dst:      &mut World,
    snapshot: &WorldSnapshot,
    ctx:      &mut dyn apex_core::SerdeContext,
    resolve:  &mut dyn FnMut(u32) -> Option<Entity>,
) -> Result<RestoreEntityMap, SerializationError> {
    WorldSerializer::restore_merge_with(dst, snapshot, ctx, resolve)
}

/// Transfer a subtree from `src` into `dst` in one call (export + import, fresh
/// entities, plain data). For asset-handle-bearing components, call
/// [`export_entities`] and [`import`] separately with each world's own context.
pub fn transfer_entities(
    src:   &World,
    dst:   &mut World,
    roots: &[Entity],
) -> Result<RestoreEntityMap, SerializationError> {
    let snapshot = export_entities(src, roots, &mut apex_core::NoContext)?;
    import(dst, &snapshot, &mut apex_core::NoContext)
}
