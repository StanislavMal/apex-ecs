//! Cross-thread integration tests for the world bridge.
//!
//! The inline unit tests in `lib.rs` drain the bridge on the SAME thread that
//! filled it — they never cross a thread boundary, so they don't exercise the
//! bridge's actual job: moving work between an `IsolatedWorld` on a worker
//! thread and the main `World`. These tests spawn real `std::thread`s, move one
//! half of the bridge across, and assert the ARTIFACT that arrives (entity
//! counts, deserialized events, a result echoed back) — not merely "no panic".
//!
//! Scope note (В4 / CORE_AUDIT §10.6): the bridge today is `unbounded` and has
//! no entity remapping. Bounded channels with telemetry and a remap protocol
//! are still design debt — these tests pin the EXISTING mechanism across
//! threads so that work can build on a verified baseline.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use apex_core::World;
use apex_isolated::{CloneableBridge, IsolatedWorld, WorldBridge};
use apex_scheduler::{par, StageLabel};

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct ScoreEvent(u32);

/// Result echoed from the worker back into the main world.
struct WorkerResult(usize);

// ── Actions cross a thread boundary without loss ──────────────────

/// Queue N structural actions, then hand the receiving half to a worker
/// thread which drains it into its own world. All N must apply — the channel
/// buffers across the boundary and nothing is dropped.
#[test]
fn actions_sent_before_worker_starts_all_apply_across_thread() {
    let (main, sub) = WorldBridge::new();
    const N: usize = 200;

    for _ in 0..N {
        main.send_action(Box::new(|w: &mut World| {
            w.spawn(());
        }));
    }

    let count = thread::spawn(move || {
        let mut world = World::new();
        sub.apply_incoming(&mut world);
        world.entity_count()
    })
    .join()
    .unwrap();

    assert_eq!(count, N, "every action must cross the thread boundary and apply");
}

// ── A running worker loop, with a result echoed back ──────────────

/// A worker thread runs a drain loop over a live stream of actions, then — once
/// told to stop — does a final drain and echoes its entity count back to the
/// main world through the reverse direction of the SAME bridge pair. Exercises
/// both directions across the boundary and asserts the echoed artifact.
#[test]
fn worker_loop_processes_stream_and_echoes_result_back() {
    let (main, sub) = WorldBridge::new();
    const N: usize = 50;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);

    let worker = thread::spawn(move || {
        let mut world = World::new();
        while !stop_worker.load(Ordering::Acquire) {
            sub.apply_incoming(&mut world);
            thread::yield_now();
        }
        // Final drain: `stop` is set only after ALL sends, and crossbeam FIFO +
        // the stop Release/Acquire pair guarantee every queued action is now
        // visible, so this collects any that arrived during the last iteration.
        sub.apply_incoming(&mut world);

        let count = world.entity_count();
        sub.send_action(Box::new(move |mw: &mut World| {
            mw.insert_resource(WorkerResult(count));
        }));
    });

    for _ in 0..N {
        main.send_action(Box::new(|w: &mut World| {
            w.spawn(());
        }));
    }
    // Ordering: the stop flag is stored AFTER all sends (program order), so the
    // worker's final drain observes every one of them.
    stop.store(true, Ordering::Release);
    worker.join().unwrap();

    // Pull the echoed result out of the reverse channel.
    let mut main_world = World::new();
    main.apply_incoming(&mut main_world);
    assert_eq!(
        main_world.try_resource::<WorkerResult>().map(|r| r.0),
        Some(N),
        "the worker's entity count must be echoed back across the boundary"
    );
}

// ── Typed events deserialize on the far side of the boundary ──────

