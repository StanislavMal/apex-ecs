//! Probe: WHICH of the six things `World::spawn` does per call is the `spawn_wide` gap?
//!
//! The cell reports one number for a path that allocates an id, touches the entity table THREE
//! times, reads the bundle cache, pushes into the archetype, copies each component's bytes, pushes
//! two `TickCell`s per component and raises two aggregates per component. One number over six
//! works names none of them (CONVENTIONS §2, lesson 25). This walks the same 10k spawns once per
//! rung — each rung one stage further into the real body — and the DIFFERENCES name the stages.
//!
//! And it turns the ladder by the parameter the cell holds FIXED: the cell always spawns FOUR
//! components, so nothing in it can say which stages are paid per ENTITY and which per COMPONENT.
//! Here the same ladder runs at widths 1/2/4/8 and each rung's delta is fitted against width:
//! the intercept is what the stage costs an entity, the slope what it costs a component. A stage
//! that turns out to be per-component is fixed by a different edit from one that is per-entity.
//!
//! Read the DIFFERENCES between rungs, never a rung's absolute against the live path. The last
//! two rows are the same ladder's top step and the REAL `World::spawn`: they must agree, and
//! their gap is printed as `copy drift` — a ladder whose copy does not meet the original is not
//! describing the original.
//!
//! Run: `cargo run --release -p apex-bench --bin spawn_ladder --features bevy`
//! Tuning: `COUNT=10000 REPEATS=9`

use apex_core::world::spawn_ladder::{self, RUNGS};
use apex_core::prelude::*;
use apex_macros::Component;
use std::time::Instant;

// A uniform 16-byte payload per component, so a width sweep changes the WIDTH and nothing else.
// (The cell's own bundle is different — 64 B + 3x12 B — and is run separately at the end.)
macro_rules! payload {
    ($($name:ident),+) => {$(
        #[derive(Component, Clone, Copy)]
        pub struct $name(pub [f32; 4]);
    )+};
}
payload!(W1, W2, W3, W4, W5, W6, W7, W8);

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Median and INTER-QUARTILE half-spread (as a percent of the median).
///
/// Not `(max-min)/2`: every pass here asks the OS for about a megabyte and hands it back, so one
/// pass in ten pays for page faults that have nothing to do with the stage under test. A spread
/// taken from the extremes reports that one pass and hides the other fourteen; the quartiles say
/// how tightly the BODY of the sample sits, which is what decides whether a delta is readable.
fn median_spread(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = v[v.len() / 2];
    let q1 = v[v.len() / 4];
    let q3 = v[v.len() * 3 / 4];
    let spread = (q3 - q1) / 2.0 / med.max(f64::MIN_POSITIVE) * 100.0;
    (med, spread)
}

/// Least-squares fit of `y = intercept + slope * width`.
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

/// One timed pass: a FRESH world (built and dropped outside the clock) spawning `count` entities
/// with the stages up to `rung`. One stamp per pass — a probe that stamped per spawn would
/// measure itself on exactly the stand it exists for.
fn pass<B, F>(count: usize, rung: u8, make: F) -> f64
where
    B: Bundle,
    F: FnMut(usize) -> B,
{
    let mut world = World::new();
    let t = Instant::now();
    spawn_ladder::spawn_rung::<B, F>(&mut world, count, rung, make);
    let ns = t.elapsed().as_secs_f64() * 1e9 / count as f64;
    std::hint::black_box(&world);
    drop(world);
    ns
}

/// The whole ladder for one bundle type: median ns per spawn at each rung.
///
/// The rungs are INTERLEAVED — one pass of every rung, then the next repeat — never all of one
/// rung's passes and then all of the next's. This machine drifts between minutes; a ladder that
/// measured its rungs in blocks would charge that drift to whichever rung was running when it
/// happened, and the delta between two rungs is the whole output of the instrument. (The first
/// version of this probe did measure in blocks: it produced negative deltas and spreads to 66 %.)
fn ladder<B, F>(count: usize, repeats: usize, make: F) -> Vec<(f64, f64)>
where
    B: Bundle,
    F: FnMut(usize) -> B + Copy,
{
    let mut samples: Vec<Vec<f64>> = vec![Vec::with_capacity(repeats); RUNGS.len()];
    // One warm pass of every rung before the clock counts (the first world's page faults are not
    // a stage of spawning).
    for rung in 0..RUNGS.len() as u8 {
        std::hint::black_box(pass::<B, F>(count.min(1000), rung, make));
    }
    for _ in 0..repeats {
        for rung in 0..RUNGS.len() as u8 {
            samples[rung as usize].push(pass::<B, F>(count, rung, make));
        }
    }
    samples.into_iter().map(median_spread).collect()
}

