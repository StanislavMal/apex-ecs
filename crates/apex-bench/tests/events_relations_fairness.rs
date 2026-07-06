//! Fairness guards for `events` and `relations`: apex and bevy MUST do the same work
//! (read all 10k events ⇒ the same sum; see all 10k children).

#[cfg(feature = "bevy")]
#[test]
fn events_apex_and_bevy_read_all_10k() {
    use apex_bench::apex::events::EventsBench;
    use apex_bench::bevy::events::Benchmark as BevyEvents;

    // sum(0..10000) = 10000*9999/2 = 49_995_000.
    const EXPECTED: u64 = 49_995_000;
    assert_eq!(EventsBench::new().run(), EXPECTED, "apex did not read all events");
    assert_eq!(BevyEvents::new().run(), EXPECTED, "bevy did not read all events");
}

/// The property that actually matters about events (correctness, not µs): a
/// reader gated to run only every 3rd frame while events are written and the
/// buffer rotated every frame. Apex preserves a lagging reader's events
/// (no-loss) — it reads ALL of them; Bevy's messages expire after 2 frames — a
/// gated reader silently DROPS the events written while it slept (a known Bevy
/// footgun). Prints the actual counts (run with `--nocapture`).
#[cfg(feature = "bevy")]
#[test]
fn gated_reader_apex_no_loss_bevy_drops() {
    use apex_bench::apex::events::gated_reader_readout as apex_gated;
    use apex_bench::bevy::events::gated_reader_readout as bevy_gated;

    let (aw, ar) = apex_gated(30, 3);
    let (bw, br) = bevy_gated(30, 3);

    assert_eq!(aw, bw, "both implementations WRITE the same");
    assert_eq!(ar, aw, "apex no-loss: gated reader must receive all {aw}, received {ar}");
    assert!(
        br < bw,
        "bevy expiry: gated reader MUST drop events — written {bw}, read {br}"
    );
    println!(
        "apex: written={aw} read={ar} (no-loss); bevy: written={bw} read={br} (dropped {})",
        bw - br
    );
}

#[cfg(feature = "bevy")]
#[test]
fn events_frame_loop_apex_and_bevy_read_all() {
    use apex_bench::apex::events::FrameLoopBench;
    use apex_bench::bevy::events::FrameLoopBenchmark as BevyFrameLoop;

    // 10_000 frames × 8 = 80_000 events; sum(0..80000) = 80000*79999/2 = 3_199_960_000.
    const EXPECTED: u64 = 3_199_960_000;
    assert_eq!(FrameLoopBench::new().run(), EXPECTED, "apex frame-loop did not read all events");
    assert_eq!(BevyFrameLoop::new().run(), EXPECTED, "bevy frame-loop did not read all events");
}

#[cfg(feature = "bevy")]
#[test]
fn relations_apex_and_bevy_see_all_10k_children() {
    use apex_bench::apex::relations::Relations;
    use apex_bench::bevy::relations::Benchmark as BevyRelations;

    assert_eq!(Relations::new().run(), 10_000, "apex did not see all children");
    assert_eq!(BevyRelations::new().run(), 10_000, "bevy did not see all children");
}
