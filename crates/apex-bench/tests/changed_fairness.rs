//! `changed_iter` fairness guard: apex and bevy MUST yield the same number of changed
//! entities each frame (otherwise the bench compares different work). We run several frames.

#[cfg(all(feature = "bevy"))]
#[test]
fn changed_iter_apex_and_bevy_yield_same_count() {
    use apex_bench::apex::changed_iter::ChangedIter;
    use apex_bench::bevy::changed_iter::Benchmark as BevyChanged;

    let mut a = ChangedIter::new();
    let mut b = BevyChanged::new();

    // First frame is warmup (the engines' initial last_run may differ).
    let _ = a.run();
    let _ = b.run();

    // Steady state: both must see EXACTLY 1000 changed (10% of 10k).
    for frame in 0..5 {
        let ca = a.run();
        let cb = b.run();
        assert_eq!(ca, 1000, "apex frame {frame}: expected 1000 changed, got {ca}");
        assert_eq!(cb, 1000, "bevy frame {frame}: expected 1000 changed, got {cb}");
    }
}
