//! Probe: is the `events_frame_loop` gap a PER-EVENT price or a PER-FRAME one?
//!
//! The cell (`benches/benchmarks.rs`, group `events_frame_loop`) runs 10 000 frames of
//! `8 send + read with a persistent cursor + rotate` and reports one number — 15.6 ns/frame for
//! apex against 10.3 for bevy (0.66x). One number at ONE batch size cannot say whether we pay
//! per event (the send/read path) or per frame (the rotate/cursor path), and the two are fixed
//! by opposite means.
//!
//! So the probe walks a LADDER of batch sizes and reports, at each rung, the SAME loop the cell
//! runs — for both engines, alternated A/B inside one window (an absolute taken minutes apart on
//! this machine is not comparable; a delta taken by alternation is). The straight line through
//! the rungs then splits the cost: the intercept is what a frame costs with no events in it
//! (rotate + cursor bookkeeping), the slope is what one event costs (push + iterate).
//!
//! A FIRST version of this probe tried to difference three loop variants (`send only`,
//! `send+rotate`, `full`). That was wrong and is recorded here so it is not tried again: with no
//! rotate the buffer never drains, so `send only` grows to frames x N events and its cost is
//! dominated by allocation and page faults — the differences came out NEGATIVE. A variant that
//! changes the memory profile is not a phase of the original work.
//!
//! Run: `cargo run --release -p apex-bench --bin events_shapes --features bevy`
//! Tuning: `FRAMES=10000 REPEATS=7 RUNGS=1,8,64,512`

use apex_core::events::Events;
use std::time::Instant;

#[derive(Clone, Copy)]
struct E(u64);

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The cell's own loop, apex side: N sends, rotate, read with a persistent cursor.
fn apex_loop(frames: usize, per_frame: usize) -> u64 {
    let mut events = Events::<E>::new();
    let cursor = events.add_reader();
    let mut sum = 0u64;
    let mut n = 0u64;
    for _ in 0..frames {
        for _ in 0..per_frame {
            events.send(E(n));
            n += 1;
        }
        events.update();
        for e in events.read(&cursor).iter() {
            sum += e.0;
        }
    }
    sum
}

/// The same loop, bevy side. Bevy rotates AFTER reading (its `update` drops the buffer two
/// rotations old, so a reader that runs before the rotate sees exactly one frame of messages) —
/// this is the order bevy's own bench uses, and the same work per frame.
#[cfg(feature = "bevy")]
fn bevy_loop(frames: usize, per_frame: usize) -> u64 {
    use bevy_ecs::message::{Message, Messages};

    #[derive(Message)]
    struct M(u64);

    let mut messages = Messages::<M>::default();
    let mut cursor = messages.get_cursor();
    let mut sum = 0u64;
    let mut n = 0u64;
    for _ in 0..frames {
        for _ in 0..per_frame {
            messages.write(M(n));
            n += 1;
        }
        for m in cursor.read(&messages) {
            sum += m.0;
        }
        messages.update();
    }
    sum
}

fn time_ns_per_frame(frames: usize, f: impl FnOnce() -> u64) -> f64 {
    let t = Instant::now();
    let out = f();
    let d = t.elapsed();
    std::hint::black_box(out);
    d.as_secs_f64() * 1e9 / frames as f64
}

/// Median and half-spread (as a percent of the median) of a sample set.
fn median_spread(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = v[v.len() / 2];
    let spread = (v[v.len() - 1] - v[0]) / 2.0 / med.max(f64::MIN_POSITIVE) * 100.0;
    (med, spread)
}

/// Least-squares fit of `y = intercept + slope * n` over the rungs. The intercept is the
/// per-FRAME cost (rotate + cursor bookkeeping), the slope the per-EVENT cost.
fn fit(points: &[(f64, f64)]) -> (f64, f64) {
    let k = points.len() as f64;
    let sx: f64 = points.iter().map(|p| p.0).sum();
    let sy: f64 = points.iter().map(|p| p.1).sum();
    let sxx: f64 = points.iter().map(|p| p.0 * p.0).sum();
    let sxy: f64 = points.iter().map(|p| p.0 * p.1).sum();
    let denom = k * sxx - sx * sx;
    if denom.abs() < f64::EPSILON {
        return (sy / k, 0.0);
    }
    let slope = (k * sxy - sx * sy) / denom;
    ((sy - slope * sx) / k, slope)
}

