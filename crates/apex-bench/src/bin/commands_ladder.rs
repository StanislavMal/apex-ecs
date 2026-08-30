//! Probe: WHAT does deferring an insert through `Commands` cost, and is that cost ours?
//!
//! `commands_insert` runs 0.89x the reference while `add_remove_component` — the SAME 10k inserts
//! taken directly through `World::insert` — runs 1.58x AHEAD of it. That contrast said the
//! subject is the machinery around the archetype move and not the move itself, but it said it by
//! SUBTRACTION between two cells measured apart, and a subtraction is not a measurement.
//!
//! So this probe measures the machinery directly, and on BOTH engines:
//!
//! - the record half (arena + queue) and the apply half (walk + payload + id + insert) are timed
//!   apart, each broken into rungs of the real body, because they are fixed by opposite means;
//! - the control arm is the same inserts with no Commands in the way;
//! - the reference runs both arms too, so the answer is `what deferring costs US` against
//!   `what deferring costs THEM` — a number that means something — instead of one absolute.
//!
//! Read the DIFFERENCES between rungs. Each half's top rung is the REAL production call, and the
//! gap to the complete copy below it is printed as `copy drift`.
//!
//! Run: `cargo run --release -p apex-bench --bin commands_ladder --features bevy`
//! Tuning: `COUNT=10000 REPEATS=9`

use apex_core::commands::insert_ladder::{self, APPLY_RUNGS, RECORD_RUNGS};
use apex_core::prelude::*;
use apex_macros::Component;
use std::time::Instant;

/// The cell's own components: the entities carry `A`, the deferred command adds `B`.
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);
#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

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

/// The cell's setup: a world of `count` entities carrying `A`. Built outside every clock.
fn setup(count: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    let entities = world.spawn_many(count, |_| (A(0.0),));
    (world, entities)
}

/// Median ns per insert over `repeats` timed passes of `f`, with one warm pass first.
fn timed(repeats: usize, mut f: impl FnMut() -> f64) -> (f64, f64) {
    std::hint::black_box(f());
    let mut samples = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        samples.push(f());
    }
    median_spread(samples)
}

/// A whole ladder, rungs INTERLEAVED inside one window. This machine drifts between minutes; a
/// ladder measured in blocks charges that drift to whichever rung was running at the time, and
/// the delta between rungs is the entire output of the instrument.
fn ladder(rungs: usize, repeats: usize, mut pass: impl FnMut(u8) -> f64) -> Vec<(f64, f64)> {
    let mut samples: Vec<Vec<f64>> = vec![Vec::with_capacity(repeats); rungs];
    for rung in 0..rungs as u8 {
        std::hint::black_box(pass(rung));
    }
    for _ in 0..repeats {
        for rung in 0..rungs as u8 {
            samples[rung as usize].push(pass(rung));
        }
    }
    samples.into_iter().map(median_spread).collect()
}

fn print_ladder(title: &str, names: &[&str], rows: &[(f64, f64)], copy_complete: usize) {
    println!("\n{title}");
    println!("{:>4} {:<36} {:>10} {:>7} {:>10}", "rung", "stage", "ns/insert", "+-%", "delta");
    let mut prev = 0.0;
    for (i, &(med, spread)) in rows.iter().enumerate() {
        let delta = if i == 0 {
            String::from("-")
        } else if i == copy_complete + 1 {
            format!("{:+.1} drift", med - prev)
        } else {
            format!("{:+.1}", med - prev)
        };
        println!("{i:>4} {:<36} {med:>10.1} {spread:>6.1}% {delta:>10}", names[i]);
        prev = med;
    }
}

// ── The reference's two arms ─────────────────────────────────────

#[cfg(feature = "bevy")]
mod reference {
    use super::median_spread;
    use bevy_ecs::prelude::*;
    use bevy_ecs::world::CommandQueue;
    use std::time::Instant;

    // Written and never read back, exactly as in the cell (`commands_insert`).
    #[derive(Component)]
    #[allow(dead_code)]
    struct A(f32);
    #[derive(Component)]
    #[allow(dead_code)]
    struct B(f32);

