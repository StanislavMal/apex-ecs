/// Apex ECS — Performance Benchmark
///
/// Запуск: cargo run -p apex-examples --example perf --release
///
/// Замеряет:
///   1. spawn_bundle        — создание N entity с 3 компонентами
///   2. query iter          — итерация по всем Position
///   3. query iter + write  — итерация + мутация Velocity
///   4. insert component    — добавление нового компонента к живым entity
///   5. remove component    — удаление компонента
///   6. despawn             — уничтожение всех entity
///   7. spawn (builder)     — старый путь через .insert() цепочку

use std::time::{Duration, Instant};
use apex_core::prelude::*;

// ── Компоненты ─────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Position { x: f32, y: f32, z: f32 }

#[derive(Clone, Copy)]
struct Velocity { x: f32, y: f32, z: f32 }

#[derive(Clone, Copy)]
struct Health { current: f32, max: f32 }

#[derive(Clone, Copy)]
struct Mass(f32);

// ── Утилиты замера ─────────────────────────────────────────────

fn bench<F: FnMut() -> u64>(label: &str, mut f: F) {
    // Прогрев
    f();

    const RUNS: u32 = 5;
    let mut times = Vec::with_capacity(RUNS as usize);

    for _ in 0..RUNS {
        let t = Instant::now();
        let count = f();
        let elapsed = t.elapsed();
        times.push((elapsed, count));
    }

    // Медиана
    times.sort_by_key(|(d, _)| *d);
    let (median, count) = times[RUNS as usize / 2];
    print_result(label, median, count);
}

fn print_result(label: &str, duration: Duration, count: u64) {
    let ns = duration.as_nanos() as f64;
    let ns_per_op = if count > 0 { ns / count as f64 } else { ns };
    let mops = if duration.as_secs_f64() > 0.0 {
        count as f64 / duration.as_secs_f64() / 1_000_000.0
    } else {
        f64::INFINITY
    };
    println!(
        "  {:<35} {:>8.2} ns/op   {:>8.2} M ops/s   (n={}, {:?})",
        label, ns_per_op, mops, count, duration
    );
}

// ── Бенчмарки ──────────────────────────────────────────────────

fn bench_spawn_bundle(n: usize) {
    bench(&format!("spawn_bundle ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        for i in 0..n * 1000 {
            let fi = i as f32;
            world.spawn_bundle((
                Position { x: fi, y: fi * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Health { current: 100.0, max: 100.0 },
            ));
        }
        (n * 1000) as u64
    });
}

fn bench_spawn_builder(n: usize) {
    bench(&format!("spawn builder chain ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        for i in 0..n * 1000 {
            let fi = i as f32;
            world.spawn()
                .insert(Position { x: fi, y: fi * 0.5, z: 0.0 })
                .insert(Velocity { x: 1.0, y: 0.0, z: 0.0 })
                .insert(Health { current: 100.0, max: 100.0 })
                .id();
        }
        (n * 1000) as u64
    });
}

fn bench_query_iter(n: usize) {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    for i in 0..n * 1000 {
        let fi = i as f32;
        world.spawn_bundle((
            Position { x: fi, y: fi * 0.5, z: 0.0 },
            Velocity { x: 1.0, y: 0.0, z: 0.0 },
            Health { current: 100.0, max: 100.0 },
        ));
    }

    bench(&format!("query iter read ({n}k)"), || {
        let mut sum = 0.0f32;
        for (_, pos) in world.query().read::<Position>().iter_one::<Position>() {
            sum += pos.x + pos.y;
        }
        // Предотвращаем оптимизацию
        std::hint::black_box(sum);
        (n * 1000) as u64
    });
}

fn bench_query_iter_write(n: usize) {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    let entities: Vec<Entity> = (0..n * 1000)
        .map(|i| {
            let fi = i as f32;
            world.spawn_bundle((
                Position { x: fi, y: fi * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Health { current: 100.0, max: 100.0 },
            ))
        })
        .collect();

    bench(&format!("get_mut per entity ({n}k)"), || {
        for &e in &entities {
            if let Some(pos) = world.get_mut::<Position>(e) {
                pos.x += 1.0;
                pos.y += 0.5;
            }
        }
        (n * 1000) as u64
    });
}

fn bench_insert_component(n: usize) {
    bench(&format!("insert new component ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Mass>();

        let entities: Vec<Entity> = (0..n * 1000)
            .map(|i| {
                world.spawn_bundle((
                    Position { x: i as f32, y: 0.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                ))
            })
            .collect();

        for &e in &entities {
            world.insert(e, Mass(1.0));
        }
        (n * 1000) as u64
    });
}

fn bench_remove_component(n: usize) {
    bench(&format!("remove component ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Mass>();

        let entities: Vec<Entity> = (0..n * 1000)
            .map(|i| {
                world.spawn_bundle((
                    Position { x: i as f32, y: 0.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                    Mass(1.0),
                ))
            })
            .collect();

        for &e in &entities {
            world.remove::<Mass>(e);
        }
        (n * 1000) as u64
    });
}

fn bench_despawn(n: usize) {
    bench(&format!("despawn ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        let entities: Vec<Entity> = (0..n * 1000)
            .map(|i| {
                world.spawn_bundle((
                    Position { x: i as f32, y: 0.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                ))
            })
            .collect();

        for e in entities {
            world.despawn(e);
        }
        (n * 1000) as u64
    });
}

fn bench_archetype_fragmentation() {
    // Симулируем сценарий с множеством разных архетипов (как в реальной игре)
    const N: usize = 10_000;

    bench("mixed archetypes spawn (10k)", || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        world.register_component::<Mass>();

        for i in 0..N {
            match i % 4 {
                0 => { world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 },)); }
                1 => { world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 })); }
                2 => { world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, Health { current: 100.0, max: 100.0 })); }
                _ => { world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, Health { current: 100.0, max: 100.0 }, Mass(1.0))); }
            }
        }
        N as u64
    });
}

// ── Main ───────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Performance Benchmark ===");
    println!("Build: {}", if cfg!(debug_assertions) {
        "DEBUG (запусти с --release для реальных цифр!)"
    } else {
        "RELEASE"
    });
    println!();

    const N: usize = 100; // 100k entity

    println!("── Spawn ─────────────────────────────────────────────────");
    bench_spawn_bundle(N);
    bench_spawn_builder(N);

    println!("\n── Query ─────────────────────────────────────────────────");
    bench_query_iter(N);
    bench_query_iter_write(N);

    println!("\n── Structural changes ────────────────────────────────────");
    bench_insert_component(N);
    bench_remove_component(N);
    bench_despawn(N);

    println!("\n── Fragmentation ─────────────────────────────────────────");
    bench_archetype_fragmentation();

    println!("\n── Archetype stats ───────────────────────────────────────");
    {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        world.register_component::<Mass>();

        for i in 0..N * 1000 {
            let fi = i as f32;
            world.spawn_bundle((
                Position { x: fi, y: fi * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Health { current: 100.0, max: 100.0 },
            ));
        }
        println!(
            "  {}k entities → {} archetypes, {} components registered",
            N,
            world.archetype_count(),
            world.entity_count(), // просто чтобы не оптимизировало
        );
    }
}
