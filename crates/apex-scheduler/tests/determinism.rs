//! Run-to-run determinism gate for `set_deterministic_spawn(true)` (ADR-001).
//!
//! ADR-001 sells **single-binary determinism**: the same binary, given the same start
//! snapshot + inputs, reproduces an IDENTICAL entity-id assignment, command-application
//! order, and iteration-independent result — the record/replay / rollback-netcode
//! foundation, which Bevy does not guarantee under parallel spawning.
//!
//! This is a permanent, cheap CI gate (the same in-process style as the executor-parity
//! guard for D9): representative schedules run TWICE in one process through the
//! production `run` executor with deterministic spawn ON, and the FULL id-sensitive
//! snapshot must be byte-identical across the two runs. Any drift — in id assignment,
//! ordering, or observable state — fails loudly here (§0.2a) rather than silently making
//! record/replay diverge, and without a cross-repo goldens run.
//!
//! Unlike executor parity (seq vs par — SEMANTIC state only, ids may legitimately differ
//! by allocation strategy), this asserts the strong guarantee: identical ENTITY IDS too.

use apex_core::prelude::*;
use apex_core::query::Read;
use apex_core::world::ChunkConfig;
use apex_core::{system, World};
use apex_macros::Component;
use apex_scheduler::{conditions, sys, Scheduler, StageLabel};

// ── Components / events ────────────────────────────────────────────

#[derive(Component)]
struct Seed(u32);

#[derive(Component, Clone, Copy)]
struct Tag {
    sys: u32,
    val: u32,
}

#[derive(Component, Clone, Copy)]
struct Live(u32);

#[derive(Clone, Copy)]
struct Spawned {
    val: u32,
}

// ── id-sensitive snapshot ──────────────────────────────────────────

/// Every entity carrying a `Tag` or `Live`, as `(entity_index, kind, a, b)`, sorted.
/// This is ID-SENSITIVE by design: the entity index is part of the key, so two runs
/// match only if the deterministic id assignment is reproduced exactly.
fn snapshot(world: &World) -> Vec<(u32, u8, u32, u32)> {
    let mut out = Vec::new();
    Query::<Read<Tag>>::new(world).for_each(|e, t| out.push((e.index(), 0u8, t.sys, t.val)));
    Query::<Read<Live>>::new(world).for_each(|e, l| out.push((e.index(), 1u8, l.0, 0)));
    out.sort_unstable();
    out
}

/// Force the true parallel path (no serial fallback, small stages still split) so the
/// deterministic block scheme is actually exercised — otherwise a small stage would run
/// sequentially (trivially deterministic) and never test the concurrent-spawn path.
fn force_parallel(world: &mut World) {
    world.set_chunk_config(ChunkConfig {
        auto_disable_stage_parallel: false,
        stage_parallel_min_entities: 0,
        ..Default::default()
    });
}

/// Run `build` for `frames` frames on a fresh deterministic-spawn scheduler, twice, and
/// assert the id-sensitive snapshots are identical. `label` names the schedule on failure.
fn assert_run_to_run_deterministic(
    build: impl Fn(&mut Scheduler, &mut World),
    frames: usize,
    label: &str,
) {
    let run = |build: &dyn Fn(&mut Scheduler, &mut World)| -> Vec<(u32, u8, u32, u32)> {
        let mut world = World::new();
        let mut sched = Scheduler::new();
        sched.set_deterministic_spawn(true);
        force_parallel(&mut world);
        build(&mut sched, &mut world);
        for _ in 0..frames {
            sched.run(&mut world);
        }
        snapshot(&world)
    };
    let a = run(&build);
    let b = run(&build);
    assert!(!a.is_empty(), "schedule '{label}' produced no entities — test is vacuous");
    assert_eq!(
        a, b,
        "run-to-run determinism broken for schedule '{label}': the id→content mapping \
         differs between two independent runs (ADR-001 single-binary guarantee)"
    );
    // Every id is unique within a run (no double-issue across per-system blocks).
    let ids: std::collections::BTreeSet<u32> = a.iter().map(|&(i, ..)| i).collect();
    assert_eq!(ids.len(), a.len(), "schedule '{label}': entity ids must be unique");
}

// ── Systems ────────────────────────────────────────────────────────

