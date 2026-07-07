//! Integration tests for `WorldSerializer`: full world round-trip with
//! relations, and the loud error paths.
//!
//! The inline unit tests in `serializer.rs` pin individual mechanisms (ZST
//! markers, E6 entity remap, E7 resources, diffs). These tests exercise the
//! PUBLIC api the way a consumer does — snapshot a whole populated world, take
//! it through JSON, restore into a fresh world, and assert *equivalence of the
//! restored artifact* (every component value, every relation), not just a
//! single field. Then they assert each failure mode surfaces as its own typed
//! `SerializationError` rather than being swallowed (§0.2a).

use std::collections::HashSet;

use apex_core::prelude::*;
use apex_core::relations::{ChildOf, RelationKind};
use serde::{Deserialize, Serialize};

use apex_serialization::{SerializationError, WorldSerializer, WorldSnapshot};

// ── Test components ───────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Velocity {
    dx: f32,
    dy: f32,
}

#[derive(Component, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Name(String);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Health {
    hp: f32,
    max: f32,
}

/// Zero-sized marker — presence is the entire state.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Frozen;

/// A **generic** component: `#[derive(Component)]` deliberately emits no linkme
/// registrar for generics (they register lazily on first concrete use). That
/// makes `Payload<u32>` guaranteed-absent from a fresh world's registry unless
/// explicitly registered — the honest way to exercise the "unknown type on
/// restore" (version-skew) path, since non-generic test components auto-register
/// via linkme in the test binary and would never hit `ComponentNotRegistered`.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Payload<T: Send + Sync + 'static>(T);

// ── Custom relation kind (beyond the built-in ChildOf) ────────────

#[derive(Clone, Copy)]
struct Follows;
impl RelationKind for Follows {}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct GameConfig {
    difficulty: u32,
    seed: u64,
}

// ── Helpers ───────────────────────────────────────────────────────

/// Register the full component + relation-kind + resource vocabulary on a world
/// (used for both the save side and the fresh restore side, so the restore side
/// knows how to deserialize every type and resolve every relation kind by name).
fn register_vocabulary(world: &mut World) {
    world.register_component_serde_json::<Position>();
    world.register_component_serde_json::<Velocity>();
    world.register_component_serde_json::<Name>();
    world.register_component_serde_json::<Health>();
    world.register_component_serde_json::<Frozen>();
    world.register_resource_serde::<GameConfig>();
    // Relation kinds are resolved by name on restore; register both so a
    // restored relation finds its kind instead of being skipped with a warn.
    world.relation_registry_mut().get_or_register::<ChildOf>();
    world.relation_registry_mut().get_or_register::<Follows>();
}

/// Every restored component value must equal the original, for all types.
fn assert_components_eq(w_old: &World, e_old: Entity, w_new: &World, e_new: Entity) {
    assert_eq!(w_old.get::<Position>(e_old), w_new.get::<Position>(e_new), "Position");
    assert_eq!(w_old.get::<Velocity>(e_old), w_new.get::<Velocity>(e_new), "Velocity");
    assert_eq!(w_old.get::<Name>(e_old), w_new.get::<Name>(e_new), "Name");
    assert_eq!(w_old.get::<Health>(e_old), w_new.get::<Health>(e_new), "Health");
    assert_eq!(w_old.get::<Frozen>(e_old), w_new.get::<Frozen>(e_new), "Frozen");
}

/// Relation set as `(subject_index, kind_name, target_index)` — comparable
/// across worlds once the caller has re-expressed the old indices through the
/// restore entity map.
fn relation_set(world: &World) -> HashSet<(u32, String, u32)> {
    world
        .iter_relations()
        .map(|(subj, kind_idx, target)| {
            let name = world
                .relation_registry()
                .get_name(kind_idx)
                .unwrap_or("<unknown>")
                .to_string();
            (subj, name, target.index())
        })
        .collect()
}

// ── Round-trip: full equivalence ──────────────────────────────────

