//! Sh2 cost-model guard: heavy work at LOW entity count must parallelize.
//!
//! The entity-count heuristic serializes a 2-system stage below ~25k entities.
//! With heavy per-entity work at N=3000, parallelism still wins big — the
//! cost-model MEASURES the cost and parallelizes where the entity heuristic
//! would not. This guards against regressing back to entity-only gating.

use apex_core::prelude::*;
use apex_core::{system, World};
use apex_macros::Component;
use apex_scheduler::{Scheduler, StageLabel};
use std::time::Instant;

#[derive(Component, Clone, Copy)]
struct A(f32);
#[derive(Component, Clone, Copy)]
struct B(f32);

// Heavy per-entity work: a CPU-bound transcendental busy-loop (memory-light so it
// parallelizes cleanly). The two systems touch DISJOINT components (A vs B) and thus
// disjoint archetypes — no conflict, both parallel-eligible, isolated (no false sharing).
#[inline]
fn heavy(x: f32) -> f32 {
    let mut v = x;
    for _ in 0..400 {
        v = (v.sin() * 1.000_001 + 1.0).sqrt().max(0.001);
    }
    v
}

system! {
    fn heavy_a(q: (&mut A,)) {
        q.for_each_mut(|_, (mut a,)| { a.0 = heavy(a.0); });
    }
}
system! {
    fn heavy_b(q: (&mut B,)) {
        q.for_each_mut(|_, (mut b,)| { b.0 = heavy(b.0); });
    }
}

fn build() -> (World, Scheduler) {
    let mut world = World::new();
    const N: usize = 3000; // well below the 25k entity heuristic for 2 systems
    for i in 0..N {
        world.spawn((A(i as f32 * 0.5 + 1.0),));
        world.spawn((B(i as f32 * 0.5 + 1.0),));
    }
    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, (heavy_a, heavy_b));
    sched.compile().unwrap();
    (world, sched)
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

#[test]
fn heavy_low_entity_parallelizes_via_cost_model() {
    let threads = rayon::current_num_threads();

    // Sequential reference.
    let (mut ws, mut ss) = build();
    for _ in 0..10 {
        ws.tick();
        ss.run_sequential(&mut ws);
    }
    let seq: Vec<u128> = (0..15)
        .map(|_| {
            let t = Instant::now();
            ws.tick();
            ss.run_sequential(&mut ws);
            t.elapsed().as_nanos()
        })
        .collect();
    let seq_med = median(seq);

    // Parallel via run(): warm up so the cost-model learns the stage is expensive
    // and flips to PAR (the entity heuristic alone would keep it serial at N=3000).
    let (mut wp, mut sp) = build();
    for _ in 0..30 {
        wp.tick();
        sp.run(&mut wp);
    }
    let par: Vec<u128> = (0..15)
        .map(|_| {
            let t = Instant::now();
            wp.tick();
            sp.run(&mut wp);
            t.elapsed().as_nanos()
        })
        .collect();
    let par_med = median(par);

    println!(
        "heavy_low_entity: threads={threads} seq={:.1}us par={:.1}us speedup={:.2}x",
        seq_med as f64 / 1000.0,
        par_med as f64 / 1000.0,
        seq_med as f64 / par_med as f64,
    );

    if threads >= 4 {
        // The entity heuristic would serialize this (3000 < 25k) → ~1.0x. The
        // cost-model must parallelize it. Lenient bound absorbs measurement noise.
        assert!(
            par_med as f64 <= seq_med as f64 * 0.66,
            "cost-model failed to parallelize heavy-low-entity work: \
             seq={seq_med}ns par={par_med}ns (expected par <= 0.66*seq on {threads} threads)"
        );
    }
}
