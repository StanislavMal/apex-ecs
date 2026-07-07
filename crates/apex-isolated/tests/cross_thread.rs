//! Cross-thread integration tests for the world bridge.
//!
//! The inline unit tests in `lib.rs` drain the bridge on the SAME thread that
//! filled it — they never cross a thread boundary, so they don't exercise the
//! bridge's actual job: moving work between an `IsolatedWorld` on a worker
//! thread and the main `World`. These tests spawn real `std::thread`s, move one
//! half of the bridge across, and assert the ARTIFACT that arrives (entity
//! counts, deserialized events, a result echoed back) — not merely "no panic".
//!
//! Scope note (В4 / CORE_AUDIT §10.6): as of the EDITOR_GOLDEN_PATH campaign the
//! bridge is **bounded** with a backpressure policy + telemetry, and there is a
//! first-class **exchange protocol** (`apex_isolated::exchange`) that moves
//! entities between worlds with reference remapping, plus a `WorldRegistrar`
//! schema recipe. The tests below cross real thread boundaries to exercise all
//! of it — actions/events, entity transfer with remapped relations, apply-back
//! by key, and a shared-schema round-trip.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use apex_core::relations::ChildOf;
use apex_core::World;
use apex_isolated::{CloneableBridge, IsolatedWorld, WorldBridge, WorldRegistrar};
use apex_scheduler::{par, StageLabel};

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct ScoreEvent(u32);

#[derive(apex_core::Component, serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq)]
struct Tag(u32);

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

// ── Exchange protocol: entity transfer with remapping (В4) ────────

/// Transfer a whole subtree (a parent + its `ChildOf` children) from a source
/// world into a fresh world on a WORKER THREAD, and assert the relations arrive
/// REMAPPED — the children re-parent under the newly-created parent, not the old
/// (now-foreign) entity ids. This is the differentiator the renderer/editor used
/// to hand-roll (MainEntity / EditorIdMap).
#[test]
fn transfer_subtree_across_thread_remaps_relations() {
    // A shared recipe registers the component serde AND the ChildOf relation kind
    // on both worlds — relation kinds are schema too, so a receiver must know them
    // or the relations are dropped on transfer.
    let mut recipe = WorldRegistrar::new();
    recipe
        .register_component_serde_json::<Tag>()
        .register_relation_kind::<ChildOf>();

    let mut src = recipe.new_world();
    let parent = src.spawn((Tag(1),));
    let c1 = src.spawn((Tag(2),));
    let c2 = src.spawn((Tag(3),));
    src.add_relation(c1, ChildOf, parent);
    src.add_relation(c2, ChildOf, parent);

    let (count, kids) = thread::spawn(move || {
        let mut dst = recipe.new_world();
        let map = apex_isolated::exchange::transfer_entities(&src, &mut dst, &[parent]).unwrap();

        // The remapped parent is a DIFFERENT entity than the source parent.
        let new_parent = map[&parent.index()];
        // Both children re-parented under the remapped parent (relation remapped).
        let kids = dst.targets_of(ChildOf, new_parent).count();
        (dst.entity_count(), kids)
    })
    .join()
    .unwrap();

    assert_eq!(count, 3, "parent + two children all transferred");
    assert_eq!(kids, 2, "both children re-parented under the remapped parent");
}

/// A `WorldRegistrar` recipe builds two worlds with an identical serde schema, so
/// a component snapshotted in one deserializes in the other WITHOUT hand-copying
/// registrations — proven by transferring the component across a thread boundary.
#[test]
fn registrar_gives_two_worlds_a_shared_schema_across_thread() {
    let mut recipe = WorldRegistrar::new();
    recipe.register_component_serde_json::<Tag>();

    // Source world built from the recipe.
    let mut src = recipe.new_world();
    let e = src.spawn((Tag(99),));

    // Worker builds ITS world from the SAME recipe and receives the transfer.
    let value = thread::spawn(move || {
        let mut dst = recipe.new_world();
        let map = apex_isolated::exchange::transfer_entities(&src, &mut dst, &[e]).unwrap();
        dst.get::<Tag>(map[&e.index()]).map(|t| t.0)
    })
    .join()
    .unwrap();

    assert_eq!(value, Some(99), "shared-schema world deserialized the transferred component");
}

/// Apply-back across a thread: fork a world (by snapshot), edit the fork on a
/// worker, then commit the fork's changes back onto the ORIGINAL entity (keyed
/// by the same external index) — the source entity is UPDATED in place, not
/// duplicated. This is the editor preview-transaction shape (Wave 5.2) in the
/// small: only the `WorldSnapshot` (Send) crosses the boundary, the live
/// document stays put.
#[test]
fn apply_back_commits_fork_edits_onto_original_entities() {
    let mut doc = World::new();
    doc.register_component_serde_json::<Tag>();
    let e = doc.spawn((Tag(1),));
    let e_index = e.index();

    // Fork out: snapshot the document on the main thread.
    let doc_snap = apex_isolated::exchange::export_world(&doc, &mut apex_core::NoContext).unwrap();

    // Worker: rebuild the fork from the snapshot, edit it, snapshot the edit back.
    let edited = thread::spawn(move || {
        let mut fork = World::new();
        fork.register_component_serde_json::<Tag>();
        let map = apex_isolated::exchange::import(&mut fork, &doc_snap, &mut apex_core::NoContext).unwrap();
        let fork_e = map[&e_index];
        *fork.get_mut::<Tag>(fork_e).unwrap() = Tag(42);
        apex_isolated::exchange::export_world(&fork, &mut apex_core::NoContext).unwrap()
    })
    .join()
    .unwrap();

    // Apply-back onto the live document, resolving the snapshot entity to the ORIGINAL.
    let before = doc.entity_count();
    apex_isolated::exchange::apply_back(&mut doc, &edited, &mut apex_core::NoContext, &mut |idx| {
        if idx == e_index { Some(e) } else { None }
    })
    .unwrap();

    assert_eq!(doc.entity_count(), before, "apply-back updated in place, no duplicate");
    assert_eq!(doc.get::<Tag>(e).map(|t| t.0), Some(42), "the fork's edit landed on the original entity");
}
