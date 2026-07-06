//! F4b (CORE_POLISH wave 2.3): the `system!` macro's event-reader sugar
//! (`EventReader<E>` / `&[E]`) uses a FRESH per-run cursor — it reads every
//! event readable that run, then frees the cursor on drop. For the common
//! per-frame system this is exactly right: events are flushed once per frame,
//! no reader lags, so the no-loss registry clears each batch after it is read
//! and the next frame's fresh cursor sees only the new events.
//!
//! What the macro path does NOT provide is a PERSISTENT per-system cursor across
//! runs WITHOUT an intervening flush (a FixedUpdate catch-up) — that is the
//! plain-fn `EventReader<E>` golden path (F4, `SystemParam::State`). `system!`
//! generates an `AutoSystem`, which structurally does not thread
//! `SystemParam::State`, so it cannot offer that persistence. This test locks
//! the SUPPORTED per-frame contract; the reassessment is recorded in
//! plans/TECH_DEBT.md (F4b).

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

/// Per-frame delivery: a `system!` event reader reads each flushed batch exactly
/// once. Because the transient reader frees its cursor on drop, no reader lags
/// between frames, so the no-loss registry clears the previous batch on the next
/// flush and the fresh per-run cursor never re-reads it.
#[test]
fn macro_event_reader_reads_each_frame_batch_once() {
    let mut world = World::new();
    world.insert_resource(PingLog::default());

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, record_pings);
    sched.compile_with_world(&world).unwrap();

    // Frame 1: two pings become readable; the system reads both.
    world.send_event(Ping(1));
    world.send_event(Ping(2));
    world.flush_all_events();
    sched.run_sequential(&mut world);

    // Frame 2: a new ping. Frame 1's batch was cleared on this flush (no lagging
    // reader), so the macro's fresh per-run cursor reads only the new event.
    world.send_event(Ping(3));
    world.flush_all_events();
    sched.run_sequential(&mut world);

    let log = world.try_resource::<PingLog>().unwrap();
    assert_eq!(
        log.0,
        vec![1, 2, 3],
        "each frame's flushed batch is read exactly once (no duplicate, no loss)"
    );
}
