//! Микро-профиль регресса simple_iter: конструктор vs итерация.
//!
//! Мир 1:1 с criterion-бенчем simple_iter (10k × 4 компонента), меряем раздельно:
//!   1. только конструктор CachedQuery (query без итерации);
//!   2. только конструктор Query::new;
//!   3. полный query().for_each (как в бенче);
//!   4. полный Query::new().for_each.
//!
//! Запуск: `cargo run --release -p apex-bench --bin iter_profile`

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
                let q = world.query_mut::<(Read<Velocity>, Write<Position>)>();
                black_box(&q);
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("CachedQuery конструктор:      {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                let q = Query::<(Read<Velocity>, Write<Position>)>::new_mut(&mut world);
                black_box(&q);
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("Query::new конструктор:       {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                world
                    .query_mut::<(Read<Velocity>, Write<Position>)>()
                    .for_each(|_, (vel, mut pos)| {
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
                Query::<(Read<Velocity>, Write<Position>)>::new_mut(&mut world).for_each(
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

    // ── W2-0: QueryState (per-system стейт, ноль локов/аллокаций) ──
    let mut state = QueryState::<(Read<Velocity>, Write<Position>)>::new();
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
    println!("QueryState конструктор:       {t:.3} µs");

    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                state.query_mut(&mut world).for_each(|_, (vel, mut pos)| {
                    pos.0 += vel.0;
                });
            }
            t.elapsed().as_secs_f64() * 1e6 / N as f64
        },
        9,
    );
    println!("QueryState + for_each 10k:    {t:.3} µs");

    // ── W2-0.5: плотная chunk-итерация (слайсы + stamp_range) ──
    let t = median_us(
        || {
            let t = Instant::now();
            for _ in 0..N {
                state.query_mut(&mut world).for_each_chunk(|_, (vel, pos)| {
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
                    .query_mut::<(Read<Velocity>, Write<Position>)>()
                    .for_each_chunk(|_, (vel, pos)| {
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
