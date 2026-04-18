/// Apex ECS — Performance Benchmark
///
/// cargo run -p apex-examples --example perf --release

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

// ── Утилиты ────────────────────────────────────────────────────

fn bench<F: FnMut() -> u64>(label: &str, mut f: F) {
    f(); // прогрев
    const RUNS: u32 = 7;
    let mut times = Vec::with_capacity(RUNS as usize);
    for _ in 0..RUNS {
        let t = Instant::now();
        let n = f();
        times.push((t.elapsed(), n));
    }
    times.sort_by_key(|(d, _)| *d);
    let (med, n) = times[RUNS as usize / 2];
    print_result(label, med, n);
}

fn print_result(label: &str, d: Duration, n: u64) {
    let ns = d.as_nanos() as f64;
    let ns_op = if n > 0 { ns / n as f64 } else { ns };
    let mops = if d.as_secs_f64() > 0.0 { n as f64 / d.as_secs_f64() / 1e6 } else { f64::INFINITY };
    println!("  {:<42} {:>7.2} ns/op  {:>8.2} M ops/s  (n={n}, {d:?})", label, ns_op, mops);
}

fn make_world(n: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    let entities = (0..n)
        .map(|i| {
            let f = i as f32;
            world.spawn_bundle((
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
                Health { current: 100.0, max: 100.0 },
            ))
        })
        .collect();
    (world, entities)
}

// ── Spawn ──────────────────────────────────────────────────────

fn bench_spawn(n: usize) {
    bench(&format!("spawn_bundle 3 components ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        for i in 0..n * 1000 {
            let f = i as f32;
            world.spawn_bundle((
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Health { current: 100.0, max: 100.0 },
            ));
        }
        (n * 1000) as u64
    });

    bench(&format!("spawn builder chain ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        for i in 0..n * 1000 {
            let f = i as f32;
            world.spawn()
                .insert(Position { x: f, y: f * 0.5, z: 0.0 })
                .insert(Velocity { x: 1.0, y: 0.0, z: 0.0 })
                .insert(Health { current: 100.0, max: 100.0 })
                .id();
        }
        (n * 1000) as u64
    });
}

// ── Query iter ─────────────────────────────────────────────────

