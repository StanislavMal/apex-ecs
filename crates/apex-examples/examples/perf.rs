/// Apex ECS — Performance Benchmark
/// cargo run -p apex-examples --example perf --release
/// Параллельный режим:
/// cargo run -p apex-examples --example perf --release --features parallel

use std::time::Instant;
use apex_core::prelude::*;
use apex_scheduler::{Scheduler, ParSystem, SystemContext};
use apex_core::access::AccessDescriptor;

#[derive(Clone, Copy)] struct Position { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Velocity { x: f32, y: f32, z: f32 }
#[derive(Clone, Copy)] struct Health   { current: f32, max: f32  }
#[derive(Clone, Copy)] struct Mass(f32);
#[derive(Clone, Copy)] struct Player;
#[derive(Clone, Copy)] struct Enemy;
// Дополнительные компоненты для демонстрации параллелизма без конфликтов
#[derive(Clone, Copy)] struct Temperature(f32);
#[derive(Clone, Copy)] struct Mana       { current: f32, max: f32 }

#[derive(Clone, Copy)] struct PhysicsConfig { gravity: f32, dt: f32 }
#[derive(Clone, Copy, Default)] struct FrameCounter { count: u64 }
#[derive(Clone, Copy)] struct DamageEvent  { target_id: u32, amount: f32 }
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
        "  {:<64} {:>8.2} ns/op  {:>8.2} M ops/s",
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

/// Создаёт World с 4 независимыми компонентами — для демонстрации
/// параллелизма без каких-либо Write-конфликтов между системами.
fn make_world_4comp(n: usize) -> World {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Temperature>();
    world.register_component::<Mana>();
    world.spawn_many_silent(n, |i| {
        let f = i as f32;
        (
            Position    { x: f, y: f * 0.5, z: 0.0 },
            Velocity    { x: 1.0, y: 0.5, z: 0.0 },
            Health      { current: 100.0, max: 100.0 },
            Temperature(20.0 + f * 0.001),
            Mana        { current: 50.0, max: 100.0 },
        )
    });
    world
}

// ── Batch Allocator benchmark ──────────────────────────────────

fn bench_batch_allocator(n: usize) {
    println!("── Batch Entity Allocator ──────────────────────────────────────────────────────");

    // spawn_bundle по одному — baseline
    bench(&format!("spawn_bundle loop      ({n}k)  [baseline]"), || {
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

    // spawn_many с Vec<Entity>
    bench(&format!("spawn_many             ({n}k)  [batch+collect]"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();
        let v = world.spawn_many(n * 1000, |i| {
            let f = i as f32;
            (
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
                Health   { current: 100.0, max: 100.0 },
            )
        });
        v.len() as u64
    });

    // spawn_many_silent — без Vec<Entity>
    bench(&format!("spawn_many_silent      ({n}k)  [batch, no collect]"), || {
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

    // 1 компонент
    bench(&format!("spawn_many_silent 1comp ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.spawn_many_silent(n * 1000, |i| {
            (Position { x: i as f32, y: 0.0, z: 0.0 },)
        });
        (n * 1000) as u64
    });

    // 4 компонента
    bench(&format!("spawn_many_silent 4comp ({n}k)"), || {
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

    // allocate_batch напрямую — изолируем overhead аллокатора
    bench(&format!("EntityAllocator::allocate_batch ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Player>();
        world.spawn_many_silent(n * 1000, |_| (Player,));
        (n * 1000) as u64
    });
}

// ── has_relation benchmark ─────────────────────────────────────

fn bench_has_relation(n: usize) {
    println!("\n── has_relation (SubjectIndex) ─────────────────────────────────────────────────");

    let parent_count = n * 100;
    let children_per = 8usize;

    let mut world = World::new();
    world.register_component::<Position>();

    let parents: Vec<Entity> = (0..parent_count)
        .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0, z: 0.0 },)))
        .collect();

    for &parent in &parents {
        for j in 0..children_per {
            let child = world.spawn_bundle((Position { x: j as f32, y: 0.0, z: 0.0 },));
            world.add_relation(child, ChildOf, parent);
        }
    }

    let pairs: Vec<(Entity, Entity)> = parents.iter()
        .filter_map(|&p| world.children_of(ChildOf, p).next().map(|c| (c, p)))
        .take(n * 1000)
        .collect();

    bench(&format!("has_relation TRUE  ({} checks)", pairs.len()), || {
        let mut found = 0u64;
        for &(child, parent) in &pairs {
            if world.has_relation(child, ChildOf, parent) { found += 1; }
        }
        found.max(1)
    });

    bench(&format!("has_relation FALSE ({} checks, early-exit)", pairs.len()), || {
        let mut found = 0u64;
        for (i, &(child, _)) in pairs.iter().enumerate() {
            if world.has_relation(child, ChildOf, parents[(i + 1) % parents.len()]) {
                found += 1;
            }
        }
        (pairs.len() as u64 - found).max(1)
    });
}

