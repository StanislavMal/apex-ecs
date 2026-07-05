//! D8b capstone — deterministic parallel-spawn entity-id assignment.
//!
//! Three command-emitting systems spawn entities CONCURRENTLY in one all-parallel
//! stage (each reads `Seed` — a shared read, so no conflict — and spawns one tagged
//! child per seed). With `deterministic_spawn` ON, each system draws ids from a
//! private, rank-deterministic block, so the entity-id assignment AND the resulting
//! world snapshot are IDENTICAL across N independent fresh runs — the record/replay
//! guarantee. Bevy does not provide this (its parallel reserve races on a shared
//! atomic). See `plans/active/WAVE6B_SOUNDNESS_DETERMINISM.md` §4.
//!
//! Without D8b, the three tasks would race on the shared high-water reserver, so the
//! (id → content) mapping would vary run-to-run. The capstone asserts the mapping is
//! stable across 40 runs and that each system's ids form a contiguous block.

use apex_core::prelude::*;
use apex_core::{system, World};
use apex_macros::Component;
use apex_scheduler::{Scheduler, StageLabel};
use std::collections::BTreeSet;

const SEEDS: u32 = 64; // < INITIAL block (256) ⇒ no frame-1 overflow ⇒ deterministic.

#[derive(Component)]
struct Seed(u32);

#[derive(Component, Clone, Copy)]
struct Made {
    sys: u32,
    seed: u32,
}

// Three command-emitting systems. Each is single-task (uses `Commands` ⇒
// `non_query_side_effects` ⇒ never row-split) and runs as its own rayon task, so the
// three spawn concurrently — the exact case a shared reserver makes non-deterministic.
system! {
    fn spawn_a(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Made { sys: 0, seed: s.0 },)); });
    }
}
system! {
    fn spawn_b(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Made { sys: 1, seed: s.0 },)); });
    }
}
system! {
    fn spawn_c(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Made { sys: 2, seed: s.0 },)); });
    }
}

/// One fresh run: build a world with `SEEDS` seeds, force the parallel path, spawn
/// concurrently, and return the sorted `(entity_index, sys, seed)` snapshot.
fn run_once() -> Vec<(u32, u32, u32)> {
    let mut world = World::new();
    for i in 0..SEEDS {
        world.spawn((Seed(i),));
    }

    let mut sched = Scheduler::new();
    sched.set_deterministic_spawn(true);
    // Force the true parallel path on this (fresh, no cost-history) run so the three
    // spawners actually race — otherwise a small stage would fall back to sequential
    // (which is trivially deterministic and would not exercise the block scheme).
    world.set_chunk_config(apex_core::world::ChunkConfig {
        auto_disable_stage_parallel: false,
        stage_parallel_min_entities: 0,
        ..Default::default()
    });
    sched.add_systems(StageLabel::Update, (spawn_a, spawn_b, spawn_c));
    sched.compile_with_world(&world).unwrap();
    sched.run(&mut world);

    let mut out = Vec::new();
    Query::<Read<Made>>::new(&world).for_each(|e, m| out.push((e.index(), m.sys, m.seed)));
    out.sort_unstable();
    out
}

#[test]
fn deterministic_parallel_spawn_is_reproducible() {
    let baseline = run_once();
    assert_eq!(
        baseline.len() as u32,
        SEEDS * 3,
        "each of the 3 systems spawns one child per seed"
    );

    // (1) Byte-for-byte identical (id → content) mapping across 40 independent runs.
    for run in 1..40 {
        assert_eq!(
            run_once(),
            baseline,
            "run {run} diverged — parallel spawn id assignment is not deterministic"
        );
    }

    // (2) Every id is unique (no double-issue across per-system blocks / overflow).
    let ids: BTreeSet<u32> = baseline.iter().map(|&(i, _, _)| i).collect();
    assert_eq!(ids.len(), baseline.len(), "entity ids must be unique");

    // (3) Structural proof of block allocation: each system's SEEDS ids form a
    //     contiguous range (its private block), independent of rank order.
    for sys in 0..3u32 {
        let mut sys_ids: Vec<u32> = baseline
            .iter()
            .filter(|&&(_, s, _)| s == sys)
            .map(|&(i, _, _)| i)
            .collect();
        sys_ids.sort_unstable();
        assert_eq!(sys_ids.len() as u32, SEEDS, "system {sys} spawned SEEDS entities");
        assert_eq!(
            sys_ids.last().unwrap() - sys_ids.first().unwrap(),
            SEEDS - 1,
            "system {sys} ids must be contiguous (drawn from one private block)"
        );
    }
}