/// The criterion cell's OWN structs, called from this binary. If these disagree with the local
/// copies above — which run the identical statements — then the difference is codegen/harness,
/// not the code under test, and the cell is measuring its own layout. That is a finding about
/// the instrument, so it is measured rather than assumed.
fn lib_cell() -> (f64, f64) {
    const CELL_FRAMES: usize = 10_000; // the structs' own shape: 10k frames x 8 events
    use apex_bench::apex::events::FrameLoopBench;
    let mut a = FrameLoopBench::new();
    let at = time_ns_per_frame(CELL_FRAMES, || a.run());
    #[cfg(feature = "bevy")]
    let bt = {
        use apex_bench::bevy::events::FrameLoopBenchmark;
        let mut b = FrameLoopBenchmark::new();
        time_ns_per_frame(CELL_FRAMES, || b.run())
    };
    #[cfg(not(feature = "bevy"))]
    let bt = f64::NAN;
    (at, bt)
}

fn main() {
    let frames = env_usize("FRAMES", 10_000);
    let repeats = env_usize("REPEATS", 7);
    let rungs: Vec<usize> = std::env::var("RUNGS")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 8, 64, 512]);

    println!(
        "events frame loop: {frames} frames, {repeats} alternated repeats, ns PER FRAME (median)"
    );
    println!(
        "\n{:>6} {:>12} {:>8} {:>12} {:>8} {:>8}",
        "N/fr", "apex", "+-%", "bevy", "+-%", "x bevy"
    );

    let mut apex_pts = Vec::new();
    let mut bevy_pts: Vec<(f64, f64)> = Vec::new();
    for &per_frame in &rungs {
        let mut a = Vec::with_capacity(repeats);
        #[allow(unused_mut)]
        let mut b: Vec<f64> = Vec::with_capacity(repeats);
        // One warm pass of each before the clock counts, then strict alternation: the machine
        // drifts between minutes, so A and B must sit inside the same one.
        std::hint::black_box(apex_loop(frames.min(1000), per_frame));
        #[cfg(feature = "bevy")]
        std::hint::black_box(bevy_loop(frames.min(1000), per_frame));
        for _ in 0..repeats {
            a.push(time_ns_per_frame(frames, || apex_loop(frames, per_frame)));
            #[cfg(feature = "bevy")]
            b.push(time_ns_per_frame(frames, || bevy_loop(frames, per_frame)));
        }
        let (am, asp) = median_spread(a);
        apex_pts.push((per_frame as f64, am));
        if b.is_empty() {
            println!(
                "{per_frame:>6} {am:>12.2} {asp:>7.1}% {:>12} {:>8} {:>8}",
                "-", "-", "-"
            );
            continue;
        }
        let (bm, bsp) = median_spread(b);
        bevy_pts.push((per_frame as f64, bm));
        println!(
            "{per_frame:>6} {am:>12.2} {asp:>7.1}% {bm:>12.2} {bsp:>7.1}% {:>8.2}",
            bm / am
        );
    }

    // Cross-check against the criterion cell's own structs at its own rung (8 events/frame).
    {
        let (mut la, mut lb) = (Vec::new(), Vec::new());
        std::hint::black_box(lib_cell());
        for _ in 0..repeats {
            let (a, b) = lib_cell();
            la.push(a);
            lb.push(b);
        }
        let (lam, lasp) = median_spread(la);
        let (lbm, lbsp) = median_spread(lb);
        println!(
            "
cell structs @8/frame: apex {lam:.2} (+-{lasp:.1}%) | bevy {lbm:.2}              (+-{lbsp:.1}%) | x bevy {:.2}",
            lbm / lam
        );
    }

    let (a_fix, a_per) = fit(&apex_pts);
    println!("\napex : {a_fix:.2} ns per frame with no events in it + {a_per:.3} ns per event");
    if !bevy_pts.is_empty() {
        let (b_fix, b_per) = fit(&bevy_pts);
        println!("bevy : {b_fix:.2} ns per frame with no events in it + {b_per:.3} ns per event");
        println!(
            "\nfixed per-frame cost: apex {:.2}x bevy | per-event cost: apex {:.2}x bevy \
             (>1 = apex cheaper)",
            b_fix / a_fix.max(f64::MIN_POSITIVE),
            b_per / a_per.max(f64::MIN_POSITIVE)
        );
    }
}
