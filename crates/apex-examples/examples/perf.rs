/// Apex ECS — Performance Benchmark
/// cargo run -p apex-examples --example perf --release

use std::time::Instant;
use apex_core::prelude::*;
use apex_scheduler::{Scheduler, ParSystem, SystemContext, AccessDescriptor};

#[derive(Clone, Copy)] struct Position { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Velocity { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Health   { current: f32, max: f32  }
#[derive(Clone, Copy)] struct Mass(f32);
#[derive(Clone, Copy)] struct Player;
#[derive(Clone, Copy)] struct Enemy;

#[derive(Clone, Copy)] struct PhysicsConfig { gravity: f32, dt: f32 }
#[derive(Clone, Copy, Default)] struct FrameCounter { count: u64 }
#[derive(Clone, Copy)] struct DamageEvent { target_id: u32, amount: f32 }
#[derive(Clone, Copy)] struct CollisionEvent { a: u32, b: u32 }

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
    let ns    = med.as_nanos() as f64;
    let ns_op = if n > 0 { ns / n as f64 } else { ns };
    let mops  = if med.as_secs_f64() > 0.0 {
        n as f64 / med.as_secs_f64() / 1e6
    } else { f64::INFINITY };
    println!(
        "  {:<62} {:>8.2} ns/op  {:>8.2} M ops/s",
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
    let entities = (0..n).map(|i| {
        let f = i as f32;
        world.spawn_bundle((
            Position { x: f, y: f * 0.5, z: 0.0 },
            Velocity { x: 1.0, y: 0.5, z: 0.0 },
            Health   { current: 100.0, max: 100.0 },
        ))
    }).collect();
    (world, entities)
}

// ── has_relation benchmark ─────────────────────────────────────

fn bench_has_relation(n: usize) {
    println!("── has_relation (SubjectIndex fix) ────────────────────────────────────────────");

    let children_per_parent = 8usize;
    let parent_count        = n * 100;

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

    // Собираем пары для теста
    let pairs: Vec<(Entity, Entity)> = parents.iter()
        .filter_map(|&parent| {
            world.children_of(ChildOf, parent).next()
                .map(|child| (child, parent))
        })
        .take(n * 1000)
        .collect();

    println!("  Setup: {} parents, {} children each, {} test pairs",
        parent_count, children_per_parent, pairs.len()
    );

    // True path — kind_mask hit + binary_search
    bench(
        &format!("has_relation TRUE  ({} checks, kind_mask+bsearch)", pairs.len()),
        || {
            let mut found = 0u64;
            for &(child, parent) in &pairs {
                if world.has_relation(child, ChildOf, parent) { found += 1; }
            }
            found.max(1)
        },
    );

    // False path — kind_mask early exit (наиболее частый в реальном коде)
    bench(
        &format!("has_relation FALSE ({} checks, early-exit)", pairs.len()),
        || {
            let mut found = 0u64;
            for (i, &(child, _)) in pairs.iter().enumerate() {
                let wrong = parents[(i + 1) % parents.len()];
                if world.has_relation(child, ChildOf, wrong) { found += 1; }
            }
            (pairs.len() as u64 - found).max(1)
        },
    );

    // Множественные kinds — проверка kind_mask с несколькими битами
    // Добавляем второй RelationKind чтобы у entity было 2 relation kinds
    #[derive(Clone, Copy)] struct Likes;
    impl apex_core::relations::RelationKind for Likes {}

    // Добавляем Likes relation к первым 100 children
    let extra_pairs: Vec<(Entity, Entity)> = pairs.iter().take(100)
        .map(|&(child, parent)| {
            world.add_relation(child, Likes, parent);
            (child, parent)
        })
        .collect();

    bench(
        &format!("has_relation multi-kind ({} checks, 2 kinds set)", extra_pairs.len()),
        || {
            let mut found = 0u64;
            for &(child, parent) in &extra_pairs {
                if world.has_relation(child, ChildOf, parent) { found += 1; }
                if world.has_relation(child, Likes,   parent) { found += 1; }
            }
            found.max(1)
        },
    );
}

