/// Apex ECS — Performance Benchmark
/// cargo run -p apex-examples --example perf --release

use std::time::{Duration, Instant};
use apex_core::prelude::*;

#[derive(Clone, Copy)] struct Position { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Velocity { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Health   { current: f32, max: f32  }
#[derive(Clone, Copy)] struct Mass(f32);
#[derive(Clone, Copy)] struct Player;
#[derive(Clone, Copy)] struct Enemy;

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
    let ns = med.as_nanos() as f64;
    let ns_op = if n > 0 { ns / n as f64 } else { ns };
    let mops = if med.as_secs_f64() > 0.0 { n as f64 / med.as_secs_f64() / 1e6 } else { f64::INFINITY };
    println!("  {:<52} {:>7.2} ns/op  {:>8.2} M ops/s", label, ns_op, mops);
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
}

// ── Query vs CachedQuery ───────────────────────────────────────

fn bench_query_vs_cached(n: usize) {
    println!("\n── Query vs CachedQuery ──────────────────────────────────────────────────────");
    let (world, _) = make_world(n * 1000);

    bench(&format!("Query::new + for_each ({n}k)  [no cache]"), || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|pos| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("CachedQuery::new + for_each ({n}k)  [cached]"), || {
        let mut sum = 0.0f32;
        world.query_typed::<Read<Position>>().for_each_component(|pos| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("Query<(Read<Vel>, Write<Pos>)> ({n}k)"), || {
        Query::<(Read<Velocity>, Write<Position>)>::new(&world)
            .for_each_component(|(vel, pos)| { pos.x += vel.x; pos.y += vel.y; });
        (n * 1000) as u64
    });

    bench(&format!("CachedQuery<(Read<Vel>, Write<Pos>)> ({n}k)"), || {
        world.query_typed::<(Read<Velocity>, Write<Position>)>()
            .for_each_component(|(vel, pos)| { pos.x += vel.x; pos.y += vel.y; });
        (n * 1000) as u64
    });
}

// ── Filters ────────────────────────────────────────────────────

fn bench_filters(n: usize) {
    println!("\n── Filters: With / Without ───────────────────────────────────────────────────");
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
    println!("  {} archetypes (Player + Enemy)", world.archetype_count());

    bench(&format!("Query<Read<Pos>> all ({n}k)"), || {
        let mut s = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|p| { s += p.x; });
        std::hint::black_box(s); (n * 1000) as u64
    });
    bench(&format!("Query<(Read<Pos>, With<Player>)> ({}k)", n/2), || {
        let mut s = 0.0f32;
        Query::<(Read<Position>, With<Player>)>::new(&world)
            .for_each_component(|(p, _)| { s += p.x; });
        std::hint::black_box(s); (n * 500) as u64
    });
    bench(&format!("Query<(Read<Pos>, Without<Enemy>)> ({}k)", n/2), || {
        let mut s = 0.0f32;
        Query::<(Read<Position>, Without<Enemy>)>::new(&world)
            .for_each_component(|(p, _)| { s += p.x; });
        std::hint::black_box(s); (n * 500) as u64
    });
}

// ── Change detection ───────────────────────────────────────────

fn bench_change_detection(n: usize) {
    println!("\n── Change detection ──────────────────────────────────────────────────────────");
    let (mut world, entities) = make_world(n * 1000);
    let tick_spawn = world.current_tick();
    world.tick();
    for &e in entities.iter().take(n * 100) {
        if let Some(p) = world.get_mut::<Position>(e) { p.x += 1.0; }
    }

    bench(&format!("Changed<Pos> all {n}k changed (baseline)"), || {
        let mut c = 0u64;
        Query::<Changed<Position>>::new_with_tick(&world, Tick::ZERO)
            .for_each_component(|_| { c += 1; });
        c
    });
    bench(&format!("Changed<Pos> 10% changed ({} entities)", n * 100), || {
        let mut c = 0u64;
        Query::<Changed<Position>>::new_with_tick(&world, tick_spawn)
            .for_each_component(|_| { c += 1; });
        c
    });
    bench(&format!("Read<Pos> baseline ({n}k)"), || {
        let mut s = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|p| { s += p.x; });
        std::hint::black_box(s); (n * 1000) as u64
    });
}

