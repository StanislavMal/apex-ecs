/// Apex ECS — Performance Benchmark
///
/// cargo run -p apex-examples --example perf --release

use std::time::{Duration, Instant};
use apex_core::prelude::*;

#[derive(Clone, Copy)] struct Position  { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Velocity  { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Health    { current: f32, max: f32  }
#[derive(Clone, Copy)] struct Mass(f32);
#[derive(Clone, Copy)] struct Player;
#[derive(Clone, Copy)] struct Enemy;

// ── Утилиты ────────────────────────────────────────────────────

fn bench<F: FnMut() -> u64>(label: &str, mut f: F) {
    f();
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
    println!("  {:<50} {:>7.2} ns/op  {:>8.2} M ops/s", label, ns_op, mops);
}

fn make_world(n: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();
    let entities = (0..n).map(|i| {
        let f = i as f32;
        world.spawn_bundle((
            Position { x: f, y: f * 0.5, z: 0.0 },
            Velocity { x: 1.0, y: 0.5, z: 0.0 },
            Health { current: 100.0, max: 100.0 },
        ))
    }).collect();
    (world, entities)
}

// ── Spawn ──────────────────────────────────────────────────────

fn bench_spawn(n: usize) {
    println!("── Spawn ─────────────────────────────────────────────────────────────────────");
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

// ── Query read / write ─────────────────────────────────────────

fn bench_query(n: usize) {
    println!("\n── Query read/write ──────────────────────────────────────────────────────────");
    let (world, _) = make_world(n * 1000);

    bench(&format!("Query<Read<Pos>> for_each ({n}k)"), || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|pos| { sum += pos.x + pos.y; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("Query<(Read<Pos>, Read<Vel>)> for_each ({n}k)"), || {
        let mut sum = 0.0f32;
        Query::<(Read<Position>, Read<Velocity>)>::new(&world)
            .for_each_component(|(pos, vel)| { sum += pos.x + vel.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("Query<(Read<Vel>, Write<Pos>)> for_each ({n}k)"), || {
        Query::<(Read<Velocity>, Write<Position>)>::new(&world)
            .for_each_component(|(vel, pos)| { pos.x += vel.x; pos.y += vel.y; });
        (n * 1000) as u64
    });
}

// ── With / Without фильтры ─────────────────────────────────────

fn bench_filters(n: usize) {
    println!("\n── Filters: With / Without ───────────────────────────────────────────────────");

    // Мир: половина entity — Player, половина — Enemy
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();

    for i in 0..n * 1000 {
        let f = i as f32;
        if i % 2 == 0 {
            world.spawn_bundle((Position { x: f, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, Player));
        } else {
            world.spawn_bundle((Position { x: f, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, Enemy));
        }
    }

    println!("  World: {}k entities, {} archetypes (Player arch + Enemy arch)",
        n, world.archetype_count());

    // Без фильтра — все entity
    bench(&format!("Query<Read<Pos>> no filter ({n}k)"), || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|pos| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    // With<Player> — только половина
    bench(&format!("Query<(Read<Pos>, With<Player>)> ({}k)", n/2), || {
        let mut sum = 0.0f32;
        Query::<(Read<Position>, With<Player>)>::new(&world)
            .for_each_component(|(pos, _)| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 500) as u64
    });

    // Without<Enemy> — только Player entity
    bench(&format!("Query<(Read<Pos>, Without<Enemy>)> ({}k)", n/2), || {
        let mut sum = 0.0f32;
        Query::<(Read<Position>, Without<Enemy>)>::new(&world)
            .for_each_component(|(pos, _)| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 500) as u64
    });

    // With<Player> + Without<Enemy> — комбинированный фильтр
    bench(&format!("Query<(Read<Pos>, With<Player>, Without<Enemy>)> ({}k)", n/2), || {
        let mut sum = 0.0f32;
        Query::<(Read<Position>, With<Player>, Without<Enemy>)>::new(&world)
            .for_each_component(|(pos, _, _)| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 500) as u64
    });
}

// ── Change detection ───────────────────────────────────────────

fn bench_change_detection(n: usize) {
    println!("\n── Change detection: Changed<T> ──────────────────────────────────────────────");

    let (mut world, entities) = make_world(n * 1000);

    // Тик 1: spawn (все компоненты помечены как changed)
    let tick_after_spawn = world.current_tick();

    // Тик 2: изменяем только 10% entity
    world.tick();
    let changed_count = n * 100; // 10%
    for &e in entities.iter().take(changed_count) {
        if let Some(pos) = world.get_mut::<Position>(e) {
            pos.x += 1.0;
        }
    }
    let tick_after_partial_update = world.current_tick();

    // Changed<Pos> после spawn — все entity changed
    bench(&format!("Changed<Pos> after spawn — all {n}k changed"), || {
        let mut count = 0u64;
        Query::<Changed<Position>>::new_with_tick(&world, Tick::ZERO)
            .for_each_component(|_pos| { count += 1; });
        count
    });

    // Changed<Pos> после частичного обновления — только 10%
    bench(&format!("Changed<Pos> after 10% update — {changed_count} changed"), || {
        let mut count = 0u64;
        Query::<Changed<Position>>::new_with_tick(&world, tick_after_spawn)
            .for_each_component(|_pos| { count += 1; });
        count
    });

    // Для сравнения: Read<Pos> без фильтра
    bench(&format!("Read<Pos> no filter (baseline {n}k)"), || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|pos| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    println!("  Verified: {} entities changed in tick {:?}",
        Query::<Changed<Position>>::new_with_tick(&world, tick_after_spawn).len(),
        tick_after_partial_update);
}

// ── Structural changes ─────────────────────────────────────────

fn bench_structural(n: usize) {
    println!("\n── Structural changes ────────────────────────────────────────────────────────");
    bench(&format!("insert new component ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Mass>();
        let entities: Vec<Entity> = (0..n * 1000)
            .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 })))
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
            .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, Mass(1.0))))
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
    println!("\n── Commands ──────────────────────────────────────────────────────────────────");
    bench(&format!("Commands::despawn + apply ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut cmds = Commands::with_capacity(n * 1000);
        Query::<Read<Health>>::new(&world).for_each(|entity, _| { cmds.despawn(entity); });
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

// ── Summary ────────────────────────────────────────────────────

fn print_summary(n: usize) {
    println!("\n── Summary ───────────────────────────────────────────────────────────────────");
    let (world, _) = make_world(n * 1000);
    println!("  {}k entities, {} archetypes", n, world.archetype_count());
    println!("  Query<Read<Pos>>                  len = {}", Query::<Read<Position>>::new(&world).len());
    println!("  Query<(Read<Pos>, With<Player>)>  len = {} (Player not registered → 0 expected)",
        Query::<(Read<Position>, With<Player>)>::new(&world).len());
    println!("  current_tick = {:?}", world.current_tick());
}

fn main() {
    println!("=== Apex ECS — Performance Benchmark ===");
    println!("Build: {}", if cfg!(debug_assertions) { "DEBUG ⚠ запусти с --release!" } else { "RELEASE ✓" });
    println!();

    const N: usize = 100;

    bench_spawn(N);
    bench_query(N);
    bench_filters(N);
    bench_change_detection(N);
    bench_structural(N);
    bench_commands(N);
    print_summary(N);
}