// ── Batch Spawn benchmark ──────────────────────────────────────

fn bench_spawn_batch(n: usize) {
    println!("\n── Batch Spawn vs spawn_bundle ────────────────────────────────────────────────");

    // Baseline: spawn_bundle по одному
    bench(&format!("spawn_bundle loop ({n}k)  [baseline]"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        for i in 0..n * 1000 {
            let f = i as f32;
            world.spawn_bundle((
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
                Health   { current: 100.0, max: 100.0 },
            ));
        }
        (n * 1000) as u64
    });

    // spawn_many — с возвратом Vec<Entity>
    bench(&format!("spawn_many         ({n}k)  [batch+collect]"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        let entities = world.spawn_many(n * 1000, |i| {
            let f = i as f32;
            (
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
                Health   { current: 100.0, max: 100.0 },
            )
        });
        entities.len() as u64
    });

    // spawn_many_silent — без Vec<Entity>
    bench(&format!("spawn_many_silent  ({n}k)  [batch, no collect]"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        world.spawn_many_silent(n * 1000, |i| {
            let f = i as f32;
            (
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
                Health   { current: 100.0, max: 100.0 },
            )
        });
        (n * 1000) as u64
    });

    // 1 компонент — минимальный overhead
    bench(&format!("spawn_many 1 comp  ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.spawn_many_silent(n * 1000, |i| {
            (Position { x: i as f32, y: 0.0, z: 0.0 },)
        });
        (n * 1000) as u64
    });

    // 8 компонентов — максимальный bundle
    bench(&format!("spawn_many 8 comp  ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        world.register_component::<Mass>();
        world.spawn_many_silent(n * 1000, |i| {
            let f = i as f32;
            (
                Position { x: f, y: 0.0, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
                Health   { current: 100.0, max: 100.0 },
                Mass(1.0),
            )
        });
        (n * 1000) as u64
    });
}

// ── Parallel Scheduler benchmark ───────────────────────────────