/// Sanity: with `deterministic_spawn` OFF the engine still runs correctly (same entity
/// COUNT and content set), only the id assignment is unconstrained. Guards against the
/// opt-in accidentally changing observable spawn results.
#[test]
fn non_deterministic_mode_still_spawns_correctly() {
    let mut world = World::new();
    for i in 0..SEEDS {
        world.spawn((Seed(i),));
    }
    let mut sched = Scheduler::new();
    // deterministic_spawn left OFF (default).
    world.set_chunk_config(apex_core::world::ChunkConfig {
        auto_disable_stage_parallel: false,
        stage_parallel_min_entities: 0,
        ..Default::default()
    });
    sched.add_systems(StageLabel::Update, (spawn_a, spawn_b, spawn_c));
    sched.compile_with_world(&world).unwrap();
    sched.run(&mut world);

    let mut content: Vec<(u32, u32)> = Vec::new();
    Query::<Read<Made>>::new(&world).for_each(|_, m| content.push((m.sys, m.seed)));
    content.sort_unstable();

    let mut expected: Vec<(u32, u32)> = Vec::new();
    for sys in 0..3u32 {
        for seed in 0..SEEDS {
            expected.push((sys, seed));
        }
    }
    expected.sort_unstable();
    assert_eq!(content, expected, "all spawns land regardless of determinism mode");
}

// ── Churn capstone: deterministic reuse of freed slots under despawn+respawn ──

#[derive(Component, Clone, Copy)]
struct Child {
    tag: u32,
}

// Each frame: spawn one Child per seed (Update). The seed set is stable, so the spawn
// COUNT and content are identical every frame.
system! {
    fn churn_spawn(seeds: Read<Seed>, cmds: Cmd) {
        seeds.for_each(|_, s| { cmds.spawn((Child { tag: s.0 },)); });
    }
}

// Each frame BEFORE spawning (PreUpdate): despawn every Child from the previous frame,
// freeing their slots back to the reuse pool.
system! {
    fn churn_despawn(q: Read<Child>, cmds: Cmd) {
        q.for_each(|e, _| { cmds.despawn(e); });
    }
}

/// With `deterministic_spawn` ON, a spawn+despawn steady-state loop reuses freed entity
/// slots DETERMINISTICALLY (identical id→content mapping across independent runs) AND
/// keeps the id-space BOUNDED (reuse, not unbounded high-water growth). This is the
/// record/replay guarantee holding under realistic churn — the D8b reuse-aware frontier.
#[test]
fn deterministic_reuse_under_churn_is_reproducible_and_bounded() {
    const FRAMES: usize = 30;

    fn run() -> (Vec<(u32, u32)>, u32) {
        let mut world = World::new();
        for i in 0..SEEDS {
            world.spawn((Seed(i),));
        }
        let mut sched = Scheduler::new();
        sched.set_deterministic_spawn(true);
        world.set_chunk_config(apex_core::world::ChunkConfig {
            auto_disable_stage_parallel: false,
            stage_parallel_min_entities: 0,
            ..Default::default()
        });
        // PreUpdate despawns last frame's children; Update spawns this frame's.
        sched.add_systems(StageLabel::PreUpdate, churn_despawn);
        sched.add_systems(StageLabel::Update, churn_spawn);
        sched.compile_with_world(&world).unwrap();
        for _ in 0..FRAMES {
            sched.run(&mut world);
        }
        // Snapshot the surviving children: (entity index, tag), plus the max index.
        let mut snap: Vec<(u32, u32)> = Vec::new();
        let mut max_index = 0u32;
        Query::<Read<Child>>::new(&world).for_each(|e, c| {
            snap.push((e.index(), c.tag));
            max_index = max_index.max(e.index());
        });
        snap.sort_unstable();
        (snap, max_index)
    }

    let (snap_a, max_a) = run();
    let (snap_b, max_b) = run();

    // The surviving set is one child per seed (last frame's spawn).
    assert_eq!(snap_a.len() as u32, SEEDS, "one live child per seed after churn");
    // Determinism UNDER CHURN: identical id→content mapping across independent runs.
    assert_eq!(snap_a, snap_b, "reuse of freed slots is deterministic run-to-run");
    assert_eq!(max_a, max_b);
    // Bounded id-space: reuse keeps indices near the concurrent peak, NOT growing with
    // FRAMES. Without reuse it would be ~SEEDS·FRAMES = 480.
    assert!(
        max_a < SEEDS * 6,
        "id-space must stay bounded under churn (max_index={max_a}, SEEDS={SEEDS})"
    );
}
