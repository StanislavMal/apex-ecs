//! Probe: WHICH HALF of the `relations` criterion cell is slow?
//!
//! The cell (`benches/benchmarks.rs`, group `relations`) reports one number for THREE works
//! fused together — spawn 10k children, link each to one parent, walk the sibling list — and
//! apex sits at 0.73x of bevy there. One number over three works cannot say where to cut, and
//! the two engines do not even split the work the same way:
//!
//! * apex spawns, then calls `add_relation` (a relation is NOT a component after CR-M1, so it
//!   costs two index insertions and no archetype move);
//! * bevy spawns `(A, ChildOf(parent))` in ONE bundle, so the link rides along inside the spawn
//!   and its marginal price is `spawn(A, ChildOf) - spawn(A)`.
//!
//! So the probe measures the halves that ARE comparable, in two framings, because the FIRST
//! run of it showed the two framings disagree:
//!
//! * BULK — spawn all, then link all: each phase streams its own memory to completion;
//! * INTERLEAVED — spawn a child and link it, 10k times: the shape the cell (and a real glTF
//!   import, and the editor) actually runs, where both working sets are live at once.
//!
//! The interleaved link price is `interleaved(spawn+link) - interleaved(spawn)` on BOTH engines,
//! which is the only apples-to-apples form: bevy has no standalone link at all.
//!
//! Run: `cargo run --release -p apex-bench --bin relations_shapes --features bevy`
//! Tuning: `RUNGS=1000,10000,100000 SAMPLES=15`
//!
//! Deliberately NOT a criterion cell: the committed core baseline is a fixed group list, and a
//! diagnostic that answers "where to cut" is read once per campaign, not gated every run.

use apex_core::prelude::*;
use apex_macros::Component;
use std::time::{Duration, Instant};

// The payloads are never read back: these types exist to give the spawn a REAL component
// layout to write, which is the whole subject of the probe.
#[allow(dead_code)]
#[derive(Component, Clone, Copy)]
struct A(f32);
// The four-component bundle mirrors the criterion cell `spawn_wide` EXACTLY -- 64-byte matrix
// plus three 12-byte vectors, 100 bytes a row. Four f32 markers would have measured per-call
// overhead only, and a spawn of a real bundle is mostly memory.
#[allow(dead_code)]
#[derive(Component, Clone, Copy)]
struct B2(cgmath::Matrix4<f32>);
#[allow(dead_code)]
#[derive(Component, Clone, Copy)]
struct C2(cgmath::Vector3<f32>);
#[allow(dead_code)]
#[derive(Component, Clone, Copy)]
struct D2(cgmath::Vector3<f32>);

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn m4(seed: f32) -> cgmath::Matrix4<f32> {
    cgmath::Matrix4::from_scale(1.0 + seed * 1e-6)
}

fn v3(seed: f32) -> cgmath::Vector3<f32> {
    cgmath::Vector3::new(seed, 0.0, 0.0)
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// Median of `samples` timed phases, in microseconds. `setup` builds the state OUTSIDE the
/// clock, `phase` is the only thing measured; both the result and the state are fed to
/// `black_box` so nothing under test can be optimised away.
fn median<S, R>(
    samples: usize,
    mut setup: impl FnMut() -> S,
    mut phase: impl FnMut(&mut S) -> R,
) -> f64 {
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let mut state = setup();
        let t = Instant::now();
        let out = phase(&mut state);
        times.push(t.elapsed());
        std::hint::black_box(out);
        std::hint::black_box(&state);
    }
    times.sort();
    us(times[samples / 2])
}