// ── Relations ──────────────────────────────────────────────────

fn bench_relations(n: usize) {
    println!("\n── Relations ─────────────────────────────────────────────────────────────────");

    // Сценарий: N родителей, каждый с 10 детьми
    let children_per_parent = 10usize;
    let parent_count = n * 100; // 10k родителей при n=100
    let total = parent_count * (1 + children_per_parent);

    bench(&format!("add_relation ChildOf ({total} entities, {parent_count} parents)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        let parents: Vec<Entity> = (0..parent_count)
            .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 },)))
            .collect();
        for &parent in &parents {
            for j in 0..children_per_parent {
                let child = world.spawn_bundle((Position { x: j as f32, y: 0.0, z: 0.0 },));
                world.add_relation(child, ChildOf, parent);
            }
        }
        total as u64
    });

    // Бенчмарк query_relation для конкретного родителя
    let mut world = World::new();
    world.register_component::<Position>();
    let parents: Vec<Entity> = (0..parent_count)
        .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 },)))
        .collect();
    for &parent in &parents {
        for j in 0..children_per_parent {
            let child = world.spawn_bundle((Position { x: j as f32, y: 0.0, z: 0.0 },));
            world.add_relation(child, ChildOf, parent);
        }
    }
    let test_parent = parents[0];
    println!("  Archetypes after relation setup: {}", world.archetype_count());

    bench(&format!("query_relation<ChildOf>(parent) — {children_per_parent} children"), || {
        let mut s = 0.0f32;
        for (_, pos) in world.query_relation::<ChildOf, Read<Position>>(ChildOf, test_parent) {
            s += pos.x;
        }
        std::hint::black_box(s);
        children_per_parent as u64
    });

    bench(&format!("children_of<ChildOf>(parent) — {children_per_parent} children"), || {
        let mut count = 0u64;
        for _ in world.children_of(ChildOf, test_parent) { count += 1; }
        count
    });

    bench(&format!("has_relation check ({}k)", n), || {
        let mut count = 0u64;
        for &parent in parents.iter().take(n * 1000 / children_per_parent) {
            if world.has_relation(parents[0], ChildOf, parent) { count += 1; }
        }
        count.max(1)
    });
}

// ── Commands ───────────────────────────────────────────────────

fn bench_commands(n: usize) {
    println!("\n── Commands (typed enum, no Box alloc for Despawn) ───────────────────────────");
    bench(&format!("Commands::despawn + apply ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut cmds = Commands::with_capacity(n * 1000);
        Query::<Read<Health>>::new(&world).for_each(|e, _| { cmds.despawn(e); });
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

// ── Structural changes ─────────────────────────────────────────

fn bench_structural(n: usize) {
    println!("\n── Structural changes ────────────────────────────────────────────────────────");
    bench(&format!("insert component ({n}k)"), || {
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

fn main() {
    println!("=== Apex ECS — Performance Benchmark ===");
    println!("Build: {}", if cfg!(debug_assertions) { "DEBUG ⚠" } else { "RELEASE ✓" });
    println!();

    const N: usize = 100;

    bench_spawn(N);
    bench_query_vs_cached(N);
    bench_filters(N);
    bench_change_detection(N);
    bench_relations(N);
    bench_commands(N);
    bench_structural(N);

    println!("\n── Summary ───────────────────────────────────────────────────────────────────");
    let (world, _) = make_world(N * 1000);
    println!("  {}k entities, {} archetypes", N, world.archetype_count());
    println!("  CachedQuery<Read<Pos>> len = {}", world.query_typed::<Read<Position>>().len());
    println!("  current_tick = {:?}", world.current_tick());
}