fn print_ladder(title: &str, rows: &[(f64, f64)]) {
    println!("\n{title}");
    println!("{:>4} {:<38} {:>10} {:>7} {:>10}", "rung", "stage", "ns/spawn", "+-%", "delta");
    let mut prev = 0.0;
    for (i, &(med, spread)) in rows.iter().enumerate() {
        let delta = med - prev;
        let delta_s = if i == 0 {
            String::from("-")
        } else if i as u8 == spawn_ladder::REAL {
            format!("{delta:+.1} drift")
        } else {
            format!("{delta:+.1}")
        };
        println!("{i:>4} {:<38} {med:>10.1} {spread:>6.1}% {delta_s:>10}", RUNGS[i]);
        prev = med;
    }
    let copy = rows[spawn_ladder::COPY_COMPLETE as usize].0;
    let real = rows[spawn_ladder::REAL as usize].0;
    println!(
        "     copy drift: real {real:.1} vs complete copy {copy:.1} = {:+.1}% \
         (the copy runs one loop over the columns PER STAGE where production interleaves them \
         per component -- this line is that price, measured)",
        (real - copy) / copy * 100.0
    );
}

/// The same 10k spawns on the reference engine, for the bar the numbers are read against. Bevy's
/// path cannot be cut into rungs from here, so only its total is available -- which is exactly the
/// quantity our top rung must be compared with. Its own module: `bevy_ecs::prelude` and
/// `apex_macros` both export a `Component` derive, and a glob of one over the other is ambiguous.
#[cfg(feature = "bevy")]
mod reference {
    use super::median_spread;
    use bevy_ecs::prelude::*;
    use std::time::Instant;

    // The payload is written and never read back, exactly as in the cells (`spawn_wide`); the
    // measurement is the write.
    macro_rules! bevy_payload {
        ($($name:ident),+) => {$(
            #[derive(Component)]
            #[allow(dead_code)]
            struct $name([f32; 4]);
        )+};
    }
    bevy_payload!(V1, V2, V3, V4, V5, V6, V7, V8);

    macro_rules! run {
        ($count:expr, $bundle:expr) => {{
            let mut world = World::new();
            let t = Instant::now();
            for _ in 0..$count {
                world.spawn($bundle);
            }
            let ns = t.elapsed().as_secs_f64() * 1e9 / $count as f64;
            std::hint::black_box(&world);
            drop(world);
            ns
        }};
    }

    /// ONE pass, so the caller can alternate it with ours inside a single window. A ratio between
    /// an absolute taken here and an absolute taken minutes ago is not a ratio: this machine is a
    /// different machine in the evening than at night, and the earlier shape of this probe read
    /// 0.47x where alternation reads something else entirely.
    pub fn spawn_pass(count: usize, width: usize) -> f64 {
        let d = [0.0f32; 4];
        match width {
            1 => run!(count, (V1(d),)),
            2 => run!(count, (V1(d), V2(d))),
            4 => run!(count, (V1(d), V2(d), V3(d), V4(d))),
            8 => run!(count, (V1(d), V2(d), V3(d), V4(d), V5(d), V6(d), V7(d), V8(d))),
            _ => f64::NAN,
        }
    }
}

#[cfg(feature = "bevy")]
fn bevy_spawn_pass(count: usize, width: usize) -> f64 {
    reference::spawn_pass(count, width)
}

#[cfg(not(feature = "bevy"))]
fn bevy_spawn_pass(_count: usize, _width: usize) -> f64 {
    f64::NAN
}

/// The bar, taken by strict alternation of the two engines' spawn loops inside one window: one
/// apex pass, one bevy pass, repeat. Returns `(apex, bevy)` medians.
fn bar<B, F>(count: usize, repeats: usize, width: usize, make: F) -> (f64, f64)
where
    B: Bundle,
    F: FnMut(usize) -> B + Copy,
{
    std::hint::black_box(pass::<B, F>(count.min(1000), spawn_ladder::REAL, make));
    std::hint::black_box(bevy_spawn_pass(count.min(1000), width));
    let mut a = Vec::with_capacity(repeats);
    let mut b = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        a.push(pass::<B, F>(count, spawn_ladder::REAL, make));
        b.push(bevy_spawn_pass(count, width));
    }
    (median_spread(a).0, median_spread(b).0)
}