// ── Scheduler benchmark ────────────────────────────────────────

fn bench_scheduler(n: usize) {
    println!("\n── Hybrid Scheduler ────────────────────────────────────────────────────────────");

    struct MoveSys;
    impl ParSystem for MoveSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Velocity>, Write<Position>), _>(|(v, p)| {
                p.x += v.x; p.y += v.y;
            });
        }
    }

    struct HpSys;
    impl ParSystem for HpSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Health>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Health>, _>(|hp| {
                hp.current = hp.current.min(hp.max);
            });
        }
    }

    // 1 ParSystem
    bench(&format!("1 ParSystem: movement ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("move", MoveSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // 2 независимые ParSystem → 1 Stage
    bench(&format!("2 ParSystem no-conflict ({n}k, 1 stage)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("move", MoveSys);
        sched.add_par_system("hp",   HpSys);
        sched.compile().unwrap();
        debug_assert_eq!(sched.stages().unwrap().len(), 1);
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // FnParSystem с ресурсом
    bench(&format!("FnParSystem + resource ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
        let mut sched = Scheduler::new();
        sched.add_fn_par_system(
            "physics",
            |ctx: SystemContext<'_>| {
                let dt = ctx.resource::<PhysicsConfig>().dt;
                ctx.for_each_component::<Write<Position>, _>(|pos| {
                    pos.x += dt;
                });
            },
            AccessDescriptor::new().read::<PhysicsConfig>().write::<Position>(),
        );
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // Sequential система — для сравнения
    bench(&format!("1 Sequential system   ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_system("move", |world: &mut World| {
            Query::<(Read<Velocity>, Write<Position>)>::new(world)
                .for_each_component(|(v, p)| { p.x += v.x; p.y += v.y; });
        });
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // compile overhead
    bench("compile() 10 mixed systems", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("m1", MoveSys);
        sched.add_par_system("m2", HpSys);
        for i in 0..4 {
            sched.add_system(format!("seq_{i}"), |_| {});
        }
        sched.add_par_system("m3", MoveSys);
        sched.add_par_system("m4", HpSys);
        for i in 0..2 {
            sched.add_system(format!("seq2_{i}"), |_| {});
        }
        sched.compile().unwrap();
        1
    });

    // Демо debug_plan
    {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics",  MoveSys);
        sched.add_par_system("hp_clamp", HpSys);
        sched.add_system("commands", |_| {});
        sched.add_par_system("ai", MoveSys);
        sched.compile().unwrap();
        println!("  Mixed pipeline plan:\n{}", sched.debug_plan());
    }
}

// ── Параллельный планировщик ───────────────────────────────────

/// Бенчмарк реального параллелизма через rayon.
///
/// Сравниваем sequential vs parallel выполнение одного и того же
/// Stage из N независимых систем. Каждая система выполняет тяжёлую
/// операцию (sin/cos) чтобы параллельный выигрыш был виден.
///
/// Ожидаемый результат на 2+ ядрах: speedup ~1.5–2x для 2 систем,
/// ~2–4x для 4 систем (ограничен Rayon thread-pool overhead).
#[cfg(feature = "parallel")]
fn bench_parallel_scheduler(n: usize) {
    println!("\n── Parallel Scheduler (rayon) ──────────────────────────────────────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());

    // Четыре системы без конфликтов — каждая пишет в свой компонент.
    // Используем тяжёлую математику чтобы thread-spawn overhead был мал
    // по сравнению с полезной работой.

    struct PhysSys;
    impl ParSystem for PhysSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Velocity>, Write<Position>), _>(|(v, p)| {
                // Очень тяжёлая операция - итеративный расчёт
                let mut x = p.x + v.x;
                let mut y = p.y + v.y;
                for _ in 0..50 {
                    x = x.sin().cos().exp().sqrt();
                    y = y.cos().sin().ln_1p().abs();
                }
                p.x = x;
                p.y = y;
                p.z = x * y;
            });
        }
    }

    struct HpSys;
    impl ParSystem for HpSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Health>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Health>, _>(|hp| {
                // Тяжёлые вычисления с health
                let mut val = hp.current;
                for i in 0..30 {
                    val = (val * 0.9999_f32).max(0.0).min(hp.max);
                    val = val.sqrt().sin().cos().exp();
                    if i % 5 == 0 {
                        val = val.ln_1p().abs();
                    }
                }
                hp.current = val;
            });
        }
    }

    struct TempSys;
    impl ParSystem for TempSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Temperature>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Temperature>, _>(|t| {
                // Много итераций уравнения охлаждения
                let mut temp = t.0;
                for _ in 0..40 {
                    temp = temp + (20.0 - temp) * 0.001;
                    temp = temp.sin().cos().exp().sqrt();
                }
                t.0 = temp;
            });
        }
    }

    struct ManaSys;
    impl ParSystem for ManaSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Mana>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Mana>, _>(|m| {
                // Сложные вычисления с манной
                let mut mana = m.current;
                for i in 0..35 {
                    mana = (mana + 0.1).min(m.max);
                    mana = mana.ln_1p().sin().cos().exp();
                    if i % 7 == 0 {
                        mana = mana.sqrt().abs();
                    }
                }
                m.current = mana;
            });
        }
    }

    // ── 2 системы: Sequential vs Parallel ─────────────────────

    bench(&format!("2 systems SEQUENTIAL  ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.compile().unwrap();
        assert!(sched.stages().unwrap()[0].is_parallelizable());
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("2 systems PARALLEL    ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.compile().unwrap();
        sched.run(&mut world); // run() выбирает parallel путь при feature = "parallel"
        (n * 1000) as u64
    });

    // ── 4 системы: Sequential vs Parallel ─────────────────────

    bench(&format!("4 systems SEQUENTIAL  ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_par_system("temp", TempSys);
        sched.add_par_system("mana", ManaSys);
        sched.compile().unwrap();
        assert_eq!(sched.stages().unwrap().len(), 1,
            "все 4 системы должны быть в одном Stage");
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("4 systems PARALLEL    ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_par_system("temp", TempSys);
        sched.add_par_system("mana", ManaSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    // ── Mixed pipeline: 4 par → seq → 2 par ───────────────────

    bench(&format!("mixed pipeline SEQ    ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_par_system("temp", TempSys);
        sched.add_par_system("mana", ManaSys);
        sched.add_system("barrier", |_world: &mut World| {
            // Structural: в реале — apply commands
        });
        sched.add_par_system("phys2", PhysSys);
        sched.add_par_system("hp2",   HpSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("mixed pipeline PAR    ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_par_system("temp", TempSys);
        sched.add_par_system("mana", ManaSys);
        sched.add_system("barrier", |_world: &mut World| {});
        sched.add_par_system("phys2", PhysSys);
        sched.add_par_system("hp2",   HpSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    // ── Показываем план ───────────────────────────────────────
    {
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_par_system("temp", TempSys);
        sched.add_par_system("mana", ManaSys);
        sched.add_system("commands", |_| {});
        sched.add_par_system("phys2", PhysSys);
        sched.add_par_system("hp2",   HpSys);
        sched.compile().unwrap();
        println!("  Pipeline plan (parallel):\n{}", sched.debug_plan());
    }
}

// ── Resources benchmark ────────────────────────────────────────

fn bench_resources(n: usize) {
    println!("\n── Resources ───────────────────────────────────────────────────────────────────");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(FrameCounter::default());

    bench(&format!("resource::<T>() read  ({n}k)"), || {
        let mut sum = 0.0f32;
        for _ in 0..n * 1000 { sum += world.resource::<PhysicsConfig>().gravity; }
        std::hint::black_box(sum);
        (n * 1000) as u64
    });

    bench(&format!("resource_mut::<T>() write ({n}k)"), || {
        for i in 0..n * 1000 { world.resource_mut::<FrameCounter>().count = i as u64; }
        std::hint::black_box(world.resource::<FrameCounter>().count);
        (n * 1000) as u64
    });

    bench(&format!("has_resource::<T>() ({n}k)"), || {
        let mut found = 0u64;
        for _ in 0..n * 1000 {
            if world.has_resource::<PhysicsConfig>() { found += 1; }
        }
        found
    });
}

// ── Events benchmark ───────────────────────────────────────────

fn bench_events(n: usize) {
    println!("\n── Events ──────────────────────────────────────────────────────────────────────");

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
    println!("\n── Query ───────────────────────────────────────────────────────────────────────");
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
            .for_each_component(|(v, p)| { p.x += v.x; p.y += v.y; });
        (n * 1000) as u64
    });

    bench(&format!("Query<(Read<Pos>, With<Player>)> (0 results)"), || {
        let mut c = 0u64;
        Query::<(Read<Position>, With<Player>)>::new(&world)
            .for_each_component(|_| { c += 1; });
        c.max(1)
    });
}

// ── Structural changes ─────────────────────────────────────────

fn bench_structural(n: usize) {
    println!("\n── Structural changes ──────────────────────────────────────────────────────────");

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

    bench(&format!("Commands::despawn + apply ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut cmds = Commands::with_capacity(n * 1000);
        Query::<Read<Health>>::new(&world).for_each(|e, _| { cmds.despawn(e); });
        cmds.apply(&mut world);
        (n * 1000) as u64
    });
}

fn main() {
    println!("=== Apex ECS — Performance Benchmark ===");
    println!("Build: {}", if cfg!(debug_assertions) { "DEBUG ⚠" } else { "RELEASE ✓" });
    #[cfg(feature = "parallel")]
    println!("Mode:  PARALLEL (rayon threads: {})", rayon::current_num_threads());
    #[cfg(not(feature = "parallel"))]
    println!("Mode:  sequential (compile with --features parallel for rayon)");
    println!();

    const N: usize = 1000; // 100K сущностей (100 * 100)

    bench_batch_allocator(N);
    bench_has_relation(N);
    bench_scheduler(N);
    bench_resources(N);
    bench_events(N);
    bench_query(N);
    bench_structural(N);

    #[cfg(feature = "parallel")]
    bench_parallel_scheduler(N);

    println!("\n── Summary ─────────────────────────────────────────────────────────────────────");
    let (mut world, _) = make_world(N * 1000);
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.add_event::<DamageEvent>();
    println!("  {}k entities, {} archetypes", N, world.archetype_count());
    println!("  resources:              {}", world.resource_count());
    println!("  CachedQuery<Pos> len:   {}", world.query_typed::<Read<Position>>().len());
    println!("  current_tick:           {:?}", world.current_tick());
}