    fn setup(count: usize) -> (World, Vec<Entity>) {
        let mut world = World::new();
        let entities = world
            .spawn_batch((0..count).map(|_| (A(0.0),)))
            .collect::<Vec<_>>();
        (world, entities)
    }

    /// Straight into the world — the reference's control arm.
    pub fn direct(count: usize, repeats: usize) -> (f64, f64) {
        let mut samples = Vec::with_capacity(repeats);
        for _ in 0..=repeats {
            let (mut world, entities) = setup(count);
            let t = Instant::now();
            for &e in &entities {
                world.entity_mut(e).insert(B(1.0));
            }
            let ns = t.elapsed().as_secs_f64() * 1e9 / count as f64;
            std::hint::black_box(&world);
            samples.push(ns);
        }
        samples.remove(0);
        median_spread(samples)
    }

    /// Record and apply, split — the reference's deferred arm, in the cell's own shape.
    pub fn deferred(count: usize, repeats: usize) -> ((f64, f64), (f64, f64)) {
        let mut rec = Vec::with_capacity(repeats);
        let mut app = Vec::with_capacity(repeats);
        for _ in 0..=repeats {
            let (mut world, entities) = setup(count);
            let mut queue = CommandQueue::default();
            let t = Instant::now();
            {
                let mut commands = Commands::new(&mut queue, &world);
                for &e in &entities {
                    commands.entity(e).insert(B(1.0));
                }
            }
            let r = t.elapsed().as_secs_f64() * 1e9 / count as f64;
            let t = Instant::now();
            queue.apply(&mut world);
            let a = t.elapsed().as_secs_f64() * 1e9 / count as f64;
            std::hint::black_box(&world);
            rec.push(r);
            app.push(a);
        }
        rec.remove(0);
        app.remove(0);
        (median_spread(rec), median_spread(app))
    }
}