fn main() {
    let count = env_usize("COUNT", 10_000);
    let repeats = env_usize("REPEATS", 9);
    let d = [0.0f32; 4];

    println!(
        "spawn ladder: {count} one-at-a-time spawns per pass, {repeats} passes per rung, \
         ns PER SPAWN (median). Read the DELTAS."
    );

    let widths = [1usize, 2, 4, 8];
    let l1 = ladder::<(W1,), _>(count, repeats, move |_| (W1(d),));
    let l2 = ladder::<(W1, W2), _>(count, repeats, move |_| (W1(d), W2(d)));
    let l4 = ladder::<(W1, W2, W3, W4), _>(count, repeats, move |_| (W1(d), W2(d), W3(d), W4(d)));
    let l8 = ladder::<(W1, W2, W3, W4, W5, W6, W7, W8), _>(count, repeats, move |_| {
        (W1(d), W2(d), W3(d), W4(d), W5(d), W6(d), W7(d), W8(d))
    });
    let ladders = [&l1, &l2, &l4, &l8];

    for (w, l) in widths.iter().zip(ladders.iter()) {
        print_ladder(&format!("width {w} (16 B per component)"), l);
    }

    // The payoff of turning the ladder: which stages are paid per ENTITY and which per COMPONENT.
    println!(
        "\nper-stage fit over widths 1/2/4/8 -- 'per entity' is the intercept, 'per component' \
         the slope (ns):"
    );
    println!("{:>4} {:<38} {:>12} {:>14}", "rung", "stage", "per entity", "per component");
    for r in 1..RUNGS.len() {
        let pts: Vec<(f64, f64)> = widths
            .iter()
            .zip(ladders.iter())
            .map(|(&w, l)| (w as f64, l[r].0 - l[r - 1].0))
            .collect();
        let (fixed, per) = fit(&pts);
        println!("{r:>4} {:<38} {fixed:>12.2} {per:>14.2}", RUNGS[r]);
    }

    // The bar, alternated. Our loop is the same statement bevy's is; the ratio is the cell's
    // subject, and it is only a ratio if both halves sat in the same window.
    println!("\nagainst the reference at the same widths, ALTERNATED (ns per spawn):");
    println!("{:>6} {:>12} {:>12} {:>10}", "width", "apex", "bevy", "x bevy");
    let d = [0.0f32; 4];
    let bars = [
        bar::<(W1,), _>(count, repeats, 1, move |_| (W1(d),)),
        bar::<(W1, W2), _>(count, repeats, 2, move |_| (W1(d), W2(d))),
        bar::<(W1, W2, W3, W4), _>(count, repeats, 4, move |_| (W1(d), W2(d), W3(d), W4(d))),
        bar::<(W1, W2, W3, W4, W5, W6, W7, W8), _>(count, repeats, 8, move |_| {
            (W1(d), W2(d), W3(d), W4(d), W5(d), W6(d), W7(d), W8(d))
        }),
    ];
    for (w, &(apex, bevy)) in widths.iter().zip(bars.iter()) {
        if bevy.is_nan() {
            println!("{w:>6} {apex:>12.1} {:>12} {:>10}", "-", "-");
        } else {
            println!("{w:>6} {apex:>12.1} {bevy:>12.1} {:>10.2}", bevy / apex);
        }
    }

    // Cross-check on the CELL'S OWN bundle (Matrix4 + 3x Vector3, 100 B over four components):
    // if the uniform-payload sweep and the cell's shape disagree about which stage dominates,
    // the sweep is describing its own payload size rather than the path.
    {
        use apex_bench::{Position, Rotation, Transform, Velocity};
        use cgmath::{Matrix4, Vector3};
        let l = ladder::<(Transform, Position, Rotation, Velocity), _>(count, repeats, |_| {
            (
                Transform(Matrix4::from_scale(1.0)),
                Position(Vector3::unit_x()),
                Rotation(Vector3::unit_x()),
                Velocity(Vector3::unit_x()),
            )
        });
        print_ladder("the `spawn_wide` cell's OWN bundle (4 components, 100 B)", &l);
    }
}