/// One engine's numbers at one rung. All five are medians of whole-phase timings.
struct Shape {
    /// BULK: 10k spawns into a fresh world (world creation outside the clock).
    bulk_spawn: f64,
    /// BULK: link 10k already-spawned children to one parent.
    bulk_link: f64,
    /// INTERLEAVED: world creation + 10k spawns, nothing linked.
    inter_spawn: f64,
    /// INTERLEAVED: world creation + 10k (spawn + link) — the cell's own shape.
    inter_linked: f64,
    /// Walk the finished sibling list.
    walk: f64,
    /// INTERLEAVED with the relation kind resolved ONCE (apex `add_relation_by_kind_idx`;
    /// bevy has no counterpart and repeats the number above).
    inter_linked_hoisted: f64,
    /// Individual (not batched) spawn of a ONE-component bundle, world creation inside.
    spawn1: f64,
    /// The same with a FOUR-component bundle. `spawn4 - spawn1` is what three extra
    /// components cost per spawn — the price of resolving the bundle, if it is not cached.
    spawn4: f64,
    /// The same four-component spawns into an archetype that ALREADY holds a large
    /// population, so its columns never grow during the measurement. `spawn4 - spawn4_warm`
    /// is what the growth costs: apex grows every column and every tick vector on its own,
    /// bevy allocates the row.
    spawn4_warm: f64,
}

impl Shape {
    /// What the link costs when it is interleaved with the spawns that feed it.
    fn inter_link(&self) -> f64 {
        self.inter_linked - self.inter_spawn
    }
}

fn apex_shape(n: usize, samples: usize) -> Shape {
    let spawn_all = |world: &mut World| {
        let mut last = None;
        for i in 0..n {
            last = Some(world.spawn((A(i as f32),)));
        }
        last
    };

    let bulk_spawn = median(samples, World::new, spawn_all);

    let bulk_link = median(
        samples,
        || {
            let mut world = World::new();
            let parent = world.spawn((A(0.0),));
            let kids: Vec<Entity> = (0..n).map(|i| world.spawn((A(i as f32),))).collect();
            (world, parent, kids)
        },
        |(world, parent, kids)| {
            for &c in kids.iter() {
                world.add_relation(c, ChildOf, *parent);
            }
        },
    );

    let inter_spawn = median(
        samples,
        || (),
        |_| {
            let mut world = World::new();
            let parent = world.spawn((A(0.0),));
            for i in 0..n {
                world.spawn((A(i as f32),));
            }
            parent
        },
    );

    let inter_linked = median(
        samples,
        || (),
        |_| {
            let mut world = World::new();
            let parent = world.spawn((A(0.0),));
            for i in 0..n {
                let c = world.spawn((A(i as f32),));
                world.add_relation(c, ChildOf, parent);
            }
            world.targets_of(ChildOf, parent).count()
        },
    );

    let walk = median(
        samples,
        || {
            let mut world = World::new();
            let parent = world.spawn((A(0.0),));
            for i in 0..n {
                let c = world.spawn((A(i as f32),));
                world.add_relation(c, ChildOf, parent);
            }
            (world, parent)
        },
        |(world, parent)| world.targets_of(ChildOf, *parent).count(),
    );

    let inter_linked_hoisted = median(
        samples,
        || (),
        |_| {
            let mut world = World::new();
            let parent = world.spawn((A(0.0),));
            let kind = world.relation_registry_mut().get_or_register::<ChildOf>();
            for i in 0..n {
                let c = world.spawn((A(i as f32),));
                world.add_relation_by_kind_idx(c, kind, parent);
            }
            world.targets_of(ChildOf, parent).count()
        },
    );

    let spawn1 = median(
        samples,
        || (),
        |_| {
            let mut world = World::new();
            for i in 0..n {
                world.spawn((A(i as f32),));
            }
            world.entity_count()
        },
    );

    let spawn4 = median(
        samples,
        || (),
        |_| {
            let mut world = World::new();
            for i in 0..n {
                let f = i as f32;
                world.spawn((A(f), B2(m4(f)), C2(v3(f)), D2(v3(f))));
            }
            world.entity_count()
        },
    );

    let spawn4_warm = median(
        samples,
        || {
            let mut world = World::new();
            for i in 0..n {
                let f = i as f32;
                world.spawn((A(f), B2(m4(f)), C2(v3(f)), D2(v3(f))));
            }
            world
        },
        |world| {
            for i in 0..n {
                let f = i as f32;
                world.spawn((A(f), B2(m4(f)), C2(v3(f)), D2(v3(f))));
            }
            world.entity_count()
        },
    );

    Shape {
        bulk_spawn,
        bulk_link,
        inter_spawn,
        inter_linked,
        walk,
        inter_linked_hoisted,
        spawn1,
        spawn4,
        spawn4_warm,
    }
}

