/// Apex ECS — Performance Benchmark
/// cargo run -p apex-examples --example perf --release

use std::time::Instant;
use apex_core::prelude::*;

#[derive(Clone, Copy)] struct Position { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Velocity { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Health   { current: f32, max: f32  }
#[derive(Clone, Copy)] struct Mass(f32);
#[derive(Clone, Copy)] struct Player;
#[derive(Clone, Copy)] struct Enemy;

// ── Ресурсы для бенчмарков ────────────────────────────────────
#[derive(Clone, Copy)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Clone, Copy, Default)]
struct FrameCounter { count: u64 }

// ── События для бенчмарков ────────────────────────────────────
#[derive(Clone, Copy)]
struct DamageEvent { target_id: u32, amount: f32 }

#[derive(Clone, Copy)]
struct CollisionEvent { a: u32, b: u32 }

// ── Bench harness ─────────────────────────────────────────────

fn bench<F: FnMut() -> u64>(label: &str, mut f: F) {
    f(); // warmup
    const RUNS: u32 = 7;
    let mut times = Vec::with_capacity(RUNS as usize);
    for _ in 0..RUNS {
        let t = Instant::now();
        let n = f();
        times.push((t.elapsed(), n));
    }
    times.sort_by_key(|(d, _)| *d);
    let (med, n) = times[RUNS as usize / 2];
    let ns     = med.as_nanos() as f64;
    let ns_op  = if n > 0 { ns / n as f64 } else { ns };
    let mops   = if med.as_secs_f64() > 0.0 {
        n as f64 / med.as_secs_f64() / 1e6
    } else {
        f64::INFINITY
    };
    println!(
        "  {:<60} {:>8.2} ns/op  {:>8.2} M ops/s",
        label, ns_op, mops
    );
}

fn make_world(n: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();
    let entities = (0..n)
        .map(|i| {
            let f = i as f32;
            world.spawn_bundle((
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
                Health   { current: 100.0, max: 100.0 },
            ))
        })
        .collect();
    (world, entities)
}

// ── [NEW] Resources benchmark ─────────────────────────────────

fn bench_resources(n: usize) {
    println!("── Resources ──────────────────────────────────────────────────────────────────");

    // insert_resource
    bench(&format!("insert_resource ({n} types, fresh world)"), || {
        let mut world = World::new();
        for i in 0..(n as u64) {
            // Один тип ресурса — перезаписываем (реалистичный кейс)
            world.insert_resource(PhysicsConfig {
                gravity: 9.8,
                dt: 0.016,
            });
            world.insert_resource(FrameCounter { count: i });
        }
        (n as u64) * 2
    });

    // resource<T> — горячий путь чтения
    {
        let mut world = World::new();
        world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
        world.insert_resource(FrameCounter::default());

        bench(&format!("resource::<T>() read ({n}k calls)"), || {
            let mut sum = 0.0f32;
            for _ in 0..n * 1000 {
                sum += world.resource::<PhysicsConfig>().gravity;
            }
            std::hint::black_box(sum);
            (n * 1000) as u64
        });

        bench(&format!("resource_mut::<T>() write ({n}k calls)"), || {
            for i in 0..n * 1000 {
                world.resource_mut::<FrameCounter>().count = i as u64;
            }
            std::hint::black_box(world.resource::<FrameCounter>().count);
            (n * 1000) as u64
        });

        bench(&format!("has_resource::<T>() check ({n}k calls)"), || {
            let mut found = 0u64;
            for _ in 0..n * 1000 {
                if world.has_resource::<PhysicsConfig>() { found += 1; }
            }
            found
        });
    }

    // Системы читающие ресурсы через World в горячем цикле
    {
        let (mut world, _) = make_world(n * 1000);
        world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });

        bench(&format!("system: read resource + query ({n}k entities)"), || {
            let dt      = world.resource::<PhysicsConfig>().dt;
            let gravity = world.resource::<PhysicsConfig>().gravity;
            Query::<(Read<Velocity>, Write<Position>)>::new(&world)
                .for_each_component(|(vel, pos)| {
                    pos.x += vel.x * dt;
                    pos.y += vel.y * dt;
                    pos.y -= gravity * dt * dt * 0.5;
                });
            (n * 1000) as u64
        });
    }
}

// ── [NEW] Events benchmark ────────────────────────────────────

