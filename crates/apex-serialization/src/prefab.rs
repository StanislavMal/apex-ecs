//! PrefabManifest — file-based prefabs (JSON format).
//!
//! Lets you describe entities and their hierarchies in JSON files,
//! load them via [`PrefabLoader`] and spawn them into a [`World`].
//!
//! # Format
//!
//! ```json
//! {
//!   "name": "Monster",
//!   "components": [
//!     { "type_name": "crate::Health",  "value": { "current": 100, "max": 100 } },
//!     { "type_name": "crate::Name",    "value": "Monster" }
//!   ],
//!   "children": [
//!     { "prefab": "Weapon", "overrides": [{ "type_name": "crate::Name", "value": "Sword" }] },
//!     { "prefab": "Helmet" }
//!   ]
//! }
//! ```

use std::collections::HashMap;
use std::path::Path;

use apex_core::{
    entity::Entity,
    relations::ChildOf,
    template::TemplateParams,
    world::World,
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::serializer::SerializationError;

// ── Structures ────────────────────────────────────────────────────

/// Description of a single component in a prefab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefabComponent {
    /// Full type name (must match `ComponentInfo.name`).
    pub type_name: String,
    /// JSON value of the component.
    pub value:     serde_json::Value,
}

/// A child in a prefab hierarchy — either a reference to a named sub-prefab (composition; must be
/// loaded into [`PrefabLoader`]), or an **inline** sub-tree manifest (self-contained).
///
/// `untagged`: a reference serializes as `{ "prefab": "...", "overrides": [...] }`, while an inline one
/// serializes as an ordinary [`PrefabManifest`] (`{ "name", "components", "children" }`). The fields do
/// not overlap (`prefab` vs `name`+`components`), so the format is unambiguous and **backward compatible**
/// with old reference files. [`hierarchy_to_prefab`](crate::WorldSerializer::hierarchy_to_prefab) builds
/// `Inline` children, so a hierarchical prefab is a single self-contained file (instantiated without
/// pre-loading sub-prefabs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrefabChild {
    /// Reference to a sub-prefab by name (+ component overrides).
    Ref {
        /// Name of the sub-prefab (must be loaded into `PrefabLoader`).
        prefab:    String,
        /// Component overrides for this child.
        #[serde(default)]
        overrides: Vec<PrefabComponent>,
    },
    /// Inline self-contained sub-tree.
    Inline(PrefabManifest),
}

/// Prefab manifest — a JSON file describing an entity and its children.
///
/// # Versioning
/// `version` is the prefab wire-format version, mirroring [`WorldSnapshot`](crate::WorldSnapshot).
/// It is `#[serde(default)]` = 0, so a pre-versioning file (no field) reads as version 0 and is
/// migrated up to [`Self::CURRENT_VERSION`] on load ([`PrefabManifest::from_json_str`] /
/// [`PrefabLoader::load_json`]). The field is meaningful at the **file root**; on a nested
/// [`PrefabChild::Inline`] sub-tree it is inert (the whole file shares one version). Prefabs are
/// JSON-only, so this is an additive, backward-compatible change (unlike positional bincode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefabManifest {
    /// Prefab wire-format version (see type docs). Defaults to 0 for old, field-less files.
    #[serde(default)]
    pub version:    u32,
    /// Prefab name (for debugging and lookup).
    pub name:       String,
    /// Components of the root entity.
    pub components: Vec<PrefabComponent>,
    /// Child entities (recursive).
    #[serde(default)]
    pub children:   Vec<PrefabChild>,
}

impl PrefabManifest {
    /// Current prefab wire-format version.
    pub const CURRENT_VERSION: u32 = 1;

    /// Parse a manifest from a JSON string, migrating an older version up to
    /// [`Self::CURRENT_VERSION`]. This is the single load choke point — every
    /// consumer (loader cache, asset loader, editor diff reads) should go through
    /// it so no path skips migration (mirrors `WorldSerializer::restore` for snapshots).
    pub fn from_json_str(json: &str) -> Result<Self, PrefabError> {
        let mut manifest: PrefabManifest = serde_json::from_str(json)?;
        manifest.migrate()?;
        Ok(manifest)
    }

