//! F4b (CORE_POLISH wave 2.3): a `system!` macro event reader now holds a
//! PERSISTENT per-system cursor — the AutoSystem analogue of the plain-fn
//! `EventReader<E>` golden path (F4). The cursor is owned by the AutoSystem
//! adapter (survives across runs) and installed into the `SystemContext` per run,
//! so a FixedUpdate catch-up (several runs in one frame without a flush) does NOT
//! re-read events, exactly like a plain-fn reader.
//!
//! Event-reading AutoSystems are forced single-task (`stateful`), so the
//! adapter-owned cursor store is touched by exactly one task (a shared cursor
//! across ASD row-split chunks would be wrong).

use apex_core::prelude::*;
use apex_core::{system, World};
use apex_scheduler::{Scheduler, StageLabel};

#[derive(Clone, Copy)]
struct Ping(u32);

#[derive(Default)]
struct PingLog(Vec<u32>);

system! {
    fn record_pings(evs: EventReader<Ping>, log: ResMut<PingLog>) {
        for p in evs.read().into_iter() {
            log.0.push(p.0);
        }
    }
}

fn setup() -> (World, Scheduler) {
    let mut world = World::new();
    world.insert_resource(PingLog::default());
    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, record_pings);
    sched.compile_with_world(&world).unwrap();
    (world, sched)
}

/// Per-frame delivery: each flushed batch is read exactly once across frames.
#[test]
fn macro_event_reader_reads_each_frame_batch_once() {
    let (mut world, mut sched) = setup();

    world.send_event(Ping(1));
    world.send_event(Ping(2));
    world.flush_all_events();
    sched.run_sequential(&mut world);

    world.send_event(Ping(3));
    world.flush_all_events();
    sched.run_sequential(&mut world);

    let log = world.try_resource::<PingLog>().unwrap();
    assert_eq!(
        log.0,
        vec![1, 2, 3],
        "each frame's flushed batch is read exactly once"
    );
}

/// F4b: a FixedUpdate-style catch-up — the SAME system runs several times in one
/// frame WITHOUT an intervening flush. The persistent cursor resumes, so the
/// events are read exactly ONCE across the extra runs (a fresh transient cursor
/// would restart at zero every run and duplicate them — the bug F4b closes).
#[test]
fn macro_event_reader_persists_across_runs_no_duplicate() {
    let (mut world, mut sched) = setup();

    world.send_event(Ping(10));
    world.send_event(Ping(20));
    world.flush_all_events();

    // Three runs in the same frame (no flush between) — a FixedUpdate catch-up.
    sched.run_sequential(&mut world);
    sched.run_sequential(&mut world);
    sched.run_sequential(&mut world);

    let log = world.try_resource::<PingLog>().unwrap();
    assert_eq!(
        log.0,
        vec![10, 20],
        "persistent cursor: the batch is read once total across the catch-up runs, not once per run"
    );
}