fn bench_events(n: usize) {
    println!("\n── Events ─────────────────────────────────────────────────────────────────────");

    // send_event — горячий путь отправки
    bench(&format!("EventQueue::send ({n}k events)"), || {
        let mut world = World::new();
        world.add_event::<DamageEvent>();
        for i in 0..n * 1000 {
            world.send_event(DamageEvent {
                target_id: i as u32,
                amount:    10.0,
            });
        }
        (n * 1000) as u64
    });

    // iter_current — читать события текущего тика
    {
        let mut world = World::new();
        world.add_event::<DamageEvent>();
        for i in 0..n * 1000 {
            world.send_event(DamageEvent { target_id: i as u32, amount: 10.0 });
        }

        bench(&format!("iter_current {n}k events"), || {
            let mut sum = 0.0f32;
            for ev in world.events::<DamageEvent>().iter_current() {
                sum += ev.amount;
            }
            std::hint::black_box(sum);
            (n * 1000) as u64
        });
    }

    // double-buffer update — world.tick()
    {
        bench(&format!("world.tick() with {n}k events (double-buffer swap)"), || {
            let mut world = World::new();
            world.add_event::<DamageEvent>();
            world.add_event::<CollisionEvent>();
            for i in 0..n * 1000 {
                world.send_event(DamageEvent { target_id: i as u32, amount: 1.0 });
            }
            world.tick(); // swap buffers
            // Теперь читаем из previous
            let mut sum = 0.0f32;
            for ev in world.events::<DamageEvent>().iter_previous() {
                sum += ev.amount;
            }
            std::hint::black_box(sum);
            (n * 1000) as u64
        });
    }

    // iter_previous после world.tick() — типичный pipeline
    {
        let mut world = World::new();
        world.add_event::<DamageEvent>();

        // Симулируем N тиков с событиями
        bench(&format!("tick pipeline: send {n}k → tick → iter_previous"), || {
            // Тик N: отправляем события
            for i in 0..n * 1000 {
                world.send_event(DamageEvent { target_id: i as u32, amount: 5.0 });
            }
            // Тик N+1: swap
            world.tick();
            // Читаем события предыдущего тика
            let mut processed = 0u64;
            for _ in world.events::<DamageEvent>().iter_previous() {
                processed += 1;
            }
            // Очищаем для следующей итерации бенчмарка
            world.tick(); // уберём previous
            processed
        });
    }

    // Реалистичный сценарий: damage system с Query + Events
    {
        let (mut world, _entities) = make_world(n * 1000);
        world.add_event::<DamageEvent>();

        // Наносим урон всем
        bench(&format!("damage system: query {n}k + send events"), || {
            // Система атаки: проходим по всем entity, шлём DamageEvent
            let targets: Vec<u32> = {
                let mut v = Vec::with_capacity(n * 1000);
                Query::<Read<Health>>::new(&world).for_each(|e, _| {
                    v.push(e.index());
                });
                v
            };
            for (i, target_id) in targets.into_iter().enumerate() {
                if i % 3 == 0 { // каждый 3й получает урон
                    world.send_event(DamageEvent { target_id, amount: 10.0 });
                }
            }
            let sent = world.events::<DamageEvent>().len_current() as u64;
            world.tick();
            sent.max(1)
        });
    }

    // send_batch vs individual send
    {
        bench(&format!("send_batch {n}k events (vs individual)"), || {
            let mut world = World::new();
            world.add_event::<CollisionEvent>();
            world.events_mut::<CollisionEvent>().send_batch(
                (0..n * 1000).map(|i| CollisionEvent { a: i as u32, b: (i + 1) as u32 })
            );
            world.events::<CollisionEvent>().len_current() as u64
        });
    }

    // Многотиповые события — проверяем overhead EventRegistry
    bench(&format!("update_all with 2 event types ({n}k events each)"), || {
        let mut world = World::new();
        world.add_event::<DamageEvent>();
        world.add_event::<CollisionEvent>();
        for i in 0..n * 1000 {
            world.send_event(DamageEvent    { target_id: i as u32, amount: 1.0 });
            world.send_event(CollisionEvent { a: i as u32, b: (i+1) as u32 });
        }
        world.tick(); // update_all
        ((n * 1000) * 2) as u64
    });
}

// ── Spawn ─────────────────────────────────────────────────────

fn bench_spawn(n: usize) {
    println!("\n── Spawn ──────────────────────────────────────────────────────────────────────");
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
                Health   { current: 100.0, max: 100.0 },
            ));
        }
        (n * 1000) as u64
    });
}

// ── Query vs CachedQuery ──────────────────────────────────────

