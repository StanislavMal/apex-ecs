//! Event no-loss under (a) a reader gated across MANY frames and (b) concurrent
//! `send_sync` producers.
//!
//! The inline tests in `events.rs` already pin the single-frame lag case and
//! the sync-flush ordering. These push on the two properties that are the
//! actual differentiator (see the events design notes): a reader may skip an
//! arbitrary number of `update()` cycles and still lose nothing, and the
//! thread-safe `send_sync` escape hatch drops nothing under contention.

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

use apex_core::events::Events;

/// A reader that never reads for N frames — while events keep being produced and
/// `update()` keeps cycling — must, when it finally reads, receive EVERY event
/// in order. Retention compounds across frames as long as the reader lags; this
/// is the multi-frame extension of the single-cycle inline test.
#[test]
fn gated_reader_skipping_many_frames_loses_nothing() {
    let mut ev = Events::<u32>::new();
    let reader = ev.add_reader();

    const FRAMES: u32 = 8;
    let mut expected = Vec::new();
    for f in 0..FRAMES {
        // Two events per frame; the reader deliberately does NOT read.
        ev.send(f * 2);
        ev.send(f * 2 + 1);
        expected.push(f * 2);
        expected.push(f * 2 + 1);
        ev.update();
    }

    // Only now does the gated reader catch up — it must see all 16 in order.
    assert_eq!(
        ev.iter(&reader),
        expected.as_slice(),
        "a reader that skipped every frame must still receive every event, in order"
    );
    ev.advance_reader_mut(&reader);

    // Fully caught up: the next update drains cleanly (nothing lingers).
    ev.update();
    assert_eq!(ev.iter(&reader), &[] as &[u32]);
}

/// A caught-up reader must keep receiving new events even while ANOTHER reader
/// stays gated for many frames (the retention must not bury the new events
/// behind the lagging cursor).
#[test]
fn caught_up_reader_keeps_receiving_while_peer_lags() {
    let mut ev = Events::<u32>::new();
    let lagging = ev.add_reader();
    let active = ev.add_reader();

    for f in 0..5u32 {
        ev.send(f);
        ev.update();
        // The active reader drains every frame; the lagging one never does.
        assert_eq!(ev.iter(&active), &[f], "active reader missed frame {f}");
        ev.advance_reader_mut(&active);
    }

    // The lagging reader still holds the full history, none lost.
    assert_eq!(
        ev.iter(&lagging),
        &[0, 1, 2, 3, 4],
        "the gated reader must retain every event produced while it lagged"
    );
}

/// Many threads hammer `send_sync` (the `&self` thread-safe path) into one
/// shared `Events`; after the merge every event must be present exactly once —
/// no loss and no duplication under contention.
#[test]
fn concurrent_send_sync_loses_no_events() {
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 2000;

    let events = Arc::new(Events::<u64>::new());

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let ev = Arc::clone(&events);
            thread::spawn(move || {
                // Distinct id ranges per thread so we can assert uniqueness.
                for i in 0..PER_THREAD {
                    ev.send_sync(t * PER_THREAD + i);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // All producer clones dropped after join ⇒ we are the sole owner again.
    let mut events = Arc::try_unwrap(events)
        .unwrap_or_else(|_| panic!("all producer threads should have finished"));

    let reader = events.add_reader();
    events.update(); // merges the sync buffer into the readable buffer

    let got = events.iter(&reader);
    assert_eq!(
        got.len() as u64,
        THREADS * PER_THREAD,
        "no send_sync event may be lost under concurrency"
    );
    let unique: HashSet<u64> = got.iter().copied().collect();
    assert_eq!(
        unique.len() as u64,
        THREADS * PER_THREAD,
        "every distinct event id must arrive exactly once (no loss, no duplication)"
    );
}