    /// Migrate an older-version manifest up to the current version.
    ///
    /// v0 → v1 is a no-op: v0 is the pre-versioning shape, structurally identical
    /// to v1 (the `version` field was simply absent). Future format changes add a
    /// step here. A newer, unmigratable version is left as-is (loaders may still
    /// reject it); like snapshots, this only steps UP toward current.
    pub fn migrate(&mut self) -> Result<(), PrefabError> {
        while self.version < Self::CURRENT_VERSION {
            match self.version {
                0 => {} // no-op: v0 (field-less) is structurally v1
                other => {
                    return Err(PrefabError::UnmigratablePrefabVersion { version: other });
                }
            }
            self.version += 1;
        }
        Ok(())
    }
}

// ── Errors ────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PrefabError {
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("component `{type_name}` not registered in world")]
    ComponentNotRegistered { type_name: String },

    #[error("component `{type_name}` has no serde (de)serializer")]
    ComponentNotSerializable { type_name: String },

    #[error("sub-prefab `{name}` not found in cache")]
    SubPrefabNotFound { name: String },

    #[error("prefab reference cycle: {chain}")]
    CycleDetected { chain: String },

    #[error("prefab nesting exceeds the depth limit ({limit})")]
    DepthLimitExceeded { limit: usize },

    #[error("prefab version {version} is newer than supported and cannot be migrated")]
    UnmigratablePrefabVersion { version: u32 },

    #[error("serialization error: {0}")]
    Serialization(#[from] SerializationError),
}

/// Defensive cap on inline prefab nesting. A hand-authored manifest deeper than
/// this is almost certainly a mistake; named `Ref` cycles are caught separately
/// by the instantiation chain.
const MAX_PREFAB_DEPTH: usize = 256;

// ── PrefabLoader ──────────────────────────────────────────────────

/// Prefab loader and cache.
///
/// Keeps loaded manifests in memory and provides the
/// [`instantiate`](PrefabLoader::instantiate) method for creating entities.
pub struct PrefabLoader {
    /// Cache of loaded manifests (name → manifest).
    cache: FxHashMap<String, PrefabManifest>,
}

impl PrefabLoader {
    pub fn new() -> Self {
        Self { cache: FxHashMap::default() }
    }

    /// Load a manifest from a JSON string (migrating an older version up to current).
    pub fn load_json(&mut self, json: &str) -> Result<&PrefabManifest, PrefabError> {
        let manifest = PrefabManifest::from_json_str(json)?;
        let name = manifest.name.clone();
        self.cache.insert(name.clone(), manifest);
        // Guaranteed to exist — just inserted it
        Ok(self.cache.get(&name).unwrap())
    }

    /// Load a manifest from a file.
    pub fn load_file(&mut self, path: &Path) -> Result<&PrefabManifest, PrefabError> {
        let content = std::fs::read_to_string(path)?;
        self.load_json(&content)
    }

    /// Get a manifest from the cache by name.
    pub fn get(&self, name: &str) -> Option<&PrefabManifest> {
        self.cache.get(name)
    }

    /// Is the prefab in the cache?
    pub fn has(&self, name: &str) -> bool {
        self.cache.contains_key(name)
    }

    /// Number of loaded prefabs.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    // ── Instantiate ──────────────────────────────────────────────

    /// Create an entity from a prefab (recursively, including children).
    ///
    /// * `overrides` — component overrides for the root entity.
    /// * `parent` — if provided, the entity becomes a child via `ChildOf`.
    /// * `params` — template parameters to thread through (may be used by
    ///   custom components implementing `EntityTemplate`).
    // `params` is currently only threaded down the recursion — this is a public
    // contract for custom EntityTemplate components, not dead code.
    #[allow(clippy::only_used_in_recursion)]
    pub fn instantiate(
        &self,
        world: &mut World,
        manifest: &PrefabManifest,
        overrides: &[PrefabComponent],
        parent: Option<Entity>,
        params: Option<&TemplateParams>,
    ) -> Result<Entity, PrefabError> {
        self.instantiate_with(world, manifest, overrides, parent, params, &mut apex_core::NoContext)
    }