fn bench_query_vs_cached(n: usize) {
    println!("\n── Query vs CachedQuery ───────────────────────────────────────────────────────");
    let (world, _) = make_world(n * 1000);

    bench(&format!("Query::new + for_each ({n}k)  [no cache]"), || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world)
            .for_each_component(|pos| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("CachedQuery + for_each ({n}k)  [cached]"), || {
        let mut sum = 0.0f32;
        world.query_typed::<Read<Position>>()
            .for_each_component(|pos| { sum += pos.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("Query<(Read<Vel>, Write<Pos>)> ({n}k)"), || {
        Query::<(Read<Velocity>, Write<Position>)>::new(&world)
            .for_each_component(|(vel, pos)| {
                pos.x += vel.x;
                pos.y += vel.y;
            });
        (n * 1000) as u64
    });
}

// ── Filters ───────────────────────────────────────────────────

fn bench_filters(n: usize) {
    println!("\n── Filters: With / Without ────────────────────────────────────────────────────");
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();
    for i in 0..n * 1000 {
        let f = i as f32;
        if i % 2 == 0 {
            world.spawn_bundle((
                Position { x: f, y: 0.0, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Player,
            ));
        } else {
            world.spawn_bundle((
                Position { x: f, y: 0.0, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Enemy,
            ));
        }
    }
    bench(&format!("Query<Read<Pos>> all ({n}k)"), || {
        let mut s = 0.0f32;
        Query::<Read<Position>>::new(&world)
            .for_each_component(|p| { s += p.x; });
        std::hint::black_box(s);
        (n * 1000) as u64
    });
    bench(&format!("Query<(Read<Pos>, With<Player>)> ({}k)", n / 2), || {
        let mut s = 0.0f32;
        Query::<(Read<Position>, With<Player>)>::new(&world)
            .for_each_component(|(p, _)| { s += p.x; });
        std::hint::black_box(s);
        (n * 500) as u64
    });
}

// ── Change detection ──────────────────────────────────────────

fn bench_change_detection(n: usize) {
    println!("\n── Change detection ───────────────────────────────────────────────────────────");
    let (mut world, entities) = make_world(n * 1000);
    let tick_spawn = world.current_tick();
    world.tick();
    for &e in entities.iter().take(n * 100) {
        if let Some(p) = world.get_mut::<Position>(e) { p.x += 1.0; }
    }

    bench(&format!("Changed<Pos> all {n}k (baseline)"), || {
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
}

// ── Relations ─────────────────────────────────────────────────

fn bench_relations(n: usize) {
    println!("\n── Relations ──────────────────────────────────────────────────────────────────");

    let children_per_parent = 10usize;
    let parent_count        = n * 100;
    let total               = parent_count * (1 + children_per_parent);

    bench(
        &format!("add_relation ChildOf ({total} entities, {parent_count} parents)"),
        || {
            let mut world = World::new();
            world.register_component::<Position>();
            let parents: Vec<Entity> = (0..parent_count)
                .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 },)))
                .collect();
            for &parent in &parents {
                for j in 0..children_per_parent {
                    let child = world.spawn_bundle((
                        Position { x: j as f32, y: 0.0, z: 0.0 },
                    ));
                    world.add_relation(child, ChildOf, parent);
                }
            }
            total as u64
        },
    );

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

    bench(
        &format!("query_relation<ChildOf> — {children_per_parent} children"),
        || {
            let mut s = 0.0f32;
            for (_, pos) in world.query_relation::<ChildOf, Read<Position>>(ChildOf, test_parent) {
                s += pos.x;
            }
            std::hint::black_box(s);
            children_per_parent as u64
        },
    );

    bench(
        &format!("has_relation O(1) SubjectIndex ({} checks)", parent_count.min(n * 1000)),
        || {
            let mut found = 0u64;
            for (i, &parent) in parents.iter().enumerate().take(n * 1000) {
                if let Some(child) = world.children_of(ChildOf, parent).next() {
                    if world.has_relation(child, ChildOf, parents[i]) { found += 1; }
                }
            }
            found.max(1)
        },
    );
}

// ── Commands ──────────────────────────────────────────────────

fn bench_commands(n: usize) {
    println!("\n── Commands ───────────────────────────────────────────────────────────────────");
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

// ── Structural changes ────────────────────────────────────────

fn bench_structural(n: usize) {
    println!("\n── Structural changes ─────────────────────────────────────────────────────────");
    bench(&format!("insert component ({n}k)"), || {
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

    bench(&format!("despawn ({n}k)"), || {
        let (mut world, entities) = make_world(n * 1000);
        for e in entities { world.despawn(e); }
        (n * 1000) as u64
    });
}

fn main() {
    println!("=== Apex ECS — Performance Benchmark ===");
    println!(
        "Build: {}",
        if cfg!(debug_assertions) { "DEBUG ⚠" } else { "RELEASE ✓" }
    );
    println!();

    const N: usize = 100;

    bench_resources(N);
    bench_events(N);
    bench_spawn(N);
    bench_query_vs_cached(N);
    bench_filters(N);
    bench_change_detection(N);
    bench_relations(N);
    bench_commands(N);
    bench_structural(N);

    println!("\n── Summary ────────────────────────────────────────────────────────────────────");
    let (mut world, _) = make_world(N * 1000);
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.add_event::<DamageEvent>();
    println!("  {}k entities, {} archetypes", N, world.archetype_count());
    println!("  resources: {}", world.resources.len());
    println!(
        "  CachedQuery<Read<Pos>> len = {}",
        world.query_typed::<Read<Position>>().len()
    );
    println!("  current_tick = {:?}", world.current_tick());
}