//! Micro-profile of the simple_iter regression: constructor vs iteration.
//!
//! World 1:1 with the simple_iter criterion bench (10k × 4 components), measured separately:
//!   1. CachedQuery constructor only (query without iteration);
//!   2. Query::new constructor only;
//!   3. full query().for_each (as in the bench);
//!   4. full Query::new().for_each.
//!
//! Run: `cargo run --release -p apex-bench --bin iter_profile`

use apex_bench::{Position, Rotation, Transform, Velocity};
use apex_core::prelude::*;
use apex_core::{Query, QueryState};
use cgmath::{Matrix4, Vector3};
use std::hint::black_box;
use std::time::Instant;

fn median_us<F: FnMut() -> f64>(mut f: F, samples: usize) -> f64 {
    let mut v: Vec<f64> = (0..samples).map(|_| f()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[samples / 2]
}

fn main() {
    let mut world = World::new();
    world.spawn_many(10_000, |_| {
        (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(0.0, 0.0, 0.0)),
            Rotation(Vector3::new(0.0, 0.0, 0.0)),
            Velocity(Vector3::new(1.0, 0.0, 0.0)),
        )
    });

    const N: u32 = 1000;

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                let q = world.query_mut::<(&Velocity, &mut Position)>();
                black_box(&q);
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("CachedQuery constructor:      {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                let q = Query::<(&Velocity, &mut Position)>::new_mut(&mut world);
                black_box(&q);
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("Query::new constructor:       {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                world
                    .query_mut::<(&Velocity, &mut Position)>()
                    .for_each_mut(|_, (vel, mut pos)| {
                        pos.0 += vel.0;
                    });
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("CachedQuery + for_each 10k:   {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                Query::<(&Velocity, &mut Position)>::new_mut(&mut world).for_each_mut(
                    |_, (vel, mut pos)| {
                        pos.0 += vel.0;
                    },
                );
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("Query::new + for_each 10k:    {t:.3} µs");

    // ── W2-0: QueryState (per-system state, zero locks/allocations) ──
    let mut state = QueryState::<(&Velocity, &mut Position)>::new();
    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                let q = state.query_mut(&mut world);
                black_box(&q);
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("QueryState constructor:       {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                state.query_mut(&mut world).for_each_mut(|_, (vel, mut pos)| {
                    pos.0 += vel.0;
                });
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("QueryState + for_each 10k:    {t:.3} µs");

    // ── W2-0.5: dense chunk iteration (slices + stamp_range) ──
    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                state.query_mut(&mut world).for_each_chunk_mut(|_, (vel, pos)| {
                    for i in 0..pos.len() {
                        pos[i].0 += vel[i].0;
                    }
                });
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("QueryState + for_each_chunk:  {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                world
                    .query_mut::<(&Velocity, &mut Position)>()
                    .for_each_chunk_mut(|_, (vel, pos)| {
                        for i in 0..pos.len() {
                            pos[i].0 += vel[i].0;
                        }
                    });
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("CachedQuery + for_each_chunk: {t:.3} µs");
}