#[test]
fn full_world_json_roundtrip_preserves_every_component_and_relation() {
    let mut world = World::new();
    register_vocabulary(&mut world);
    world.insert_resource(GameConfig { difficulty: 3, seed: 0xABCD });

    // A small but varied scene: different component sets, a ZST marker, a
    // parent chain (ChildOf, exclusive) and a cross-link (Follows).
    let root = world.spawn((
        Position { x: 1.0, y: 2.0 },
        Name("root".into()),
        Health { hp: 100.0, max: 100.0 },
    ));
    let child = world.spawn((
        Position { x: 3.0, y: 4.0 },
        Velocity { dx: -1.0, dy: 0.5 },
        Frozen,
    ));
    let follower = world.spawn((Position { x: 5.0, y: 6.0 }, Name("follower".into())));
    world.add_relation(child, ChildOf, root);
    world.add_relation(follower, Follows, root);

    let originals = [root, child, follower];

    // snapshot → JSON bytes → parse back → restore into a fresh world.
    let snap = WorldSerializer::snapshot(&world).unwrap();
    let json = snap.to_json().unwrap();
    let parsed = WorldSnapshot::from_json(&json).unwrap();

    let mut restored = World::new();
    register_vocabulary(&mut restored);
    let map = WorldSerializer::restore(&mut restored, &parsed).unwrap();

    // Same number of entities came back.
    assert_eq!(map.len(), originals.len(), "entity count must match");

    // Every component value round-trips for every original entity.
    for &e_old in &originals {
        let e_new = map[&e_old.index()];
        assert_components_eq(&world, e_old, &restored, e_new);
    }

    // Relations round-trip: re-express old relations through the entity map and
    // compare against the restored world's relation set exactly.
    let expected: HashSet<(u32, String, u32)> = relation_set(&world)
        .into_iter()
        .map(|(subj, kind, target)| {
            (map[&subj].index(), kind, map[&target].index())
        })
        .collect();
    assert_eq!(
        expected,
        relation_set(&restored),
        "the restored relation set must match the original, remapped through the entity map"
    );

    // The opted-in resource round-trips too.
    assert_eq!(
        restored.try_resource::<GameConfig>(),
        Some(&GameConfig { difficulty: 3, seed: 0xABCD }),
    );
}

/// Bincode is the other supported wire format — the same equivalence must hold.
#[test]
fn full_world_bincode_roundtrip_preserves_relations() {
    let mut world = World::new();
    register_vocabulary(&mut world);
    let a = world.spawn((Position { x: 7.0, y: 8.0 }, Frozen));
    let b = world.spawn((Position { x: 9.0, y: 10.0 },));
    world.add_relation(a, ChildOf, b);

    let snap = WorldSerializer::snapshot(&world).unwrap();
    let bytes = snap.to_bincode().unwrap();
    let parsed = WorldSnapshot::from_bincode(&bytes).unwrap();

    let mut restored = World::new();
    register_vocabulary(&mut restored);
    let map = WorldSerializer::restore(&mut restored, &parsed).unwrap();

    assert_eq!(map.len(), 2);
    let a2 = map[&a.index()];
    let b2 = map[&b.index()];
    assert_eq!(restored.get::<Position>(a2), Some(&Position { x: 7.0, y: 8.0 }));
    assert!(restored.get::<Frozen>(a2).is_some(), "ZST marker survives bincode");
    assert!(
        restored.has_relation(a2, ChildOf, b2),
        "ChildOf(a -> b) must survive a bincode round-trip and remap"
    );
}

// ── Deterministic "fuzz": many random worlds, each must round-trip ─

/// Tiny deterministic LCG (Knuth MMIX constants). No `rand` dependency and no
/// wall-clock seed, so failures reproduce exactly.
struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn below(&mut self, n: u32) -> u32 {
        ((self.next_u64() >> 33) as u32) % n
    }
    fn coord(&mut self) -> f32 {
        self.below(200_000) as f32 / 1000.0 - 100.0
    }
    fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }
}

#[test]
fn fuzz_random_worlds_roundtrip_to_equivalence() {
    let mut rng = Lcg(0x1234_5678_9abc_def0);

    for iteration in 0..64 {
        let mut world = World::new();
        register_vocabulary(&mut world);

        let n = 1 + rng.below(12); // 1..=12 entities
        let mut entities = Vec::new();
        for i in 0..n {
            // Position is always present so the entity carries at least one
            // serde-backed component; the rest are random.
            let e = world.spawn((Position { x: rng.coord(), y: rng.coord() },));
            if rng.chance(60) {
                world.insert(e, Velocity { dx: rng.coord(), dy: rng.coord() });
            }
            if rng.chance(50) {
                world.insert(e, Name(format!("e{iteration}_{i}")));
            }
            if rng.chance(40) {
                world.insert(e, Health { hp: rng.coord().abs(), max: 100.0 });
            }
            if rng.chance(25) {
                world.insert(e, Frozen);
            }
            // Link to an earlier entity so relations are acyclic and valid.
            if i > 0 && rng.chance(50) {
                let parent = entities[rng.below(i) as usize];
                world.add_relation(e, ChildOf, parent);
            }
            if i > 0 && rng.chance(30) {
                let other = entities[rng.below(i) as usize];
                world.add_relation(e, Follows, other);
            }
            entities.push(e);
        }

        let snap = WorldSerializer::snapshot(&world).unwrap();
        let json = snap.to_json().unwrap();
        let parsed = WorldSnapshot::from_json(&json).unwrap();

        let mut restored = World::new();
        register_vocabulary(&mut restored);
        let map = WorldSerializer::restore(&mut restored, &parsed).unwrap();

        assert_eq!(
            map.len(),
            entities.len(),
            "iteration {iteration}: entity count mismatch"
        );
        for &e_old in &entities {
            let e_new = map[&e_old.index()];
            assert_components_eq(&world, e_old, &restored, e_new);
        }

        let expected: HashSet<(u32, String, u32)> = relation_set(&world)
            .into_iter()
            .map(|(subj, kind, target)| (map[&subj].index(), kind, map[&target].index()))
            .collect();
        assert_eq!(
            expected,
            relation_set(&restored),
            "iteration {iteration}: relation set mismatch after round-trip"
        );
    }
}