/// `send_event` serializes with bincode; the worker registers the type and
/// deserializes it during drain. Proves the serialized wire path works across a
/// real thread, not just in-process.
#[test]
fn typed_events_deserialize_on_worker_thread() {
    let (main, sub) = WorldBridge::new();

    main.send_event(&ScoreEvent(7));
    main.send_event(&ScoreEvent(8));

    let received = thread::spawn(move || {
        let mut world = World::new();
        // Registration can happen after the bytes are already queued — it only
        // needs to be in place before the drain deserializes them.
        sub.register_event::<ScoreEvent>(&mut world);
        sub.apply_incoming(&mut world);
        world.events::<ScoreEvent>().len()
    })
    .join()
    .unwrap();

    assert_eq!(received, 2, "both typed events crossed the thread and deserialized");
}

// ── Concurrent producers: no-loss under contention ────────────────

/// Many producer threads hammer one shared `CloneableBridge`; a single consumer
/// drains the lot. The exact total must arrive — the no-loss guarantee under
/// concurrent senders (our channel is MPSC-safe), demonstrated through the
/// bridge API rather than raw crossbeam.
#[test]
fn concurrent_producers_lose_no_actions() {
    const PRODUCERS: usize = 8;
    const PER_PRODUCER: usize = 250;

    // Two linked directions, exactly as `WorldBridge::new` builds internally:
    // producers send on `forward`, the consumer drains it; the reverse pair is
    // unused here but required to construct a bidirectional bridge.
    let (forward_tx, forward_rx) = crossbeam_channel::unbounded();
    let (reverse_tx, reverse_rx) = crossbeam_channel::unbounded();
    let producer = CloneableBridge::new(forward_tx, reverse_rx);
    let consumer = CloneableBridge::new(reverse_tx, forward_rx);

    let handles: Vec<_> = (0..PRODUCERS)
        .map(|_| {
            let p = producer.clone();
            thread::spawn(move || {
                for _ in 0..PER_PRODUCER {
                    p.send_action(Box::new(|w: &mut World| {
                        w.spawn(());
                    }));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // Drop the original producer so no live sender lingers (not required for a
    // one-shot drain, but keeps the intent — all producers are done).
    drop(producer);

    let mut world = World::new();
    consumer.apply_incoming(&mut world);
    assert_eq!(
        world.entity_count(),
        PRODUCERS * PER_PRODUCER,
        "every action from every producer thread must arrive exactly once"
    );
}

// ── Peer teardown across a thread: graceful drop, no panic ────────

/// The receiving half is dropped when a worker thread exits; subsequent sends
/// from the main thread hit a closed channel and must be dropped gracefully
/// (surfaced via `warn_once`), never panic.
#[test]
fn send_after_worker_drops_receiver_is_graceful() {
    let (main, sub) = WorldBridge::new();

    // Worker takes ownership of `sub` and immediately drops it on exit,
    // closing the channel `main` sends into.
    thread::spawn(move || {
        let _sub = sub;
    })
    .join()
    .unwrap();

    // These now hit a closed channel — no panic, action/event simply dropped.
    main.send_action(Box::new(|_w: &mut World| {}));
    main.send_event(&ScoreEvent(1));
}

// ── IsolatedWorld runs its scheduler on a worker thread ───────────

/// `IsolatedWorld` is `Send` (single-thread-at-a-time contract). Move a fully
/// configured one onto a worker thread, tick it there, and assert the system
/// observed the world it owns — the isolated-simulation-on-another-thread use
/// case end to end.
#[test]
fn isolated_world_ticks_on_worker_thread() {
    let observed = thread::spawn(|| {
        let mut iso = IsolatedWorld::new();

        let seen = Arc::new(AtomicUsize::new(0));
        let seen_sys = Arc::clone(&seen);
        iso.scheduler_mut().add_systems(
            StageLabel::Update,
            par("observe_count", move |ctx| {
                seen_sys.store(ctx.entity_count(), Ordering::SeqCst);
            }),
        );

        iso.world_mut().spawn(());
        iso.world_mut().spawn(());
        iso.tick();

        seen.load(Ordering::SeqCst)
    })
    .join()
    .unwrap();

    assert_eq!(observed, 2, "the isolated scheduler ran against its own world on the worker thread");
}
