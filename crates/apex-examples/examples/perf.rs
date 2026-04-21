/// Apex ECS — Performance Benchmark
/// cargo run -p apex-examples --example perf --release
/// Параллельный режим:
/// cargo run -p apex-examples --example perf --release --features parallel

use std::time::Instant;
use apex_core::prelude::*;
use apex_scheduler::{Scheduler, ParSystem};
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

    // AutoSystem для сравнения производительности
    struct AutoMoveSys;
    impl AutoSystem for AutoMoveSys {
        type Query = (Read<Velocity>, Write<Position>);
        
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Self::Query, _>(|(v, p)| {
                p.x += v.x; p.y += v.y;
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

    // compile overhead - разные размеры графов
    bench("compile() 5 systems (2 par + 3 seq)", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("m1", MoveSys);
        sched.add_par_system("m2", HpSys);
        for i in 0..3 {
            sched.add_system(format!("seq_{i}"), |_| {});
        }
        sched.compile().unwrap();
        1
    });

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

    bench("compile() 20 systems (complex graph)", || {
        let mut sched = Scheduler::new();
        // Stage 0: 4 параллельные системы
        for i in 0..4 {
            sched.add_par_system(format!("par_a{i}"), MoveSys);
        }
        // Stage 1: 2 последовательные
        for i in 0..2 {
            sched.add_system(format!("seq_a{i}"), |_| {});
        }
        // Stage 2: 4 параллельные
        for i in 0..4 {
            sched.add_par_system(format!("par_b{i}"), HpSys);
        }
        // Stage 3: 4 последовательные
        for i in 0..4 {
            sched.add_system(format!("seq_b{i}"), |_| {});
        }
        // Stage 4: 6 параллельные
        for i in 0..6 {
            sched.add_par_system(format!("par_c{i}"), MoveSys);
        }
        sched.compile().unwrap();
        1
    });

    // Измерение overhead AutoSystem vs ParSystem при компиляции
    bench("compile() 10 AutoSystems", || {
        let mut sched = Scheduler::new();
        for i in 0..10 {
            sched.add_auto_system(format!("auto_{i}"), AutoMoveSys);
        }
        sched.compile().unwrap();
        1
    });

    // AutoSystem бенчмарк
    bench(&format!("1 AutoSystem ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_auto_system("auto_move", AutoMoveSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // Сравнение ParSystem vs AutoSystem
    bench(&format!("ParSystem vs AutoSystem ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("par", MoveSys);
        sched.add_auto_system("auto", AutoMoveSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
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
            AccessDescriptor::new().write::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Velocity>, Write<Position>), _>(|(v, p)| {
                // Реалистичные вычисления: движение с ускорением и трением
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                
                // Небольшое трение
                v.x *= 0.99;
                v.y *= 0.99;
                v.z *= 0.99;
                
                // Гравитация
                v.y -= 9.8 * 0.016;
            });
        }
    }

    struct HpSys;
    impl ParSystem for HpSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Health>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Health>, _>(|hp| {
                // Реалистичное обновление здоровья: регенерация и clamp
                hp.current = (hp.current + 0.1).min(hp.max).max(0.0);
            });
        }
    }

    struct TempSys;
    impl ParSystem for TempSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Temperature>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Temperature>, _>(|t| {
                // Реалистичное уравнение охлаждения
                t.0 += (20.0 - t.0) * 0.001;
            });
        }
    }

    struct ManaSys;
    impl ParSystem for ManaSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Mana>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Mana>, _>(|m| {
                // Реалистичная регенерация маны
                m.current = (m.current + 0.2).min(m.max);
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

    // ── Тест "тяжёлой нагрузки" для Parallel Scheduler ─────────────
    println!("\n── Heavy Workload Parallel Scheduler (AAA игры) ──────────────────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());
    println!("  Workload: 50-100 операций на сущность (физика + AI + рендеринг + логика)");

    // Система с тяжёлыми вычислениями (имитация AAA игры)
    struct HeavyPhysicsSys;
    impl ParSystem for HeavyPhysicsSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Velocity>, Write<Position>), _>(|(v, p)| {
                // Тяжёлые вычисления: физика с коллизиями, трением, гравитацией
                // ~20-30 операций на сущность
                let dt = 0.016;
                
                // Движение
                p.x += v.x * dt;
                p.y += v.y * dt;
                p.z += v.z * dt;
                
                // Трение (сложная модель)
                let speed_sq = v.x * v.x + v.y * v.y + v.z * v.z;
                if speed_sq > 0.001 {
                    let speed = speed_sq.sqrt();
                    let friction = 0.99 - 0.01 * speed;
                    v.x *= friction;
                    v.y *= friction;
                    v.z *= friction;
                }
                
                // Гравитация с сопротивлением воздуха
                v.y -= 9.8 * dt;
                v.y *= 0.999;
                
                // Ограничение скорости
                let max_speed = 100.0;
                if speed_sq > max_speed * max_speed {
                    let scale = max_speed / speed_sq.sqrt();
                    v.x *= scale;
                    v.y *= scale;
                    v.z *= scale;
                }
                
                // Простая коллизия с землёй
                if p.y < 0.0 {
                    p.y = 0.0;
                    v.y = -v.y * 0.8; // Отскок
                }
                
                // Дополнительные вычисления для нагрузки
                let distance = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                let angle = (p.y / distance.max(1.0)).asin();
                let _ = angle; // Используем чтобы компилятор не оптимизировал
            });
        }
    }

    struct HeavyAISys;
    impl ParSystem for HeavyAISys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Position>().write::<Velocity>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Position>, Write<Velocity>), _>(|(p, v)| {
                // Тяжёлый AI: поиск пути, принятие решений
                // ~15-20 операций на сущность
                
                // Искусственный интеллект: движение к цели
                let target_x = 1000.0;
                let target_y = 500.0;
                let target_z = 0.0;
                
                let dx = target_x - p.x;
                let dy = target_y - p.y;
                let dz = target_z - p.z;
                
                let distance = (dx * dx + dy * dy + dz * dz).sqrt();
                if distance > 1.0 {
                    let speed = 10.0;
                    v.x += (dx / distance) * speed * 0.016;
                    v.y += (dy / distance) * speed * 0.016;
                    v.z += (dz / distance) * speed * 0.016;
                }
                
                // Избегание препятствий
                let obstacle_distance = (p.x * p.x + p.z * p.z).sqrt();
                if obstacle_distance < 50.0 {
                    let avoid_strength = 5.0;
                    v.x -= p.x * avoid_strength * 0.016;
                    v.z -= p.z * avoid_strength * 0.016;
                }
                
                // Ограничение ускорения
                let accel_sq = v.x * v.x + v.y * v.y + v.z * v.z;
                let max_accel = 20.0;
                if accel_sq > max_accel * max_accel {
                    let scale = max_accel / accel_sq.sqrt();
                    v.x *= scale;
                    v.y *= scale;
                    v.z *= scale;
                }
                
                // Дополнительные вычисления
                let _decision_value = (p.x * 0.1).sin() + (p.y * 0.05).cos();
            });
        }
    }

    struct HeavyRenderSys;
    impl ParSystem for HeavyRenderSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Position>().read::<Velocity>().write::<Health>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Position>, Read<Velocity>, Write<Health>), _>(|(p, v, h)| {
                // Тяжёлые вычисления для рендеринга: LOD, culling, подготовка данных
                // ~10-15 операций на сущность
                
                // Вычисление расстояния до камеры
                let camera_x = 0.0;
                let camera_y = 0.0;
                let camera_z = -100.0;
                
                let dx = p.x - camera_x;
                let dy = p.y - camera_y;
                let dz = p.z - camera_z;
                let distance_to_camera = (dx * dx + dy * dy + dz * dz).sqrt();
                
                // LOD (Level of Detail) на основе расстояния
                let lod_level = if distance_to_camera < 100.0 {
                    3 // Высокий детализация
                } else if distance_to_camera < 500.0 {
                    2 // Средняя детализация
                } else {
                    1 // Низкая детализация
                };
                
                // Frustum culling (упрощённый)
                let in_frustum = distance_to_camera < 1000.0 && p.y > -100.0;
                
                // Подготовка данных для рендеринга
                if in_frustum {
                    // Вычисление нормали для освещения
                    let speed = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                    let normal_x = if speed > 0.0 { v.x / speed } else { 0.0 };
                    let normal_y = if speed > 0.0 { v.y / speed } else { 1.0 };
                    let normal_z = if speed > 0.0 { v.z / speed } else { 0.0 };
                    
                    // Освещение (заглушка)
                    let light_intensity = (normal_y * 0.5 + 0.5).max(0.0).min(1.0);
                    
                    // Влияние на здоровье (для демонстрации)
                    if lod_level == 3 {
                        // Высокая детализация → больше вычислений → "стоимость" здоровья
                        h.current = (h.current - 0.001).max(0.0);
                    }
                    
                    let _render_data = (lod_level, light_intensity, distance_to_camera);
                }
            });
        }
    }

    // ── Тестирование с разным количеством систем ───────────────
    
    // 1 система тяжёлой нагрузки
    bench(&format!("1 system HEAVY workload ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_physics", HeavyPhysicsSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    // 2 системы тяжёлой нагрузки (параллельные)
    bench(&format!("2 systems HEAVY PARALLEL ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_physics", HeavyPhysicsSys);
        sched.add_par_system("heavy_ai", HeavyAISys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    // 3 системы тяжёлой нагрузки (параллельные)
    bench(&format!("3 systems HEAVY PARALLEL ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_physics", HeavyPhysicsSys);
        sched.add_par_system("heavy_ai", HeavyAISys);
        sched.add_par_system("heavy_render", HeavyRenderSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    // Mixed pipeline с тяжёлой нагрузкой
    bench(&format!("Mixed pipeline HEAVY ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("physics1", HeavyPhysicsSys);
        sched.add_par_system("ai1", HeavyAISys);
        sched.add_system("barrier", |_world: &mut World| {
            // Structural changes
        });
        sched.add_par_system("physics2", HeavyPhysicsSys);
        sched.add_par_system("render", HeavyRenderSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    println!("  Note: Тяжёлая нагрузка = 50-100 операций на сущность");
    println!("        Ожидаемый speedup на 12 ядрах: 4-8x для 3+ систем");
}

// ── Инкрементальный граф (ленивый топосорт) ─────────────────────

#[cfg(feature = "parallel")]
fn bench_incremental_graph(n: usize) {
    println!("\n── Incremental Graph (Lazy Toposort) ──────────────────────────────────────────────");
    println!("  Тестирование ленивого обновления графа зависимостей");
    println!("  Добавление систем без полного пересчёта графа");

    struct SimpleSys;
    impl ParSystem for SimpleSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Position>, _>(|p| {
                p.x += 1.0;
            });
        }
    }

    // Тест 1: Добавление систем по одной и измерение времени компиляции
    println!("  --- Добавление систем по одной ---");
    
    bench(&format!("compile() после 1 системы"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("sys1", SimpleSys);
        sched.compile().unwrap();
        1
    });

    bench(&format!("compile() после 5 систем"), || {
        let mut sched = Scheduler::new();
        for i in 0..5 {
            sched.add_par_system(format!("sys{}", i), SimpleSys);
        }
        sched.compile().unwrap();
        1
    });

    bench(&format!("compile() после 10 систем"), || {
        let mut sched = Scheduler::new();
        for i in 0..10 {
            sched.add_par_system(format!("sys{}", i), SimpleSys);
        }
        sched.compile().unwrap();
        1
    });

    bench(&format!("compile() после 20 систем"), || {
        let mut sched = Scheduler::new();
        for i in 0..20 {
            sched.add_par_system(format!("sys{}", i), SimpleSys);
        }
        sched.compile().unwrap();
        1
    });

    // Тест 2: Инкрементальное добавление систем
    println!("  --- Инкрементальное добавление ---");
    
    bench(&format!("инкрементальное: 1→5→10→20 систем"), || {
        let mut sched = Scheduler::new();
        
        // Добавляем 1 систему и компилируем
        sched.add_par_system("sys0", SimpleSys);
        sched.compile().unwrap();
        
        // Добавляем ещё 4 системы (всего 5)
        for i in 1..5 {
            sched.add_par_system(format!("sys{}", i), SimpleSys);
        }
        sched.compile().unwrap();
        
        // Добавляем ещё 5 систем (всего 10)
        for i in 5..10 {
            sched.add_par_system(format!("sys{}", i), SimpleSys);
        }
        sched.compile().unwrap();
        
        // Добавляем ещё 10 систем (всего 20)
        for i in 10..20 {
            sched.add_par_system(format!("sys{}", i), SimpleSys);
        }
        sched.compile().unwrap();
        
        1
    });

    // Тест 3: Добавление зависимостей
    println!("  --- Добавление зависимостей ---");
    
    bench(&format!("добавление 10 зависимостей"), || {
        let mut sched = Scheduler::new();
        
        // Создаём 5 систем
        let mut system_ids = Vec::new();
        for i in 0..5 {
            let id = sched.add_par_system(format!("sys{}", i), SimpleSys);
            system_ids.push(id);
        }
        
        // Добавляем зависимости: каждая система зависит от предыдущей
        for i in 1..5 {
            sched.add_dependency(system_ids[i], system_ids[i-1]);
        }
        
        sched.compile().unwrap();
        1
    });

    // Тест 4: Сравнение полной перестройки vs инкрементального обновления
    println!("  --- Сравнение полной vs инкрементальной компиляции ---");
    
    // Полная перестройка каждый раз
    bench(&format!("полная перестройка графа 10 раз"), || {
        for _ in 0..10 {
            let mut sched = Scheduler::new();
            for i in 0..10 {
                sched.add_par_system(format!("sys{}", i), SimpleSys);
            }
            sched.compile().unwrap();
        }
        1
    });

    // Инкрементальное обновление
    bench(&format!("инкрементальное обновление 10 раз"), || {
        let mut sched = Scheduler::new();
        
        // Первая компиляция
        for i in 0..10 {
            sched.add_par_system(format!("sys{}", i), SimpleSys);
        }
        sched.compile().unwrap();
        
        // 9 раз добавляем по одной системе и компилируем
        for batch in 0..9 {
            for i in 0..10 {
                sched.add_par_system(format!("sys_batch{}_{}", batch, i), SimpleSys);
            }
            sched.compile().unwrap();
        }
        
        1
    });

    // Тест 5: Граф с конфликтами
    println!("  --- Граф с Write-конфликтами ---");
    
    struct WritePosSys;
    impl ParSystem for WritePosSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Position>, _>(|p| {
                p.x += 1.0;
            });
        }
    }

    struct WriteVelSys;
    impl ParSystem for WriteVelSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Velocity>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Velocity>, _>(|v| {
                v.x += 1.0;
            });
        }
    }

    bench(&format!("граф с 5 Write-конфликтами"), || {
        let mut sched = Scheduler::new();
        
        // Создаём 5 систем пишущих в Position (конфликты)
        for i in 0..5 {
            sched.add_par_system(format!("pos_sys{}", i), WritePosSys);
        }
        
        // Создаём 5 систем пишущих в Velocity (нет конфликтов с Position)
        for i in 0..5 {
            sched.add_par_system(format!("vel_sys{}", i), WriteVelSys);
        }
        
        sched.compile().unwrap();
        
        // Проверяем что системы с конфликтами в разных Stage
        let stages = sched.stages().unwrap();
        assert!(stages.len() >= 2, "Write-конфликты должны создавать разные Stage");
        
        1
    });

    println!("  Note: Инкрементальный граф хранит зависимости между compile()");
    println!("        Добавление систем обновляет только новые узлы/рёбра");
    println!("        Полный пересчёт только при graph_dirty = true");
}

// ── Специализированные Query с WorldQuerySystemAccess ──────────

#[cfg(feature = "parallel")]
fn bench_specialized_queries(n: usize) {
    println!("\n── Specialized Queries (WorldQuerySystemAccess) ──────────────────────────────────");
    println!("  Тестирование производительности специализированных Query");
    println!("  Сравнение AutoSystem vs ParSystem vs специализированных Query");

    // Создаём мир с разными архетипами
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();
    
    // Создаём 100k сущностей в 3 разных архетипах
    let total = n * 100;
    let third = total / 3;
    
    // Архетип 1: Position + Velocity + Health
    world.spawn_many_silent(third, |i| {
        let f = i as f32;
        (
            Position { x: f, y: f * 0.5, z: 0.0 },
            Velocity { x: 1.0, y: 0.5, z: 0.0 },
            Health { current: 100.0, max: 100.0 },
        )
    });
    
    // Архетип 2: Position + Velocity + Mass
    world.spawn_many_silent(third, |i| {
        let f = i as f32;
        (
            Position { x: f + 1000.0, y: f * 0.3, z: 0.0 },
            Velocity { x: 0.5, y: 1.0, z: 0.0 },
            Mass(1.0 + (i % 10) as f32 * 0.1),
        )
    });
    
    // Архетип 3: Position + Velocity + Player
    world.spawn_many_silent(third, |i| {
        let f = i as f32;
        (
            Position { x: f * 2.0, y: f * 0.7, z: 0.0 },
            Velocity { x: 0.3, y: 0.8, z: 0.0 },
            Player,
        )
    });

    // Тест 1: Обычный AutoSystem
    struct AutoMovementSys;
    impl AutoSystem for AutoMovementSys {
        type Query = (Read<Velocity>, Write<Position>);
        
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Self::Query, _>(|(v, p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
            });
        }
    }

    // Тест 2: ParSystem с явным AccessDescriptor
    struct ParMovementSys;
    impl ParSystem for ParMovementSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Velocity>, Write<Position>), _>(|(v, p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
            });
        }
    }

    // Тест 3: Специализированный Query с фильтрацией
    struct SpecializedQuerySys;
    impl ParSystem for SpecializedQuerySys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>().read::<Health>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            // Специализированный Query: только сущности с Health > 50
            ctx.for_each_component::<(Read<Velocity>, Write<Position>, Read<Health>), _>(|(v, p, h)| {
                if h.current > 50.0 {
                    p.x += v.x * 0.016 * 2.0; // Быстрее если здоровье высокое
                    p.y += v.y * 0.016 * 2.0;
                    p.z += v.z * 0.016 * 2.0;
                } else {
                    p.x += v.x * 0.016 * 0.5; // Медленнее если здоровье низкое
                    p.y += v.y * 0.016 * 0.5;
                    p.z += v.z * 0.016 * 0.5;
                }
            });
        }
    }

    // Тест 4: Query с With<Player> фильтром
    struct PlayerOnlySys;
    impl ParSystem for PlayerOnlySys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>().read::<Player>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            // Только игроки
            ctx.for_each_component::<(Read<Velocity>, Write<Position>, Read<Player>), _>(|(v, p, _)| {
                p.x += v.x * 0.016 * 1.5; // Игроки двигаются быстрее
                p.y += v.y * 0.016 * 1.5;
                p.z += v.z * 0.016 * 1.5;
            });
        }
    }

    // Тест 5: Сложный Query с ветвлением по Health (используем архетип 1: Position + Velocity + Health)
    struct ComplexQuerySys;
    impl ParSystem for ComplexQuerySys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new()
                .read::<Velocity>()
                .write::<Position>()
                .read::<Health>()  // Только архетип 1 имеет Health
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            // Сложный Query: сущности с разным уровнем здоровья (только архетип 1)
            ctx.for_each_component::<(Read<Velocity>, Write<Position>, Read<Health>), _>(|(v, p, h)| {
                if h.current > 70.0 {
                    // Высокое здоровье: быстрее
                    p.x += v.x * 0.016 * 1.2;
                    p.y += v.y * 0.016 * 1.2;
                    p.z += v.z * 0.016 * 1.2;
                } else if h.current > 40.0 {
                    // Среднее здоровье: нормальная скорость
                    p.x += v.x * 0.016;
                    p.y += v.y * 0.016;
                    p.z += v.z * 0.016;
                } else {
                    // Низкое здоровье: медленнее
                    p.x += v.x * 0.016 * 0.7;
                    p.y += v.y * 0.016 * 0.7;
                    p.z += v.z * 0.016 * 0.7;
                }
            });
        }
    }

    println!("  --- Сравнение разных типов Query ---");
    
    let total_entities = n * 100;
    
    // AutoSystem
    bench(&format!("AutoSystem (базовый) ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_auto_system("auto", AutoMovementSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    // ParSystem
    bench(&format!("ParSystem (явный access) ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("par", ParMovementSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    // Специализированный Query с Health фильтром
    bench(&format!("Специализированный Query (Health filter) ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("specialized", SpecializedQuerySys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    // Query с With<Player> фильтром
    bench(&format!("Query с With<Player> фильтром ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("player_only", PlayerOnlySys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    // Сложный Query с ветвлением по Health
    bench(&format!("Сложный Query (Health ветвление) ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("complex", ComplexQuerySys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    println!("  --- Параллельное выполнение специализированных Query ---");
    
    // Параллельное выполнение нескольких специализированных систем
    bench(&format!("3 специализированные системы PARALLEL ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("auto", ParMovementSys);
        sched.add_par_system("specialized", SpecializedQuerySys);
        sched.add_par_system("player_only", PlayerOnlySys);
        sched.compile().unwrap();
        sched.run(&mut world);
        total_entities as u64
    });

    // Сравнение sequential vs parallel для сложного Query
    bench(&format!("Сложный Query SEQUENTIAL ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("complex", ComplexQuerySys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    bench(&format!("Сложный Query PARALLEL ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("complex", ComplexQuerySys);
        sched.compile().unwrap();
        sched.run(&mut world);
        total_entities as u64
    });

    println!("  --- Измерение overhead компиляции специализированных Query ---");
    
    bench("compile() 5 специализированных систем", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("sys1", ParMovementSys);
        sched.add_par_system("sys2", SpecializedQuerySys);
        sched.add_par_system("sys3", PlayerOnlySys);
        sched.add_par_system("sys4", ComplexQuerySys);
        sched.add_par_system("sys5", ParMovementSys);
        sched.compile().unwrap();
        1
    });

    bench("compile() 10 смешанных систем (Auto + Par + специализированные)", || {
        let mut sched = Scheduler::new();
        for i in 0..3 {
            sched.add_auto_system(format!("auto_{}", i), AutoMovementSys);
        }
        for i in 0..3 {
            sched.add_par_system(format!("par_{}", i), ParMovementSys);
        }
        for i in 0..2 {
            sched.add_par_system(format!("special_{}", i), SpecializedQuerySys);
        }
        sched.add_par_system("player", PlayerOnlySys);
        sched.add_par_system("complex", ComplexQuerySys);
        sched.compile().unwrap();
        1
    });

    println!("  Note: Специализированные Query используют WorldQuerySystemAccess");
    println!("        для статической проверки доступа к компонентам");
    println!("        With<T> фильтры исключают сущности без компонента T");
    println!("        Сложные Query с несколькими фильтрами могут быть дороже");
}

// ── Intra-system parallelism (par_for_each_component) ──────────

#[cfg(feature = "parallel")]
fn bench_intra_system_parallel(n: usize) {
    println!("\n── Intra-system Parallelism (par_for_each_component) ──────────────────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());

    // Создаём мир с несколькими архетипами для демонстрации параллелизма
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();
    
    // Создаём 100k сущностей в 4 разных архетипах
    let total = n * 100;
    let quarter = total / 4;
    
    // Архетип 1: Position + Velocity + Health
    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (
            Position { x: f, y: f * 0.5, z: 0.0 },
            Velocity { x: 1.0, y: 0.5, z: 0.0 },
            Health { current: 100.0, max: 100.0 },
        )
    });
    
    // Архетип 2: Position + Velocity + Mass
    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (
            Position { x: f + 1000.0, y: f * 0.3, z: 0.0 },
            Velocity { x: 0.5, y: 1.0, z: 0.0 },
            Mass(1.0 + (i % 10) as f32 * 0.1),
        )
    });
    
    // Архетип 3: Position + Velocity + Player
    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (
            Position { x: f * 2.0, y: f * 0.7, z: 0.0 },
            Velocity { x: 0.3, y: 0.8, z: 0.0 },
            Player,
        )
    });
    
    // Архетип 4: Position + Velocity + Enemy
    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (
            Position { x: f * 1.5, y: f * 0.2, z: 0.0 },
            Velocity { x: 0.8, y: 0.2, z: 0.0 },
            Enemy,
        )
    });

    // Система с обычным for_each_component (последовательная)
    struct SequentialSys;
    impl ParSystem for SequentialSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Velocity>, Write<Position>), _>(|(v, p)| {
                // Реалистичные вычисления: движение с небольшими дополнительными операциями
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                
                // Небольшое дополнительное вычисление для демонстрации
                let len = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                if len > 1000.0 {
                    p.x /= len * 0.001;
                    p.y /= len * 0.001;
                    p.z /= len * 0.001;
                }
            });
        }
    }

    // Система с par_for_each_component (параллельная внутри системы)
    struct ParallelSys;
    impl ParSystem for ParallelSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.par_for_each_component::<(Read<Velocity>, Write<Position>), _>(|(v, p)| {
                // Те же реалистичные вычисления
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                
                let len = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                if len > 1000.0 {
                    p.x /= len * 0.001;
                    p.y /= len * 0.001;
                    p.z /= len * 0.001;
                }
            });
        }
    }

    // Бенчмарк последовательной версии
    let total_entities = n * 100;
    bench(&format!("Sequential for_each_component ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("seq", SequentialSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    // Бенчмарк параллельной версии
    bench(&format!("Parallel par_for_each_component ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("par", ParallelSys);
        sched.compile().unwrap();
        sched.run(&mut world); // run() использует параллельное выполнение
        total_entities as u64
    });

    // AutoSystem с par_for_each_component (идентичные вычисления)
    struct AutoParallelSys;
    impl AutoSystem for AutoParallelSys {
        type Query = (Read<Velocity>, Write<Position>);
        
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.par_for_each_component::<Self::Query, _>(|(v, p)| {
                // Те же вычисления, что и в обычном тесте
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                
                let len = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                if len > 1000.0 {
                    p.x /= len * 0.001;
                    p.y /= len * 0.001;
                    p.z /= len * 0.001;
                }
            });
        }
    }

    bench(&format!("AutoSystem + par_for_each_component ({}k)", total_entities), || {
        let mut sched = Scheduler::new();
        sched.add_auto_system("auto_par", AutoParallelSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        total_entities as u64
    });

    println!("  Note: par_for_each_component распараллеливает итерацию по архетипам");
    println!("        внутри одной системы, когда много сущностей (>10k)");
}

// ── Medium workload parallel scheduler (реальная игровая нагрузка) ──────────

#[cfg(feature = "parallel")]
fn bench_medium_workload_parallel(n: usize) {
    println!("\n── Medium Workload Parallel Scheduler (реальная игровая нагрузка) ────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());
    println!("  Workload: 15-20 операций на сущность (физика + AI + логика)");

    // Система с средней нагрузкой (как в реальных играх)
    struct PhysicsMediumSys;
    impl ParSystem for PhysicsMediumSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Velocity>, Write<Position>), _>(|(v, p)| {
                // Реалистичная физика: 8 операций
                let dt = 0.016;
                
                // Гравитация + трение
                v.y -= 9.8 * dt;
                v.x *= 0.99;
                v.y *= 0.99;
                v.z *= 0.99;
                
                // Движение
                p.x += v.x * dt;
                p.y += v.y * dt;
                p.z += v.z * dt;
                
                // Коллизия с полом
                if p.y < 0.0 {
                    p.y = 0.0;
                    v.y = -v.y * 0.8;
                }
                
                // Нормализация (если слишком быстро)
                let speed_sq = v.x * v.x + v.y * v.y + v.z * v.z;
                if speed_sq > 100.0 {
                    let inv_speed = 10.0 / speed_sq.sqrt();
                    v.x *= inv_speed;
                    v.y *= inv_speed;
                    v.z *= inv_speed;
                }
            });
        }
    }

    struct AIMediumSys;
    impl ParSystem for AIMediumSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Position>().write::<Velocity>().write::<Health>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Position>, Write<Velocity>, Write<Health>), _>(|(p, v, hp)| {
                // Реалистичный AI: 7 операций
                let target_x = 100.0;
                let target_y = 50.0;
                
                // Вектор к цели
                let dx = target_x - p.x;
                let dy = target_y - p.y;
                let dist = (dx * dx + dy * dy).sqrt();
                
                if dist > 1.0 {
                    // Движение к цели
                    v.x += dx / dist * 0.1;
                    v.y += dy / dist * 0.1;
                    
                    // Ограничение скорости
                    let speed_sq = v.x * v.x + v.y * v.y;
                    if speed_sq > 25.0 {
                        let inv_speed = 5.0 / speed_sq.sqrt();
                        v.x *= inv_speed;
                        v.y *= inv_speed;
                    }
                }
                
                // Регенерация здоровья
                hp.current = (hp.current + 0.05).min(hp.max);
                
                // Если здоровье низкое - убегаем
                if hp.current < 30.0 {
                    v.x *= 1.2;
                    v.y *= 1.2;
                }
            });
        }
    }

    struct LogicMediumSys;
    impl ParSystem for LogicMediumSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Temperature>().write::<Mana>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Temperature>, Write<Mana>), _>(|(temp, mana)| {
                // Реалистичная логика: 6 операций
                // Температура стремится к 20°C
                temp.0 += (20.0 - temp.0) * 0.01;
                
                // Если температура высокая, мана тратится быстрее
                let mana_cost = if temp.0 > 30.0 { 0.02 } else { 0.01 };
                mana.current = (mana.current - mana_cost).max(0.0);
                
                // Регенерация маны если температура нормальная
                if temp.0 < 25.0 {
                    mana.current = (mana.current + 0.03).min(mana.max);
                }
                
                // Эффект от маны на температуру
                if mana.current < 20.0 {
                    temp.0 += 0.5; // Низкая мана → перегрев
                }
            });
        }
    }

    // Создаём мир для теста средней нагрузки
    let mut world = make_world_4comp(n * 1000);
    
    // Добавляем недостающие компоненты
    world.register_component::<Temperature>();
    world.register_component::<Mana>();
    
    // Обновляем сущности чтобы у всех были все компоненты
    let mut entities = Vec::new();
    world.query_typed::<Read<Position>>().for_each(|e, _| {
        entities.push(e);
    });
    for &e in &entities {
        world.insert(e, Temperature(20.0));
        world.insert(e, Mana { current: 50.0, max: 100.0 });
    }

    // Тестируем scaling с разным количеством систем
    println!("  --- Scaling с количеством систем ---");
    
    // 1 система
    bench(&format!("1 system MEDIUM workload ({n}k entities)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsMediumSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    // 2 системы
    bench(&format!("2 systems MEDIUM workload ({n}k entities)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsMediumSys);
        sched.add_par_system("ai", AIMediumSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("2 systems MEDIUM PARALLEL ({n}k entities)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsMediumSys);
        sched.add_par_system("ai", AIMediumSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    // 3 системы
    bench(&format!("3 systems MEDIUM workload ({n}k entities)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsMediumSys);
        sched.add_par_system("ai", AIMediumSys);
        sched.add_par_system("logic", LogicMediumSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("3 systems MEDIUM PARALLEL ({n}k entities)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsMediumSys);
        sched.add_par_system("ai", AIMediumSys);
        sched.add_par_system("logic", LogicMediumSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    println!("  --- Mixed pipeline (реальный сценарий) ---");
    
    // Mixed pipeline: 2 par → seq → 2 par
    bench(&format!("Mixed pipeline MEDIUM SEQ ({n}k entities)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics1", PhysicsMediumSys);
        sched.add_par_system("ai1", AIMediumSys);
        sched.add_system("barrier", |_world: &mut World| {
            // Команды, события и т.д.
        });
        sched.add_par_system("physics2", PhysicsMediumSys);
        sched.add_par_system("logic", LogicMediumSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("Mixed pipeline MEDIUM PAR ({n}k entities)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics1", PhysicsMediumSys);
        sched.add_par_system("ai1", AIMediumSys);
        sched.add_system("barrier", |_world: &mut World| {});
        sched.add_par_system("physics2", PhysicsMediumSys);
        sched.add_par_system("logic", LogicMediumSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    println!("  Note: Средняя нагрузка = 15-20 операций на сущность");
    println!("        Ожидаемый speedup на 12 ядрах: 3-6x для 3+ систем");
}

#[cfg(not(feature = "parallel"))]
fn bench_medium_workload_parallel(_n: usize) {
    println!("\n── Medium Workload Parallel Scheduler ────────────────────────────────────────────");
    println!("  Feature 'parallel' not enabled - skipping medium workload benchmarks");
}

// Fallback для non-parallel builds
#[cfg(not(feature = "parallel"))]
fn bench_intra_system_parallel(_n: usize) {
    println!("\n── Intra-system Parallelism (par_for_each_component) ──────────────────────────────");
    println!("  Feature 'parallel' not enabled - skipping intra-system parallelism benchmarks");
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

    #[cfg(feature = "parallel")]
    bench_intra_system_parallel(N);

    #[cfg(feature = "parallel")]
    bench_medium_workload_parallel(N);

    #[cfg(feature = "parallel")]
    bench_incremental_graph(N);

    #[cfg(feature = "parallel")]
    bench_specialized_queries(N);

    println!("\n── Summary ─────────────────────────────────────────────────────────────────────");
    let (mut world, _) = make_world(N * 1000);
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.add_event::<DamageEvent>();
    println!("  {}k entities, {} archetypes", N, world.archetype_count());
    println!("  resources:              {}", world.resource_count());
    println!("  CachedQuery<Pos> len:   {}", world.query_typed::<Read<Position>>().len());
    println!("  current_tick:           {:?}", world.current_tick());
}