// ── Error paths (loud, typed failures — §0.2a) ────────────────────

/// A snapshot referencing a type the restore world never registered must fail
/// with `ComponentNotRegistered` naming that type — not silently drop it.
#[test]
fn restore_unregistered_component_is_a_typed_error() {
    let mut world = World::new();
    world.register_component_serde_json::<Payload<u32>>();
    world.spawn((Payload(99u32),));
    let snap = WorldSerializer::snapshot(&world).unwrap();
    assert_eq!(snap.entities.len(), 1);
    assert_eq!(snap.entities[0].components.len(), 1, "Payload<u32> was serialized");

    // Restore world knows nothing about Payload<u32> (generic ⇒ no linkme
    // auto-registration), so its type_name is absent from the registry.
    let mut restored = World::new();
    let err = WorldSerializer::restore(&mut restored, &snap).unwrap_err();
    match err {
        SerializationError::ComponentNotRegistered { type_name } => {
            assert!(
                type_name.contains("Payload"),
                "error must name the missing type, got: {type_name}"
            );
        }
        other => panic!("expected ComponentNotRegistered, got {other:?}"),
    }
}

/// Corrupt JSON must fail at parse time, not produce a bogus snapshot.
#[test]
fn corrupt_json_fails_to_parse() {
    let garbage = br#"{ "version": 2, "entities": [ this is not json ] "#;
    let result = WorldSnapshot::from_json(garbage);
    assert!(result.is_err(), "malformed JSON must not parse into a snapshot");
}

/// A snapshot from a newer, unmigratable format version is rejected loudly by
/// `restore` with the expected/found versions (no forward migrator exists).
#[test]
fn future_version_is_rejected_by_restore() {
    let mut world = World::new();
    register_vocabulary(&mut world);
    world.spawn((Position { x: 0.0, y: 0.0 },));
    let mut snap = WorldSerializer::snapshot(&world).unwrap();

    snap.version = WorldSnapshot::CURRENT_VERSION + 1; // from the future

    let mut restored = World::new();
    register_vocabulary(&mut restored);
    match WorldSerializer::restore(&mut restored, &snap).unwrap_err() {
        SerializationError::VersionMismatch { expected, found } => {
            assert_eq!(expected, WorldSnapshot::CURRENT_VERSION);
            assert_eq!(found, WorldSnapshot::CURRENT_VERSION + 1);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

/// An OLDER-version snapshot restores by migrating internally — the golden-path
/// fix for A2. This exercises the same path the editor uses (`restore` /
/// `restore_with`, not `read_from_file`), which previously bypassed migration
/// and hard-rejected old versions. A v1 snapshot (pre-`resources`, v1→v2 no-op
/// migrator) must load its entity + component through restore's centralised migrate.
#[test]
fn older_version_snapshot_is_migrated_on_restore() {
    let mut world = World::new();
    register_vocabulary(&mut world);
    world.spawn((Position { x: 3.0, y: 4.0 },));
    let mut snap = WorldSerializer::snapshot(&world).unwrap();
    assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);

    // Downgrade the envelope to the previous version (as if loaded from an old
    // on-disk scene). The caller's copy must NOT be mutated by restore.
    snap.version = WorldSnapshot::CURRENT_VERSION - 1;

    let mut restored = World::new();
    register_vocabulary(&mut restored);
    let map = WorldSerializer::restore(&mut restored, &snap)
        .expect("old-version snapshot must migrate + restore, not be rejected");

    assert_eq!(map.len(), 1, "the single entity must be restored");
    // restore migrated a throwaway copy — the caller's snapshot is untouched.
    assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION - 1);
    // The migrated entity carries its component.
    let (&_old, &new_entity) = map.iter().next().unwrap();
    let pos = restored.get::<Position>(new_entity).expect("Position restored");
    assert_eq!((pos.x, pos.y), (3.0, 4.0));
}

/// If a component's stored bytes are corrupt, restore must surface
/// `DeserializeFailed` naming the type — never spawn a half-decoded value.
#[test]
fn corrupt_component_bytes_yield_deserialize_failed() {
    let mut world = World::new();
    world.register_component_serde_json::<Position>();
    world.spawn((Position { x: 1.0, y: 2.0 },));
    let mut snap = WorldSerializer::snapshot(&world).unwrap();

    // Overwrite the (single, non-ZST) component's payload with non-JSON bytes.
    assert_eq!(snap.entities[0].components.len(), 1);
    snap.entities[0].components[0].data = b"\xff not valid json \x00".to_vec();

    let mut restored = World::new();
    restored.register_component_serde_json::<Position>();
    match WorldSerializer::restore(&mut restored, &snap).unwrap_err() {
        SerializationError::DeserializeFailed { type_name, .. } => {
            assert!(
                type_name.contains("Position"),
                "error must name the failing type, got: {type_name}"
            );
        }
        other => panic!("expected DeserializeFailed, got {other:?}"),
    }
}