fn bench_scheduler(n: usize) {
    println!("\n── Hybrid Scheduler ───────────────────────────────────────────────────────────");

    // ParSystem реализации для бенчмарка
    struct MovementSys;
    impl ParSystem for MovementSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<(Read<Velocity>, Write<Position>)>()
               .for_each_component(|(vel, pos)| {
                   pos.x += vel.x;
                   pos.y += vel.y;
               });
        }
    }

    struct HealthSys;
    impl ParSystem for HealthSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Health>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<Write<Health>>().for_each_component(|hp| {
                hp.current = hp.current.min(hp.max);
            });
        }
    }

    // Одна ParSystem — baseline
    bench(&format!("1 ParSystem: movement ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // Две независимые ParSystem в одном Stage
    bench(&format!("2 ParSystem parallel stage ({n}k, no conflict)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSys);
        sched.add_par_system("health",   HealthSys);
        sched.compile().unwrap();
        // Проверяем что они в одном Stage
        debug_assert_eq!(sched.stages().unwrap().len(), 1);
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // Sequential система — baseline
    bench(&format!("1 Sequential system ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_system("movement", |world: &mut World| {
            Query::<(Read<Velocity>, Write<Position>)>::new(world)
                .for_each_component(|(vel, pos)| {
                    pos.x += vel.x;
                    pos.y += vel.y;
                });
        });
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // compile() overhead
    bench("compile() overhead (10 systems)", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("s1", MovementSys);
        sched.add_par_system("s2", HealthSys);
        for i in 0..8 {
            let name = format!("seq_{i}");
            sched.add_system(name, |_| {});
        }
        sched.compile().unwrap();
        1
    });

    // Stage detection — сколько Stage генерируется
    {
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSys);
        sched.add_par_system("health",   HealthSys);
        sched.add_system("commands", |_| {});
        sched.add_par_system("movement2", MovementSys);
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        println!("  Mixed pipeline stages: {} (par+par | seq | par)",
            stages.len()
        );
        println!("{}", sched.debug_plan());
    }
}

// ── Resources benchmark ────────────────────────────────────────

fn bench_resources(n: usize) {
    println!("\n── Resources ──────────────────────────────────────────────────────────────────");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(FrameCounter::default());

    bench(&format!("resource::<T>() ({n}k)"), || {
        let mut sum = 0.0f32;
        for _ in 0..n * 1000 { sum += world.resource::<PhysicsConfig>().gravity; }
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("resource_mut::<T>() ({n}k)"), || {
        for i in 0..n * 1000 {
            world.resource_mut::<FrameCounter>().count = i as u64;
        }
        std::hint::black_box(world.resource::<FrameCounter>().count);
        (n * 1000) as u64
    });
}

// ── Events benchmark ───────────────────────────────────────────

fn bench_events(n: usize) {
    println!("\n── Events ─────────────────────────────────────────────────────────────────────");

    bench(&format!("send + iter_current ({n}k)"), || {
        let mut world = World::new();
        world.add_event::<DamageEvent>();
        for i in 0..n * 1000 {
            world.send_event(DamageEvent { target_id: i as u32, amount: 10.0 });
        }
        let mut sum = 0.0f32;
        for ev in world.events::<DamageEvent>().iter_current() { sum += ev.amount; }
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("tick pipeline: send→tick→iter_prev ({n}k)"), || {
        let mut world = World::new();
        world.add_event::<DamageEvent>();
        for i in 0..n * 1000 {
            world.send_event(DamageEvent { target_id: i as u32, amount: 5.0 });
        }
        world.tick();
        let mut sum = 0.0f32;
        for ev in world.events::<DamageEvent>().iter_previous() { sum += ev.amount; }
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("send_batch ({n}k)"), || {
        let mut world = World::new();
        world.add_event::<CollisionEvent>();
        world.events_mut::<CollisionEvent>().send_batch(
            (0..n * 1000).map(|i| CollisionEvent { a: i as u32, b: (i + 1) as u32 })
        );
        world.events::<CollisionEvent>().len_current() as u64
    });
}

// ── Query benchmark ────────────────────────────────────────────

fn bench_query(n: usize) {
    println!("\n── Query ──────────────────────────────────────────────────────────────────────");
    let (world, _) = make_world(n * 1000);

    bench(&format!("Query::new + for_each ({n}k)"), || {
        let mut sum = 0.0f32;
        Query::<Read<Position>>::new(&world).for_each_component(|p| { sum += p.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("CachedQuery + for_each ({n}k)"), || {
        let mut sum = 0.0f32;
        world.query_typed::<Read<Position>>().for_each_component(|p| { sum += p.x; });
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("Query<(Read<Vel>, Write<Pos>)> ({n}k)"), || {
        Query::<(Read<Velocity>, Write<Position>)>::new(&world)
            .for_each_component(|(vel, pos)| { pos.x += vel.x; pos.y += vel.y; });
        (n * 1000) as u64
    });
}

// ── Structural changes ─────────────────────────────────────────

fn bench_structural(n: usize) {
    println!("\n── Structural changes ─────────────────────────────────────────────────────────");

    bench(&format!("insert component ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Mass>();
        let entities: Vec<Entity> = (0..n * 1000).map(|i| {
            world.spawn_bundle((
                Position { x: i as f32, y: 0.0, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
            ))
        }).collect();
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
    println!("Build: {}", if cfg!(debug_assertions) { "DEBUG ⚠" } else { "RELEASE ✓" });
    println!();

    const N: usize = 100;

    bench_has_relation(N);
    bench_spawn_batch(N);
    bench_scheduler(N);
    bench_resources(N);
    bench_events(N);
    bench_query(N);
    bench_structural(N);

    println!("\n── Summary ────────────────────────────────────────────────────────────────────");
    let (mut world, _) = make_world(N * 1000);
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.add_event::<DamageEvent>();
    println!("  {}k entities, {} archetypes", N, world.archetype_count());
    println!("  resources: {}", world.resource_count());
    println!("  CachedQuery<Read<Pos>> = {}", world.query_typed::<Read<Position>>().len());
    println!("  current_tick = {:?}", world.current_tick());
}