    /// `instantiate` with a **(de)serialization context** (TD-44): prefab components with external refs
    /// (asset Handle, etc.) resolve them through `ctx` — consistently with snapshots. The context is
    /// threaded through the whole hierarchy. Ordinary components ignore `ctx`.
    ///
    /// **Atomic:** on any error (unregistered component, missing sub-prefab, cycle, depth) every
    /// entity spawned so far is despawned before the error returns — a failed instantiation must not
    /// leave a partial hierarchy in the world (the editor observed orphaned sub-tree fragments from
    /// exactly such a midway failure).
    #[allow(clippy::too_many_arguments)]
    pub fn instantiate_with(
        &self,
        world: &mut World,
        manifest: &PrefabManifest,
        overrides: &[PrefabComponent],
        parent: Option<Entity>,
        params: Option<&TemplateParams>,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<Entity, PrefabError> {
        let mut chain: Vec<String> = Vec::new();
        let mut spawned: Vec<Entity> = Vec::new();
        let result =
            self.instantiate_rec(world, manifest, overrides, parent, params, ctx, &mut chain, 0, &mut spawned);
        if result.is_err() {
            // Children were spawned after their parents; despawn in reverse so no child ever holds a
            // relation to an already-dead parent mid-cleanup.
            for entity in spawned.into_iter().rev() {
                if world.is_alive(entity) {
                    world.despawn(entity);
                }
            }
        }
        result
    }

    /// Recursive worker for [`instantiate_with`]. `chain` is the stack of named
    /// `Ref` prefabs currently being instantiated — a `Ref` back to any of them
    /// is a cycle (A -> B -> A) and would otherwise recurse until the stack
    /// overflowed and aborted the process (E5). `depth` bounds inline nesting.
    #[allow(clippy::only_used_in_recursion, clippy::too_many_arguments)]
    fn instantiate_rec(
        &self,
        world: &mut World,
        manifest: &PrefabManifest,
        overrides: &[PrefabComponent],
        parent: Option<Entity>,
        params: Option<&TemplateParams>,
        ctx: &mut dyn apex_core::SerdeContext,
        chain: &mut Vec<String>,
        depth: usize,
        spawned: &mut Vec<Entity>,
    ) -> Result<Entity, PrefabError> {
        if depth > MAX_PREFAB_DEPTH {
            return Err(PrefabError::DepthLimitExceeded {
                limit: MAX_PREFAB_DEPTH,
            });
        }
        let entity = self.spawn_entity(world, manifest, overrides, ctx, spawned)?;

        // Parent relation
        if let Some(parent_entity) = parent {
            world.add_relation(entity, ChildOf, parent_entity);
        }

        // Children — a named reference resolves from the loader cache; an inline child is a self-contained
        // sub-manifest instantiated directly (no cache lookup, no pre-loading).
        for child in &manifest.children {
            match child {
                PrefabChild::Ref { prefab, overrides } => {
                    if chain.iter().any(|n| n == prefab) {
                        let mut c = chain.clone();
                        c.push(prefab.clone());
                        return Err(PrefabError::CycleDetected {
                            chain: c.join(" -> "),
                        });
                    }
                    let child_manifest = self
                        .cache
                        .get(prefab)
                        .ok_or_else(|| PrefabError::SubPrefabNotFound { name: prefab.clone() })?;
                    chain.push(prefab.clone());
                    self.instantiate_rec(
                        world,
                        child_manifest,
                        overrides,
                        Some(entity),
                        params,
                        ctx,
                        chain,
                        depth + 1,
                        spawned,
                    )?;
                    chain.pop();
                }
                PrefabChild::Inline(child_manifest) => {
                    self.instantiate_rec(
                        world,
                        child_manifest,
                        &[],
                        Some(entity),
                        params,
                        ctx,
                        chain,
                        depth + 1,
                        spawned,
                    )?;
                }
            }
        }

        Ok(entity)
    }

    /// Internal method: creates a single entity with components from manifest + overrides (via `ctx`).
    /// The entity is recorded into `spawned` immediately, so [`instantiate_with`]'s atomic cleanup
    /// covers it even when a later component of this same entity fails to deserialize.
    fn spawn_entity(
        &self,
        world: &mut World,
        manifest: &PrefabManifest,
        overrides: &[PrefabComponent],
        ctx: &mut dyn apex_core::SerdeContext,
        spawned: &mut Vec<Entity>,
    ) -> Result<Entity, PrefabError> {
        let entity = world.spawn(());
        spawned.push(entity);
        let tick = world.current_tick();

        // Build a HashMap of overrides for fast lookup
        let override_map: HashMap<&str, &serde_json::Value> = overrides
            .iter()
            .map(|c| (c.type_name.as_str(), &c.value))
            .collect();

        // Merge: overrides replace components from the manifest.
        // If the manifest has duplicate type_names — the last one wins.
        let mut seen: HashMap<&str, &serde_json::Value> = HashMap::new();
        for comp in &manifest.components {
            seen.insert(&comp.type_name, &comp.value);
        }
        // Overrides take precedence
        for (type_name, value) in &override_map {
            seen.insert(type_name, value);
        }

        // Apply the components
        for (type_name, json_value) in &seen {
            let component_id = match world.component_id_by_name(type_name) {
                Some(id) => id,
                None => return Err(PrefabError::ComponentNotRegistered {
                    type_name: (*type_name).to_string(),
                }),
            };

            let info = match world.registry().get_info(component_id) {
                Some(i) => i,
                // component_id_by_name resolved above, so this should hold; return
                // a proper error rather than unwrap-panic if the invariant breaks.
                None => return Err(PrefabError::ComponentNotRegistered {
                    type_name: (*type_name).to_string(),
                }),
            };
            let serde_fns = match &info.serde {
                Some(s) => s,
                None => return Err(PrefabError::ComponentNotSerializable {
                    type_name: (*type_name).to_string(),
                }),
            };

            // Serialize JSON Value → JSON string → bytes → deserialize_fn via context (TD-44):
            // components with external refs resolve them; ordinary components ignore `ctx`.
            let json_bytes = serde_json::to_vec(json_value)?;
            let component_bytes = (serde_fns.deserialize_fn)(&json_bytes, ctx)
                .map_err(|e| PrefabError::Serialization(
                    SerializationError::DeserializeFailed {
                        type_name: (*type_name).to_string(),
                        reason: e.to_string(),
                    }
                ))?;

            world.insert_dyn(entity, component_id, component_bytes, tick);
        }

        Ok(entity)
    }
}

impl Default for PrefabLoader {
    fn default() -> Self { Self::new() }
}

// ── EntityTemplate for PrefabManifest ─────────────────────────────

impl apex_core::template::EntityTemplate for PrefabManifest {
    fn spawn(&self, world: &mut World, params: &apex_core::template::TemplateParams) -> Entity {
        let loader = PrefabLoader::new();

        // Convert TemplateParams into overrides.
        // TemplateParams with a component_type_name() become PrefabComponents
        // and replace components with a matching type_name.
        let overrides: Vec<PrefabComponent> = params.json_overrides_iter()
            .map(|(name, json)| PrefabComponent {
                type_name: name.to_string(),
                value: json.clone(),
            })
            .collect();

        // NOTE: child prefabs from self.children will not be found
        // if they were not loaded into this loader. This is a known limitation —
        // EntityTemplate::spawn() has no access to the original PrefabLoader.
        // §0.2a: the `EntityTemplate` trait returns `Entity`, so a failed
        // instantiation cannot propagate a `Result` here (a `try_spawn` on the
        // trait is a wave-6 API change). Until then, panic LOUDLY with the
        // prefab name and the underlying cause instead of an opaque "spawn
        // failed" — the caller sees exactly which prefab and why.
        loader
            .instantiate(world, self, &overrides, None, Some(params))
            .unwrap_or_else(|e| {
                panic!("PrefabManifest::spawn: instantiating prefab '{}' failed: {e}", self.name)
            })
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::prelude::*;
    use serde::{Deserialize, Serialize};

    // Test components
    #[derive(Component, Serialize, Deserialize, Debug, PartialEq)]
    struct Health {
        current: f32,
        max: f32,
    }

    #[derive(Component, Serialize, Deserialize, Debug, PartialEq)]
    struct Name(String);

    #[derive(Component, Serialize, Deserialize, Debug, PartialEq)]
    struct Position {
        x: f32,
        y: f32,
        z: f32,
    }

    fn setup_world() -> World {
        let mut world = World::new();
        world.register_component_serde_json::<Health>();
        world.register_component_serde_json::<Name>();
        world.register_component_serde_json::<Position>();
        world
    }

    #[test]
    fn prefab_json_roundtrip() {
        let manifest = PrefabManifest {
            version: PrefabManifest::CURRENT_VERSION,
            name: "Test".to_string(),
            components: vec![
                PrefabComponent {
                    type_name: "apex_core::Health".to_string(),
                    value: serde_json::json!({ "current": 100.0, "max": 100.0 }),
                },
            ],
            children: vec![],
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let restored: PrefabManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.version, PrefabManifest::CURRENT_VERSION);
        assert_eq!(restored.name, "Test");
        assert_eq!(restored.components.len(), 1);
        assert_eq!(restored.components[0].type_name, "apex_core::Health");
    }

    /// A pre-versioning prefab file (no `version` field) reads as v0 and migrates
    /// up to CURRENT via `from_json_str` — the golden-path load choke point (A4).
    #[test]
    fn old_prefab_without_version_migrates_on_load() {
        let json = r#"{ "name": "Legacy", "components": [] }"#;
        // Raw serde read: version defaults to 0 (field absent).
        let raw: PrefabManifest = serde_json::from_str(json).unwrap();
        assert_eq!(raw.version, 0);
        // The load choke point migrates it to current.
        let migrated = PrefabManifest::from_json_str(json).unwrap();
        assert_eq!(migrated.version, PrefabManifest::CURRENT_VERSION);
        assert_eq!(migrated.name, "Legacy");
    }

    /// A midway instantiation failure must not leave a partial hierarchy: the root and an earlier
    /// child spawn fine, then a later child with an unregistered component fails — EVERY entity
    /// spawned so far must be rolled back (the editor observed orphaned sub-tree fragments from
    /// exactly such a leak on a failed duplicate).
    #[test]
    fn failed_instantiate_rolls_back_all_spawned_entities() {
        let mut world = setup_world();
        let ok_child = PrefabManifest {
            version: PrefabManifest::CURRENT_VERSION,
            name: "Ok".to_string(),
            components: vec![],
            children: vec![],
        };
        let bad_child = PrefabManifest {
            version: PrefabManifest::CURRENT_VERSION,
            name: "Bad".to_string(),
            components: vec![PrefabComponent {
                type_name: "Nonexistent".to_string(),
                value: serde_json::json!({}),
            }],
            children: vec![],
        };
        let manifest = PrefabManifest {
            version: PrefabManifest::CURRENT_VERSION,
            name: "Root".to_string(),
            components: vec![],
            children: vec![PrefabChild::Inline(ok_child), PrefabChild::Inline(bad_child)],
        };
        let before = world.entity_count();
        let result = PrefabLoader::new().instantiate(&mut world, &manifest, &[], None, None);
        assert!(result.is_err(), "an unregistered component must fail the instantiation");
        assert_eq!(
            world.entity_count(),
            before,
            "a failed instantiation must roll back every spawned entity (root + ok-child + bad-child)"
        );
    }

    /// §0.2a: `EntityTemplate::spawn` cannot return a Result, but a failed
    /// instantiation must still be LOUD and name the prefab + cause rather than
    /// panic with an opaque "spawn failed".
    #[test]
    #[should_panic(expected = "instantiating prefab 'Broken'")]
    fn prefab_spawn_panics_loudly_with_name() {
        use apex_core::template::{EntityTemplate, TemplateParams};
        let mut world = World::new();
        // "Nonexistent" is never registered in `world`.
        let manifest = PrefabLoader::new()
            .load_json(r#"{ "name": "Broken", "components": [ { "type_name": "Nonexistent", "value": {} } ] }"#)
            .unwrap()
            .clone();
        let _ = manifest.spawn(&mut world, &TemplateParams::new());
    }

    #[test]
    fn prefab_instantiate_single() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        let json = r#"{
            "name": "Test",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 100.0, "max": 100.0 } },
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Goblin" }
            ]
        }"#;

        let manifest = loader.load_json(json).unwrap().clone();
        let entity = loader.instantiate(&mut world, &manifest, &[], None, None).unwrap();

        let health = world.get::<Health>(entity).unwrap();
        assert_eq!(health.current, 100.0);
        assert_eq!(health.max, 100.0);

        let name = world.get::<Name>(entity).unwrap();
        assert_eq!(name.0, "Goblin");
    }

    #[test]
    fn prefab_instantiate_hierarchy() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        // Parent prefab
        loader.load_json(r#"{
            "name": "Parent",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Parent" }
            ],
            "children": [
                { "prefab": "Child" }
            ]
        }"#).unwrap();

        // Child prefab
        loader.load_json(r#"{
            "name": "Child",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Child" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Parent").unwrap();
        let parent = loader.instantiate(&mut world, manifest, &[], None, None).unwrap();

        let parent_name = world.get::<Name>(parent).unwrap();
        assert_eq!(parent_name.0, "Parent");

        // Find the child via ChildOf
        let children: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(children.len(), 1);

        let child_name = world.get::<Name>(children[0]).unwrap();
        assert_eq!(child_name.0, "Child");
    }

    #[test]
    fn hierarchy_to_prefab_is_self_contained() {
        // Export a parent→child hierarchy to a SINGLE prefab, then instantiate it in a fresh world with an
        // EMPTY loader (no sub-prefabs pre-loaded). Inline children make the file self-contained — before
        // this fix the export kept only child *names* and instantiate failed with `SubPrefabNotFound`.
        use crate::WorldSerializer;

        let mut world = setup_world();
        let parent = world.spawn(());
        world.insert(parent, Name("Parent".to_string()));
        let child = world.spawn(());
        world.insert(child, Name("Child".to_string()));
        world.insert(child, Health { current: 30.0, max: 30.0 });
        world.add_relation(child, ChildOf, parent);

        let manifest = WorldSerializer::hierarchy_to_prefab(&world, parent).unwrap();
        // The child is INLINE (a full sub-manifest), not a name reference.
        assert_eq!(manifest.children.len(), 1);
        assert!(matches!(manifest.children[0], PrefabChild::Inline(_)));

        // Round-trip through JSON, then instantiate into a fresh world with an empty loader cache.
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: PrefabManifest = serde_json::from_str(&json).unwrap();

        let mut world2 = setup_world();
        let loader = PrefabLoader::new(); // empty cache — proves self-containment
        let new_parent = loader.instantiate(&mut world2, &restored, &[], None, None).unwrap();

        assert_eq!(world2.get::<Name>(new_parent).unwrap().0, "Parent");
        let kids: Vec<Entity> = world2.targets_of(ChildOf, new_parent).collect();
        assert_eq!(kids.len(), 1);
        assert_eq!(world2.get::<Name>(kids[0]).unwrap().0, "Child");
        assert_eq!(world2.get::<Health>(kids[0]).unwrap().current, 30.0);
    }

    #[test]
    fn prefab_with_overrides() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{
            "name": "Monster",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 50.0, "max": 50.0 } },
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Monster" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Monster").unwrap();
        let overrides = vec![
            PrefabComponent {
                type_name: "apex_serialization::prefab::tests::Health".to_string(),
                value: serde_json::json!({ "current": 200.0, "max": 200.0 }),
            },
        ];

        let entity = loader.instantiate(&mut world, manifest, &overrides, None, None).unwrap();

        let health = world.get::<Health>(entity).unwrap();
        assert_eq!(health.current, 200.0);  // override
        assert_eq!(health.max, 200.0);

        let name = world.get::<Name>(entity).unwrap();
        assert_eq!(name.0, "Monster");  // default

    }

    #[test]
    fn prefab_component_not_registered() {
        let mut world = World::new();  // Health is registered (auto) but has no serde functions — must fail
        let mut loader = PrefabLoader::new();

        let json = r#"{
            "name": "Test",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 100.0, "max": 100.0 } }
            ]
        }"#;

        let manifest = loader.load_json(json).unwrap().clone();
        let result = loader.instantiate(&mut world, &manifest, &[], None, None);

        // Health is registered (auto-reg) but has no serde functions —
        // deserialization will fail. Expect any error.
        assert!(result.is_err());
    }

    #[test]
    fn prefab_sub_prefab_not_found() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{
            "name": "Parent",
            "components": [],
            "children": [
                { "prefab": "NonExistent" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Parent").unwrap();
        let result = loader.instantiate(&mut world, manifest, &[], None, None);

        assert!(result.is_err());
        match result {
            Err(PrefabError::SubPrefabNotFound { .. }) => {} // expected
            _ => panic!("expected SubPrefabNotFound error"),
        }
    }

    #[test]
    fn prefab_loader_cache() {
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{"name": "A", "components": []}"#).unwrap();
        loader.load_json(r#"{"name": "B", "components": []}"#).unwrap();

        assert!(loader.has("A"));
        assert!(loader.has("B"));
        assert!(!loader.has("C"));
        assert_eq!(loader.len(), 2);
    }

    #[test]
    fn prefab_instantiate_with_position() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{
            "name": "Mover",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Position", "value": { "x": 1.0, "y": 2.0, "z": 3.0 } },
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Mover" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Mover").unwrap();
        let entity = loader.instantiate(&mut world, manifest, &[], None, None).unwrap();

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.x, 1.0);
        assert_eq!(pos.y, 2.0);
        assert_eq!(pos.z, 3.0);
    }

    #[test]
    fn prefab_child_overrides() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        // Child prefab — will be overridden
        loader.load_json(r#"{
            "name": "Child",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 10.0, "max": 10.0 } },
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "OriginalChild" }
            ]
        }"#).unwrap();

        // Parent prefab with a child prefab + overrides
        loader.load_json(r#"{
            "name": "Parent",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Parent" }
            ],
            "children": [
                {
                    "prefab": "Child",
                    "overrides": [
                        { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 999.0, "max": 999.0 } },
                        { "type_name": "apex_serialization::prefab::tests::Name", "value": "OverriddenChild" }
                    ]
                }
            ]
        }"#).unwrap();

        let manifest = loader.get("Parent").unwrap();
        let parent = loader.instantiate(&mut world, manifest, &[], None, None).unwrap();

        // Check the parent
        let parent_name = world.get::<Name>(parent).unwrap();
        assert_eq!(parent_name.0, "Parent");

        // Find the child
        use apex_core::relations::ChildOf;
        let children: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(children.len(), 1, "parent must have exactly one child");

        let child = children[0];

        // Check that the overrides were applied
        let child_health = world.get::<Health>(child).unwrap();
        assert_eq!(child_health.current, 999.0, "Health.current must come from overrides");
        assert_eq!(child_health.max, 999.0, "Health.max must come from overrides");

        let child_name = world.get::<Name>(child).unwrap();
        assert_eq!(child_name.0, "OverriddenChild", "Name must come from overrides");
    }

    /// TD-44 gap-close: `SerdeContext` threads through BOTH the **diff** path (`diff_with`) and the
    /// **prefab** path (`entity_to_prefab_with` + `instantiate_with`) — consistently with `snapshot_with`,
    /// no silent `NoContext`. A context-aware (JSON, host-resolved) component is verified end-to-end on
    /// both: a component with an external ref resolves in a diff-save and round-trips through a prefab.
    #[test]
    fn context_threads_through_diff_and_prefab() {
        use crate::WorldSerializer;
        use apex_core::{ComponentSerdeFns, SerdeContext};
        use std::any::Any;

        struct Shift {
            by: i64,
        }
        impl SerdeContext for Shift {
            fn as_any(&self) -> &dyn Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn Any {
                self
            }
        }
        #[derive(Component, Debug, PartialEq)]
        struct CtxRef(i64);

        // JSON-format context-aware fns (compatible with snapshot AND prefab paths).
        fn make_fns() -> ComponentSerdeFns {
            ComponentSerdeFns {
                serialize_fn: |ptr, ctx| {
                    let r = unsafe { &*(ptr as *const CtxRef) };
                    let by = ctx.as_any().downcast_ref::<Shift>().map_or(0, |s| s.by);
                    Ok(serde_json::to_vec(&(r.0 + by)).unwrap())
                },
                deserialize_fn: |bytes, ctx| {
                    let by = ctx.as_any().downcast_ref::<Shift>().map_or(0, |s| s.by);
                    let stored: i64 = serde_json::from_slice(bytes).unwrap();
                    let r = CtxRef(stored - by);
                    let size = std::mem::size_of::<CtxRef>();
                    let mut buf = vec![0u8; size];
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            &r as *const CtxRef as *const u8,
                            buf.as_mut_ptr(),
                            size,
                        );
                    }
                    std::mem::forget(r);
                    Ok(buf)
                },
                format: "json",
            }
        }

        // ── diff path: diff_with serialises the added component THROUGH the context ──
        let mut world = World::new();
        world.register_component_serde_with::<CtxRef>(make_fns());
        let empty = WorldSerializer::snapshot_with(&world, &mut Shift { by: 0 }).unwrap();
        world.spawn((CtxRef(7),));
        let diff = WorldSerializer::diff_with(&empty, &world, &mut Shift { by: 1000 }).unwrap();
        let added = &diff.added_entities[0].components[0].data;
        let stored: i64 = serde_json::from_slice(added).unwrap();
        assert_eq!(stored, 1007, "diff_with threaded the context (7 → 1007)");

        // ── prefab path: entity_to_prefab_with serialises, instantiate_with deserialises, via context ──
        let e = world.spawn((CtxRef(7),));
        let manifest = WorldSerializer::entity_to_prefab_with(&world, e, &mut Shift { by: 1000 }).unwrap();
        assert_eq!(
            manifest.components[0].value,
            serde_json::json!(1007),
            "entity_to_prefab_with resolved the ref via context"
        );

        let mut rworld = World::new();
        rworld.register_component_serde_with::<CtxRef>(make_fns());
        let loader = PrefabLoader::new();
        let inst = loader
            .instantiate_with(&mut rworld, &manifest, &[], None, None, &mut Shift { by: 1000 })
            .unwrap();
        assert_eq!(
            rworld.get::<CtxRef>(inst),
            Some(&CtxRef(7)),
            "instantiate_with threaded the context ⇒ ref resolved back to 7"
        );
    }

    /// E5: two prefabs referencing each other must not recurse until the stack
    /// overflows — the cycle is detected and reported.
    #[test]
    fn instantiate_rejects_ref_cycle() {
        let mut loader = PrefabLoader::new();
        loader.cache.insert(
            "A".into(),
            PrefabManifest {
                version: PrefabManifest::CURRENT_VERSION,
                name: "A".into(),
                components: vec![],
                children: vec![PrefabChild::Ref {
                    prefab: "B".into(),
                    overrides: vec![],
                }],
            },
        );
        loader.cache.insert(
            "B".into(),
            PrefabManifest {
                version: PrefabManifest::CURRENT_VERSION,
                name: "B".into(),
                components: vec![],
                children: vec![PrefabChild::Ref {
                    prefab: "A".into(),
                    overrides: vec![],
                }],
            },
        );

        let mut world = World::new();
        let a = loader.get("A").unwrap().clone();
        let res = loader.instantiate(&mut world, &a, &[], None, None);
        assert!(
            matches!(res, Err(PrefabError::CycleDetected { .. })),
            "a Ref cycle must be rejected, got {res:?}"
        );
    }
}