fn bench_query(n: usize) {
    let (world, _) = make_world(n * 1000);

    // Zero-cost Query::for_each
    bench(&format!("Query<Read<Pos>> for_each ({n}k)"), || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|pos| {
            sum += pos.x + pos.y;
        });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    // Zero-cost Query::iter
    bench(&format!("Query<Read<Pos>> iter ({n}k)"), || {
        let mut sum = 0.0f32;
        for pos in Query::<Read<Position>>::new(&world).iter_components() {
            sum += pos.x + pos.y;
        }
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    // Tuple query: два компонента
    bench(&format!("Query<(Read<Pos>, Read<Vel>)> for_each ({n}k)"), || {
        let mut sum = 0.0f32;
        Query::<(Read<Position>, Read<Velocity>)>::new(&world)
            .for_each_component(|(pos, vel)| {
                sum += pos.x + vel.x;
            });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    // Старый Box<dyn Iterator> для сравнения
    bench(&format!("QueryBuilder iter_one<Pos> ({n}k)  [legacy]"), || {
        let mut sum = 0.0f32;
        for (_, pos) in world.query().read::<Position>().iter_one::<Position>() {
            sum += pos.x + pos.y;
        }
        std::hint::black_box(sum);
        (n * 1000) as u64
    });
}

// ── Query write ────────────────────────────────────────────────

fn bench_query_write(n: usize) {
    let (mut world, _) = make_world(n * 1000);

    bench(&format!("Query<Write<Pos>> for_each mut ({n}k)"), || {
        Query::<Write<Position>>::new(&world).for_each_component(|pos| {
            pos.x += 1.0;
            pos.y += 0.5;
        });
        (n * 1000) as u64
    });

    bench(&format!("Query<(Read<Vel>, Write<Pos>)> for_each ({n}k)"), || {
        Query::<(Read<Velocity>, Write<Position>)>::new(&world)
            .for_each_component(|(vel, pos)| {
                pos.x += vel.x;
                pos.y += vel.y;
            });
        (n * 1000) as u64
    });

    // get_mut per entity для сравнения
    let (mut world2, entities) = make_world(n * 1000);
    bench(&format!("get_mut per entity ({n}k)           [legacy]"), || {
        for &e in &entities {
            if let Some(pos) = world2.get_mut::<Position>(e) {
                pos.x += 1.0;
            }
        }
        (n * 1000) as u64
    });
}

// ── Structural changes ─────────────────────────────────────────

fn bench_structural(n: usize) {
    bench(&format!("insert new component ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Mass>();
        let entities: Vec<Entity> = (0..n * 1000)
            .map(|i| world.spawn_bundle((
                Position { x: i as f32, y: 0.0, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
            )))
            .collect();
        for &e in &entities { world.insert(e, Mass(1.0)); }
        (n * 1000) as u64
    });

    bench(&format!("remove component ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Mass>();
        let entities: Vec<Entity> = (0..n * 1000)
            .map(|i| world.spawn_bundle((
                Position { x: i as f32, y: 0.0, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Mass(1.0),
            )))
            .collect();
        for &e in &entities { world.remove::<Mass>(e); }
        (n * 1000) as u64
    });

    bench(&format!("despawn ({n}k)"), || {
        let (mut world, entities) = make_world(n * 1000);
        for e in entities { world.despawn(e); }
        (n * 1000) as u64
    });
}

// ── Commands ───────────────────────────────────────────────────

fn bench_commands(n: usize) {
    bench(&format!("Commands::despawn + apply ({n}k)"), || {
        let (mut world, entities) = make_world(n * 1000);
        let mut cmds = Commands::with_capacity(n * 1000);
        // Симулируем: итерируем, накапливаем команды, применяем
        Query::<Read<Health>>::new(&world).for_each(|entity, _health| {
            cmds.despawn(entity);
        });
        cmds.apply(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("Commands::insert + apply ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Mass>();
        let entities: Vec<Entity> = (0..n * 1000)
            .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 },)))
            .collect();
        let mut cmds = Commands::with_capacity(n * 1000);
        for &e in &entities { cmds.insert(e, Mass(1.0)); }
        cmds.apply(&mut world);
        (n * 1000) as u64
    });
}

// ── Archetype fragmentation ────────────────────────────────────

fn bench_fragmentation() {
    const N: usize = 50_000;
    bench("mixed archetypes spawn (50k, 4 types)", || {
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

    // Query по Position через 4 архетипа
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
    println!("  Archetypes after fragmented spawn: {}", world.archetype_count());

    bench("Query<Read<Pos>> across 4 archetypes (50k)", || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|pos| { sum += pos.x; });
        std::hint::black_box(sum);
        N as u64
    });
}

// ── Main ───────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Performance Benchmark ===");
    println!("Build: {}", if cfg!(debug_assertions) {
        "DEBUG ⚠ запусти с --release!"
    } else {
        "RELEASE ✓"
    });
    println!();

    const N: usize = 100;

    println!("── Spawn ─────────────────────────────────────────────────────────────────");
    bench_spawn(N);

    println!("\n── Query read ────────────────────────────────────────────────────────────");
    bench_query(N);

    println!("\n── Query write ───────────────────────────────────────────────────────────");
    bench_query_write(N);

    println!("\n── Structural changes ────────────────────────────────────────────────────");
    bench_structural(N);

    println!("\n── Commands ──────────────────────────────────────────────────────────────");
    bench_commands(N);

    println!("\n── Fragmentation ─────────────────────────────────────────────────────────");
    bench_fragmentation();

    println!("\n── Summary ───────────────────────────────────────────────────────────────");
    {
        let (world, _) = make_world(N * 1000);
        println!("  {}k entities, {} archetypes", N, world.archetype_count());
        println!("  Query<Read<Pos>> len = {}", Query::<Read<Position>>::new(&world).len());
    }
}
