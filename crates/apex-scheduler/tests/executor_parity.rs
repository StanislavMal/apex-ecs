//! Differential parity between the two executors, `run_sequential` and `run`.
//!
//! The scheduler intentionally keeps TWO executor paths: `run_sequential` is the
//! pure serial path (a clean baseline for the perf A/B methodology and a simple
//! reference implementation of the schedule semantics), while `run` is the
//! production path (per-system D8b command slots, the cost-model SEQ/PAR gate,
//! and ASD). Their per-stage bodies legitimately differ — the command model is
//! tied to the execution strategy. What must NEVER differ is the OBSERVABLE
//! RESULT of running the same schedule.
//!
//! These tests drive representative schedules through BOTH executors and assert
//! the resulting world state is identical. This is the guard against the two
//! stage-executor skeletons drifting (TECH_DEBT D9): a change that makes them
//! diverge in behaviour fails here, loudly (§0.2a), instead of silently making
//! the sequential baseline and production disagree.
//!
//! Assertions are over SEMANTIC state (counts, sorted component values,
//! resource values) — not raw entity ids, which may legitimately differ between
//! the shared-buffer and per-system-slot allocation strategies.

use apex_core::prelude::*;
use apex_core::query::{Read, Write};
use apex_core::world::ChunkConfig;
use apex_scheduler::{conditions, par, seq, Scheduler, StageLabel};

// A system with DECLARED access (Read<Vel>, Write<Pos>) — the scheduler sees a
// concrete component filter, so under ASD it is row-split across rayon workers.
// A raw `par(|ctx| …)` closure declares no access and would run as a single
// whole-world task, never exercising the row-split path.
apex_core::system! {
    fn move_split(q: (Read<Vel>, Write<Pos>)) {
        q.for_each_mut(|_e, (vel, mut pos)| {
            pos.x += vel.x;
            pos.y += vel.y;
        });
    }
}

#[derive(Component, Clone, Copy)]
struct Pos {
    x: i64,
    y: i64,
}

#[derive(Component, Clone, Copy)]
struct Vel {
    x: i64,
    y: i64,
}

struct Counter(u64);

/// A semantic, id-independent snapshot of observable world state.
#[derive(Debug, PartialEq, Eq)]
struct Observation {
    entity_count: usize,
    positions: Vec<(i64, i64)>,
    counter: u64,
}

fn observe(world: &World) -> Observation {
    let mut positions = Vec::new();
    world.query::<Read<Pos>>().for_each(|_e, p| positions.push((p.x, p.y)));
    positions.sort_unstable();
    Observation {
        entity_count: world.entity_count(),
        positions,
        counter: world.try_resource::<Counter>().map(|c| c.0).unwrap_or(0),
    }
}

/// Build a fresh (Scheduler, World) via `build`, run it `frames` times through
/// `exec`, and return the observation.
fn drive(
    build: &dyn Fn(&mut Scheduler, &mut World),
    exec: impl Fn(&mut Scheduler, &mut World),
    frames: usize,
) -> Observation {
    let mut sched = Scheduler::new();
    let mut world = World::new();
    build(&mut sched, &mut world);
    for _ in 0..frames {
        exec(&mut sched, &mut world);
    }
    observe(&world)
}

/// Assert both executors produce identical observable state for `build`.
fn assert_parity(build: impl Fn(&mut Scheduler, &mut World), frames: usize, label: &str) {
    let seq_obs = drive(&build, |s, w| s.run_sequential(w), frames);
    let par_obs = drive(&build, |s, w| s.run(w), frames);
    assert_eq!(
        seq_obs, par_obs,
        "executor parity broken for schedule '{label}': run_sequential != run"
    );
}

// ── Schedules ─────────────────────────────────────────────────────

/// Startup spawns entities; every Update step advances Pos by Vel. Exercises
/// startup-once, multi-frame component evolution, and a parallel query-mutation
/// system going through the SubWorld on both paths.
#[test]
fn parity_spawn_then_move() {
    assert_parity(
        |sched, world| {
            world.register_component::<Pos>();
            world.register_component::<Vel>();

            sched.add_systems(
                StageLabel::Startup,
                seq("spawn", |w: &mut World| {
                    for i in 0..64i64 {
                        w.spawn((Pos { x: i, y: -i }, Vel { x: 1, y: 2 }));
                    }
                }),
            );
            sched.add_systems(
                StageLabel::Update,
                par("move", |ctx| {
                    ctx.query_unchecked::<(Read<Vel>, Write<Pos>)>()
                        .for_each_mut(|_e, (vel, mut pos)| {
                            pos.x += vel.x;
                            pos.y += vel.y;
                        });
                }),
            );
        },
        10,
        "spawn_then_move",
    );
}