#[cfg(feature = "bevy")]
fn bevy_shape(n: usize, samples: usize) -> Shape {
    use bevy_ecs::hierarchy::{ChildOf as BChildOf, Children};
    use bevy_ecs::prelude::{Entity as BEntity, World as BWorld};

    #[allow(dead_code)]
    #[derive(bevy_ecs::component::Component)]
    struct B(f32);
    #[allow(dead_code)]
    #[derive(bevy_ecs::component::Component)]
    struct B3(cgmath::Matrix4<f32>);
    #[allow(dead_code)]
    #[derive(bevy_ecs::component::Component)]
    struct B4(cgmath::Vector3<f32>);
    #[allow(dead_code)]
    #[derive(bevy_ecs::component::Component)]
    struct B5(cgmath::Vector3<f32>);

    let bulk_spawn = median(samples, BWorld::new, |world: &mut BWorld| {
        let mut last = None;
        for i in 0..n {
            last = Some(world.spawn(B(i as f32)).id());
        }
        last
    });

    // Bevy's standalone link is `insert(ChildOf)` on an already-spawned child — an archetype
    // move, which apex's index insertion is not. Reported for completeness, never as the
    // headline: the headline comparison is the INTERLEAVED marginal cost below.
    let bulk_link = median(
        samples,
        || {
            let mut world = BWorld::new();
            let parent = world.spawn(B(0.0)).id();
            let kids: Vec<BEntity> = (0..n).map(|i| world.spawn(B(i as f32)).id()).collect();
            (world, parent, kids)
        },
        |(world, parent, kids): &mut (BWorld, BEntity, Vec<BEntity>)| {
            for &c in kids.iter() {
                world.entity_mut(c).insert(BChildOf(*parent));
            }
        },
    );

    let inter_spawn = median(
        samples,
        || (),
        |_| {
            let mut world = BWorld::new();
            let parent = world.spawn(B(0.0)).id();
            for i in 0..n {
                world.spawn(B(i as f32));
            }
            parent
        },
    );

    let inter_linked = median(
        samples,
        || (),
        |_| {
            let mut world = BWorld::new();
            let parent = world.spawn(B(0.0)).id();
            for i in 0..n {
                world.spawn((B(i as f32), BChildOf(parent)));
            }
            world
                .entity(parent)
                .get::<Children>()
                .map(|c| c.iter().count())
                .unwrap_or(0)
        },
    );

    let walk = median(
        samples,
        || {
            let mut world = BWorld::new();
            let parent = world.spawn(B(0.0)).id();
            for i in 0..n {
                world.spawn((B(i as f32), BChildOf(parent)));
            }
            (world, parent)
        },
        |(world, parent): &mut (BWorld, BEntity)| {
            world
                .entity(*parent)
                .get::<Children>()
                .map(|c| c.iter().count())
                .unwrap_or(0)
        },
    );

    let spawn1 = median(
        samples,
        || (),
        |_| {
            let mut world = BWorld::new();
            for i in 0..n {
                world.spawn(B(i as f32));
            }
            world.entities().len()
        },
    );

    let spawn4 = median(
        samples,
        || (),
        |_| {
            let mut world = BWorld::new();
            for i in 0..n {
                let f = i as f32;
                world.spawn((B(f), B3(m4(f)), B4(v3(f)), B5(v3(f))));
            }
            world.entities().len()
        },
    );

    let spawn4_warm = median(
        samples,
        || {
            let mut world = BWorld::new();
            for i in 0..n {
                let f = i as f32;
                world.spawn((B(f), B3(m4(f)), B4(v3(f)), B5(v3(f))));
            }
            world
        },
        |world: &mut BWorld| {
            for i in 0..n {
                let f = i as f32;
                world.spawn((B(f), B3(m4(f)), B4(v3(f)), B5(v3(f))));
            }
            world.entities().len()
        },
    );

    Shape {
        bulk_spawn,
        bulk_link,
        inter_spawn,
        inter_linked,
        walk,
        inter_linked_hoisted: inter_linked,
        spawn1,
        spawn4,
        spawn4_warm,
    }
}

