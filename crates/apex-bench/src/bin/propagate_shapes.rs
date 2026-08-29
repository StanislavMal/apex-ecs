//! Probe: what does `propagate_transforms` cost on the SHAPES a real scene actually has —
//! a flat moving mass, a deep hierarchy, and the two mixed in one frame?
//!
//! Why this probe exists: the criterion cell `propagate` measures 200 roots × a chain of 50 (deep
//! and narrow, the many-foxes shape) and `propagate_static` measures nothing moving. The shape the
//! ENGINE pays for on its moving-mass bench (`many_transforms`, 100k entities) is neither — it is
//! 100 001 childless roots, all dirty, every frame. Measured there 2026-08-29 with
//! `APEX_MAIN_PROF=1`: `propagate_transforms` **7.77 ms/call**, the largest single main-thread
//! system — bigger than the extract commit (3.78) and the retained-scene update (3.27). No bench
//! cell of the core covered that shape at all, so the cost had no home to regress against.
//!
//! The chain shape is measured in the SAME run for exactly one reason: a change that pays for the
//! flat mass out of the hierarchy's pocket must be visible as such, in one place, before anyone
//! quotes the flat number.
//!
//! Run: `cargo run --release -p apex-bench --bin propagate_shapes`
//! Tuning: `FLAT=100000 CHAIN_ROOTS=200 CHAIN_LEN=50 SAMPLES=15 ...`
//! Phases: `APEX_PROP_TRACE=1` (the system's own changed-query / seed / descend split);
//! attribution inside the descent: `APEX_PROP_LADDER=1`.

use apex_core::prelude::*;
use apex_core::transform::{
    propagate_transforms, GlobalTransform, LocalTransform, TransformPlugin,
};
use glam::{DVec3, Quat, Vec3};
use std::time::{Duration, Instant};

/// Minimal stderr logger — `propagate_transforms` prints its phase split (`APEX_PROP_TRACE=1`)
/// through `log`, and a probe binary with no logger installed would silently swallow the very
/// breakdown it exists to read.
struct StderrLog;
impl log::Log for StderrLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("{}", record.args());
    }
    fn flush(&self) {}
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn lt(seed: f32) -> LocalTransform {
    LocalTransform {
        translation: DVec3::new(seed as f64, 0.0, 0.0),
        rotation: Quat::from_rotation_y(0.01 * seed),
        scale: Vec3::ONE,
    }
}

/// Median of `samples` propagations, each preceded by an UNMEASURED "animation" of `movers`.
fn median_propagate(world: &mut World, movers: &[Entity], samples: usize) -> (f64, f64, f64) {
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        // Not measured: the animation is the probe's own load, not the system under test.
        world.tick();
        for &e in movers {
            if let Some(mut l) = world.get_mut::<LocalTransform>(e) {
                l.translation.x += 0.001;
            }
        }
        let t = Instant::now();
        propagate_transforms(world);
        times.push(t.elapsed());
    }
    times.sort();
    (
        ms(times[0]),
        ms(times[samples / 2]),
        ms(times[samples - 1]),
    )
}

fn report(label: &str, n: usize, (lo, med, hi): (f64, f64, f64)) {
    println!(
        "{label:<22}: min {lo:.3} ms | med {med:.3} ms | max {hi:.3} ms | {:.1} ns/node (med)",
        med * 1e6 / n.max(1) as f64
    );
}

fn main() {
    static LOGGER: StderrLog = StderrLog;
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));

    let flat = env_usize("FLAT", 100_000);
    let chain_roots = env_usize("CHAIN_ROOTS", 200);
    let chain_len = env_usize("CHAIN_LEN", 50);
    let samples = env_usize("SAMPLES", 15);

    // ── Shape 1: flat moving mass (the engine's `many_transforms`) ──
    {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);
        let mut nodes = Vec::with_capacity(flat);
        for i in 0..flat {
            nodes.push(world.spawn((lt(i as f32 * 0.001), GlobalTransform::IDENTITY)));
        }
        world.tick();
        propagate_transforms(&mut world); // settle; not part of any number below
        println!("=== flat: {flat} childless roots, no hierarchy ===");
        report(
            "ALL dirty",
            flat,
            median_propagate(&mut world, &nodes, samples),
        );
        report(
            "ONE dirty",
            1,
            median_propagate(&mut world, &nodes[..1], samples),
        );
        let none: [Entity; 0] = [];
        report("NOTHING dirty", 1, median_propagate(&mut world, &none, samples));
    }

    // ── Shape 2: deep hierarchy (the many-foxes shape the criterion cell already guards) ──
    {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);
        let total = chain_roots * chain_len;
        let mut nodes = Vec::with_capacity(total);
        let mut roots = Vec::with_capacity(chain_roots);
        for r in 0..chain_roots {
            let root = world.spawn((lt(r as f32), GlobalTransform::IDENTITY));
            nodes.push(root);
            roots.push(root);
            let mut parent = root;
            for d in 1..chain_len {
                let child = world.spawn((lt(d as f32), GlobalTransform::IDENTITY));
                world.add_relation(child, ChildOf, parent);
                nodes.push(child);
                parent = child;
            }
        }
        world.tick();
        propagate_transforms(&mut world);
        println!("\n=== chain: {chain_roots} roots x {chain_len} deep = {total} nodes ===");
        report(
            "ALL dirty",
            total,
            median_propagate(&mut world, &nodes, samples),
        );
        report(
            "ROOTS dirty (cascade)",
            total,
            median_propagate(&mut world, &roots, samples),
        );
    }

    // ── Shape 3: both in one world — a moving mass beside a hierarchy (a real level) ──
    {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);
        let mut movers = Vec::with_capacity(flat);
        for i in 0..flat {
            movers.push(world.spawn((lt(i as f32 * 0.001), GlobalTransform::IDENTITY)));
        }
        let mut chain_nodes = Vec::new();
        for r in 0..chain_roots {
            let root = world.spawn((lt(r as f32), GlobalTransform::IDENTITY));
            chain_nodes.push(root);
            let mut parent = root;
            for d in 1..chain_len {
                let child = world.spawn((lt(d as f32), GlobalTransform::IDENTITY));
                world.add_relation(child, ChildOf, parent);
                chain_nodes.push(child);
                parent = child;
            }
        }
        world.tick();
        propagate_transforms(&mut world);
        let mut all = movers.clone();
        all.extend_from_slice(&chain_nodes);
        println!(
            "\n=== mixed: {flat} flat movers + {} hierarchy nodes ===",
            chain_nodes.len()
        );
        report(
            "ALL dirty",
            all.len(),
            median_propagate(&mut world, &all, samples),
        );
    }
}