/// A stateful run condition (`run_until`) gates a counter system. Both executors
/// must evaluate the condition exactly once per frame and stop at the same count
/// (D6 stage-skip + single condition eval parity).
#[test]
fn parity_run_condition_gating() {
    assert_parity(
        |sched, world| {
            world.insert_resource(Counter(0));
            sched.add_systems(
                StageLabel::Update,
                seq("count", |w: &mut World| {
                    w.try_resource_mut::<Counter>().unwrap().0 += 1;
                })
                .run_if_cond(conditions::run_until(4)),
            );
        },
        10,
        "run_condition_gating",
    );
}

/// Spawning continues every frame while a parallel system mutates existing rows
/// — the entity set grows across frames and both executors must land on the same
/// counts and (sorted) values. Exercises the per-stage command application on
/// both the shared-buffer and per-system-slot paths.
#[test]
fn parity_growing_world_with_mutation() {
    assert_parity(
        |sched, world| {
            world.register_component::<Pos>();
            world.register_component::<Vel>();
            world.insert_resource(Counter(0));

            sched.add_systems(
                StageLabel::Update,
                seq("spawn_one", |w: &mut World| {
                    let n = w.try_resource::<Counter>().unwrap().0 as i64;
                    w.spawn((Pos { x: n, y: n }, Vel { x: 2, y: -1 }));
                    w.try_resource_mut::<Counter>().unwrap().0 += 1;
                }),
            );
            sched.add_systems(
                StageLabel::PostUpdate,
                par("move", |ctx| {
                    ctx.query_unchecked::<(Read<Vel>, Write<Pos>)>()
                        .for_each_mut(|_e, (vel, mut pos)| {
                            pos.x += vel.x;
                            pos.y += vel.y;
                        });
                }),
            );
        },
        8,
        "growing_world_with_mutation",
    );
}

/// Forces `run` down the real ASD row-split path (chunk config makes even small
/// stages split, and `move_split` declares access so it is Filtered/row-split
/// across workers). The concurrently-split parallel executor must still land on
/// exactly the same per-row result as the pure serial baseline.
#[test]
fn parity_forced_parallel_row_split() {
    assert_parity(
        |sched, world| {
            world.register_component::<Pos>();
            world.register_component::<Vel>();
            // Never fall back to serial; split into small chunks so 512 rows fan
            // out across the worker pool (mirrors the ASD row-split unit tests).
            world.set_chunk_config(ChunkConfig {
                min_entities_per_thread: 1,
                dynamic_min_chunk: 8,
                max_chunk_size: 65536,
                auto_serial_fallback: false,
                stage_parallel_min_entities: 1,
                task_multiplier: 2.0,
                ..Default::default()
            });

            sched.add_systems(
                StageLabel::Startup,
                seq("spawn", |w: &mut World| {
                    for i in 0..512i64 {
                        w.spawn((Pos { x: i, y: -i }, Vel { x: 1, y: 3 }));
                    }
                }),
            );
            sched.add_systems(StageLabel::Update, move_split);
        },
        10,
        "forced_parallel_row_split",
    );
}

/// Multi-stage ordering: three stages each mutate the same rows in sequence; the
/// cumulative per-frame delta must be identical on both executors (stage order
/// and per-stage command/flush boundaries agree).
#[test]
fn parity_multi_stage_ordering() {
    assert_parity(
        |sched, world| {
            world.register_component::<Pos>();
            world.register_component::<Vel>();

            sched.add_systems(
                StageLabel::Startup,
                seq("spawn", |w: &mut World| {
                    for i in 0..32i64 {
                        w.spawn((Pos { x: i * 10, y: 0 }, Vel { x: 3, y: 7 }));
                    }
                }),
            );
            // PreUpdate: +Vel. Update: +Vel again. PostUpdate: +Vel again.
            for stage in [StageLabel::PreUpdate, StageLabel::Update, StageLabel::PostUpdate] {
                sched.add_systems(
                    stage,
                    par("move", |ctx| {
                        ctx.query_unchecked::<(Read<Vel>, Write<Pos>)>()
                            .for_each_mut(|_e, (vel, mut pos)| {
                                pos.x += vel.x;
                                pos.y += vel.y;
                            });
                    }),
                );
            }
        },
        6,
        "multi_stage_ordering",
    );
}