fn row(label: &str, n: usize, apex: f64, bevy: Option<f64>) {
    let (bevy_cell, ratio) = match bevy {
        Some(b) => (
            format!("{b:.1}"),
            if apex > 0.0 { format!("{:.2}", b / apex) } else { "-".into() },
        ),
        None => ("-".into(), "-".into()),
    };
    println!(
        "  {label:<28} {apex:>10.1} {bevy_cell:>10} {ratio:>8}   {:>8.1}",
        apex * 1e3 / n.max(1) as f64
    );
}

fn main() {
    let samples = env_usize("SAMPLES", 15);
    let rungs: Vec<usize> = std::env::var("RUNGS")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1_000, 10_000, 100_000]);

    println!("relations shapes: {samples} samples per phase, medians in microseconds");
    for &n in &rungs {
        let a = apex_shape(n, samples);
        #[cfg(feature = "bevy")]
        let b = Some(bevy_shape(n, samples));
        #[cfg(not(feature = "bevy"))]
        let b: Option<Shape> = None;
        let pick = |f: fn(&Shape) -> f64| b.as_ref().map(f);

        println!("\n=== {n} children ===");
        println!(
            "  {:<28} {:>10} {:>10} {:>8}   {:>8}",
            "phase", "apex us", "bevy us", "x bevy", "ns/child"
        );
        row("BULK spawn", n, a.bulk_spawn, pick(|s| s.bulk_spawn));
        row("BULK link", n, a.bulk_link, pick(|s| s.bulk_link));
        row("INTER spawn only", n, a.inter_spawn, pick(|s| s.inter_spawn));
        row("INTER spawn+link (the cell)", n, a.inter_linked, pick(|s| s.inter_linked));
        row("INTER link, marginal", n, a.inter_link(), pick(|s| s.inter_link()));
        row("walk siblings", n, a.walk, pick(|s| s.walk));
        row("INTER link, kind hoisted", n, a.inter_linked_hoisted, pick(|s| s.inter_linked_hoisted));
        row("spawn 1 component", n, a.spawn1, pick(|s| s.spawn1));
        row("spawn 4 components", n, a.spawn4, pick(|s| s.spawn4));
        row("spawn 4, warm archetype", n, a.spawn4_warm, pick(|s| s.spawn4_warm));
        println!(
            "  note: column growth costs apex {:.1} us, bevy {:.1} us (spawn4 minus the warm run)",
            a.spawn4 - a.spawn4_warm,
            b.as_ref().map(|s| s.spawn4 - s.spawn4_warm).unwrap_or(f64::NAN)
        );
        println!(
            "  note: the 3 extra components cost apex {:.1} us, bevy {:.1} us — a per-spawn bundle 
                     resolution that is NOT cached shows up here and nowhere else",
            a.spawn4 - a.spawn1,
            b.as_ref().map(|s| s.spawn4 - s.spawn1).unwrap_or(f64::NAN)
        );
        println!(
            "  note: apex link costs {:.1} us in bulk vs {:.1} us interleaved ({:.2}x) — a gap here \
             is cache/allocation shape, not algorithm",
            a.bulk_link,
            a.inter_link(),
            a.inter_link() / a.bulk_link.max(f64::MIN_POSITIVE)
        );
    }
    println!(
        "\nBevy's BULK link is an archetype move (insert ChildOf on a live entity); apex's is two \
         index insertions.\nThe comparable number is INTER link, marginal — the same task on both."
    );
}