fn main() {
    let count = env_usize("COUNT", 10_000);
    let repeats = env_usize("REPEATS", 9);

    println!(
        "commands insert ladder: {count} inserts per pass, {repeats} passes per rung, \
         ns PER INSERT (median). Read the DELTAS."
    );

    // ── Record half ──
    let record = ladder(RECORD_RUNGS.len(), repeats, |rung| {
        let (_world, entities) = setup(count);
        let mut cmds = insert_ladder::fresh_commands();
        let t = Instant::now();
        insert_ladder::record_rung::<B, _>(&mut cmds, &entities, rung, |i| B(i as f32));
        let ns = t.elapsed().as_secs_f64() * 1e9 / count as f64;
        std::hint::black_box(insert_ladder::arena_bytes(&cmds));
        ns
    });
    print_ladder(
        "RECORD half (per insert)",
        &RECORD_RUNGS,
        &record,
        insert_ladder::RECORD_COPY_COMPLETE as usize,
    );

    // Two VARIANTS of the record half, alternated with each other so the comparison sits inside
    // one window. They answer the question the rung above cannot: is the queue push expensive
    // because of what it writes, or because of where it has to put it?
    let variants = ladder(2, repeats, |which| match which {
        // The same complete copy, into a queue that was given its room OUTSIDE the clock.
        0 => {
            let (_world, entities) = setup(count);
            let mut cmds = insert_ladder::fresh_commands();
            insert_ladder::reserve_queue(&mut cmds, entities.len());
            let t = Instant::now();
            insert_ladder::record_rung::<B, _>(
                &mut cmds,
                &entities,
                insert_ladder::RECORD_COPY_COMPLETE,
                |i| B(i as f32),
            );
            let ns = t.elapsed().as_secs_f64() * 1e9 / count as f64;
            std::hint::black_box(insert_ladder::queue_len(&cmds));
            ns
        }
        // The payload plus a 12-byte per-command record: what the queue would weigh if the three
        // function pointers every `Insert` carries were held once per TYPE, not once per COMMAND.
        _ => {
            let (_world, entities) = setup(count);
            let mut cmds = insert_ladder::fresh_commands();
            let mut scratch = Vec::new();
            let t = Instant::now();
            insert_ladder::record_narrow::<B, _>(&mut cmds, &mut scratch, &entities, |i| B(i as f32));
            let ns = t.elapsed().as_secs_f64() * 1e9 / count as f64;
            std::hint::black_box(scratch.len());
            ns
        }
    });
    println!(
        "     variants: complete copy {:.1} | into a RESERVED queue {:.1} (+-{:.1}%) | \
into a {}-byte record instead of {} {:.1} (+-{:.1}%)",
        record[insert_ladder::RECORD_COPY_COMPLETE as usize].0,
        variants[0].0,
        variants[0].1,
        std::mem::size_of::<(apex_core::entity::Entity, u32)>(),
        insert_ladder::command_bytes(),
        variants[1].0,
        variants[1].1,
    );

    // ── Apply half ── the queue is rebuilt (untimed) before every pass.
    let apply = ladder(APPLY_RUNGS.len(), repeats, |rung| {
        let (mut world, entities) = setup(count);
        let mut cmds = insert_ladder::fresh_commands();
        insert_ladder::record_rung::<B, _>(&mut cmds, &entities, insert_ladder::RECORD_REAL, |i| {
            B(i as f32)
        });
        let t = Instant::now();
        insert_ladder::apply_rung::<B>(&mut cmds, &mut world, rung);
        let ns = t.elapsed().as_secs_f64() * 1e9 / count as f64;
        std::hint::black_box(&world);
        ns
    });
    print_ladder(
        "APPLY half (per insert)",
        &APPLY_RUNGS,
        &apply,
        insert_ladder::APPLY_COPY_COMPLETE as usize,
    );

    // ── The control arm: the same inserts with no Commands in the way ──
    let (direct, direct_sp) = timed(repeats, || {
        let (mut world, entities) = setup(count);
        let t = Instant::now();
        insert_ladder::direct_insert::<B, _>(&mut world, &entities, |i| B(i as f32));
        let ns = t.elapsed().as_secs_f64() * 1e9 / count as f64;
        std::hint::black_box(&world);
        ns
    });

    let rec_real = record[insert_ladder::RECORD_REAL as usize].0;
    let app_real = apply[insert_ladder::APPLY_REAL as usize].0;
    println!(
        "\napex   : direct {direct:.1} (+-{direct_sp:.1}%) | deferred {:.1} \
         (record {rec_real:.1} + apply {app_real:.1}) => deferring costs {:+.1} ns/insert \
         ({:+.0}%)",
        rec_real + app_real,
        rec_real + app_real - direct,
        (rec_real + app_real - direct) / direct * 100.0
    );

    #[cfg(feature = "bevy")]
    {
        let (b_direct, b_direct_sp) = reference::direct(count, repeats);
        let ((b_rec, _), (b_app, _)) = reference::deferred(count, repeats);
        println!(
            "bevy   : direct {b_direct:.1} (+-{b_direct_sp:.1}%) | deferred {:.1} \
             (record {b_rec:.1} + apply {b_app:.1}) => deferring costs {:+.1} ns/insert ({:+.0}%)",
            b_rec + b_app,
            b_rec + b_app - b_direct,
            (b_rec + b_app - b_direct) / b_direct * 100.0
        );
        let ours = rec_real + app_real - direct;
        let theirs = b_rec + b_app - b_direct;
        println!(
            "\nverdict: the archetype move is ours to keep (direct {:.2}x bevy, >1 = we are \
             faster); the machinery around it costs us {ours:.1} ns against their {theirs:.1} \
             ({:.2}x, >1 = ours is dearer). Record: {:.2}x. Apply: {:.2}x.",
            b_direct / direct.max(f64::MIN_POSITIVE),
            ours / theirs.max(f64::MIN_POSITIVE),
            rec_real / b_rec.max(f64::MIN_POSITIVE),
            app_real / b_app.max(f64::MIN_POSITIVE),
        );
    }
}