// Three command-emitting systems spawning concurrently in one all-parallel stage — the
// exact case a shared reserver makes non-deterministic (each is single-task, its own
// rayon task).
system! {
    fn spawn_a(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Tag { sys: 0, val: s.0 },)); });
    }
}
system! {
    fn spawn_b(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Tag { sys: 1, val: s.0 },)); });
    }
}
system! {
    fn spawn_c(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Tag { sys: 2, val: s.0 },)); });
    }
}

// Churn: PreUpdate despawns last frame's Live children; Update respawns one per seed.
system! {
    fn churn_despawn(q: Read<Live>, cmds: Cmd) {
        q.for_each(|e, _| { cmds.despawn(e); });
    }
}
system! {
    fn churn_spawn(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Live(s.0),)); });
    }
}

// Event-driven spawn: a producer emits N events; a consumer spawns one child per event.
system! {
    fn emit_spawns(seeds: Read<Seed>, writer: &mut Vec<Spawned>) {
        seeds.for_each(|_, s| { writer.send(Spawned { val: s.0 }); });
    }
}
system! {
    fn spawn_from_events(reader: &[Spawned], cmds: Cmd) {
        for ev in reader.iter() {
            cmds.spawn((Tag { sys: 9, val: ev.val },));
        }
    }
}

fn seed_world(world: &mut World, n: u32) {
    for i in 0..n {
        world.spawn((Seed(i),));
    }
}

// ── Gates ──────────────────────────────────────────────────────────

/// Concurrent spawn: three systems spawn one tagged child per seed in one parallel
/// stage. The (id → sys, seed) mapping is identical run-to-run.
#[test]
fn determinism_concurrent_spawn() {
    assert_run_to_run_deterministic(
        |sched, world| {
            seed_world(world, 64);
            sched.add_systems(StageLabel::Update, (spawn_a, spawn_b, spawn_c));
            sched.compile_with_world(world).unwrap();
        },
        1,
        "concurrent_spawn",
    );
}

/// Despawn+respawn churn over many frames: freed slots are reused DETERMINISTICALLY, so
/// the surviving id→content mapping is identical run-to-run (the reuse-aware frontier).
#[test]
fn determinism_spawn_despawn_churn() {
    assert_run_to_run_deterministic(
        |sched, world| {
            seed_world(world, 48);
            sched.add_systems(StageLabel::PreUpdate, churn_despawn);
            sched.add_systems(StageLabel::Update, churn_spawn);
            sched.compile_with_world(world).unwrap();
        },
        25,
        "spawn_despawn_churn",
    );
}

/// Conditional spawning: a spawner gated by `run_until(5)` runs only the first frames.
/// The deterministic id assignment must be identical run-to-run despite the gating.
#[test]
fn determinism_conditional_spawn() {
    assert_run_to_run_deterministic(
        |sched, world| {
            seed_world(world, 32);
            // A command-emitting spawner (gets a deterministic block) gated by
            // run_until — it spawns only the first 5 frames, then stops.
            sched.add_systems(
                StageLabel::Update,
                sys("gated_spawn", spawn_a).run_if_cond(conditions::run_until(5)),
            );
            sched.compile_with_world(world).unwrap();
        },
        10,
        "conditional_spawn",
    );
}

/// Event-driven spawn: spawns are produced from events read across the frame. The
/// event→spawn→id pipeline is deterministic run-to-run.
#[test]
fn determinism_event_driven_spawn() {
    assert_run_to_run_deterministic(
        |sched, world| {
            seed_world(world, 40);
            world.add_event::<Spawned>();
            sched.add_systems(StageLabel::PreUpdate, emit_spawns);
            sched.add_systems(StageLabel::Update, spawn_from_events);
            sched.compile_with_world(world).unwrap();
        },
        3,
        "event_driven_spawn",
    );
}

/// Spike within the escrow (1.1): a spawn count exceeding the adaptive block but within
/// block + escrow stays deterministic run-to-run (the frontier ADR-001 deferred, now
/// covered for in-escrow spikes).
#[test]
fn determinism_escrow_spike() {
    assert_run_to_run_deterministic(
        |sched, world| {
            seed_world(world, 300); // > INITIAL block 256, <= block + escrow 384
            sched.add_systems(StageLabel::Update, (spawn_a, spawn_b, spawn_c));
            sched.compile_with_world(world).unwrap();
        },
        1,
        "escrow_spike",
    );
}
