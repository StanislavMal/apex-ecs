//! Correctness of the parallel query iteration paths (`par_for_each`,
//! `par_for_each_mut`, and the dense `_chunk` variants).
//!
//! These paths are unsafe-heavy (raw `fetch_slices`, disjoint per-leaf row
//! ranges proven by `&mut self`) and split work across archetypes with rayon,
//! so the properties that matter are: every matching entity is visited EXACTLY
//! once, mutations land on disjoint ranges (no double-apply, no skip), and the
//! aggregate equals the sequential iteration. Order is nondeterministic, so all
//! assertions are over sets/aggregates, never sequence.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use apex_core::prelude::*;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Value(u64);

#[derive(Component, Clone, Copy)]
struct Counter(u64);

// Marker components used only to spread entities across several archetypes, so
// the parallel split actually crosses archetype boundaries.
#[derive(Component, Clone, Copy)]
struct TagA;
#[derive(Component, Clone, Copy)]
struct TagB;

/// Spawn `Value(i)` for i in 0..n, distributed over three archetypes.
fn spawn_across_archetypes(world: &mut World, n: u64) -> HashSet<Entity> {
    let mut all = HashSet::new();
    for i in 0..n {
        let e = match i % 3 {
            0 => world.spawn((Value(i),)),
            1 => world.spawn((Value(i), TagA)),
            _ => world.spawn((Value(i), TagB)),
        };
        all.insert(e);
    }
    all
}

#[test]
fn par_for_each_visits_every_entity_exactly_once() {
    let mut world = World::new();
    const N: u64 = 6000;
    let expected = spawn_across_archetypes(&mut world, N);

    let seen = Mutex::new(HashSet::new());
    let sum = AtomicU64::new(0);
    world.query::<&Value>().par_for_each(|e, v| {
        seen.lock().unwrap().insert(e);
        // If any entity were visited twice, the sum would exceed the closed form.
        sum.fetch_add(v.0, Ordering::Relaxed);
    });

    assert_eq!(
        seen.into_inner().unwrap(),
        expected,
        "par_for_each must visit every matching entity across all archetypes"
    );
    assert_eq!(
        sum.load(Ordering::Relaxed),
        (0..N).sum::<u64>(),
        "each entity visited exactly once (aggregate matches the closed form)"
    );
}

#[test]
fn par_for_each_matches_sequential_iteration() {
    let mut world = World::new();
    spawn_across_archetypes(&mut world, 4000);

    // Sequential reference.
    let mut seq = Vec::new();
    world.query::<&Value>().for_each(|_e, v| seq.push(v.0));
    seq.sort_unstable();

    // Parallel, then sort (order is nondeterministic).
    let par = Mutex::new(Vec::new());
    world
        .query::<&Value>()
        .par_for_each(|_e, v| par.lock().unwrap().push(v.0));
    let mut par = par.into_inner().unwrap();
    par.sort_unstable();

    assert_eq!(par, seq, "parallel iteration must yield the same multiset as sequential");
}

#[test]
fn par_for_each_mut_increments_each_element_once() {
    let mut world = World::new();
    const N: u64 = 6000;
    for i in 0..N {
        match i % 3 {
            0 => {
                world.spawn((Counter(0),));
            }
            1 => {
                world.spawn((Counter(0), TagA));
            }
            _ => {
                world.spawn((Counter(0), TagB));
            }
        };
    }

    world
        .query_mut::<&mut Counter>()
        .par_for_each_mut(|_e, mut c| c.0 += 1);

    // Every counter must be exactly 1: no row processed twice (overlapping
    // ranges) and none skipped (gap in the split).
    let mut wrong = 0u64;
    let mut total = 0u64;
    world.query::<&Counter>().for_each(|_e, c| {
        total += 1;
        if c.0 != 1 {
            wrong += 1;
        }
    });
    assert_eq!(total, N, "all entities present");
    assert_eq!(wrong, 0, "each element incremented exactly once (disjoint row ranges)");
}

#[test]
fn par_for_each_chunk_covers_all_rows() {
    let mut world = World::new();
    const N: u64 = 5000;
    spawn_across_archetypes(&mut world, N);

    let sum = AtomicU64::new(0);
    let count = AtomicU64::new(0);
    world.query::<&Value>().par_for_each_chunk(|entities, values: &[Value]| {
        assert_eq!(entities.len(), values.len(), "entity and value slices align");
        for v in values {
            sum.fetch_add(v.0, Ordering::Relaxed);
        }
        count.fetch_add(values.len() as u64, Ordering::Relaxed);
    });

    assert_eq!(count.load(Ordering::Relaxed), N, "every row appears in exactly one chunk");
    assert_eq!(sum.load(Ordering::Relaxed), (0..N).sum::<u64>(), "chunked sum matches the closed form");
}

#[test]
fn par_for_each_chunk_mut_mutates_all_rows() {
    let mut world = World::new();
    const N: u64 = 5000;
    for i in 0..N {
        match i % 3 {
            0 => {
                world.spawn((Counter(0),));
            }
            1 => {
                world.spawn((Counter(0), TagA));
            }
            _ => {
                world.spawn((Counter(0), TagB));
            }
        };
    }

    world
        .query_mut::<&mut Counter>()
        .par_for_each_chunk_mut(|_entities, counters: &mut [Counter]| {
            for c in counters {
                c.0 += 1;
            }
        });

    let mut wrong = 0u64;
    world.query::<&Counter>().for_each(|_e, c| {
        if c.0 != 1 {
            wrong += 1;
        }
    });
    assert_eq!(wrong, 0, "every row mutated exactly once through disjoint mutable slices");
}
