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
#[derive(Clone, Copy)] struct Temperature(f32);
#[derive(Clone, Copy)] struct Mana { current: f32, max: f32 }

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

    bench(&format!("spawn_many_silent 1comp ({n}k)"), || {
        let mut world = World::new();
        world.register_component::<Position>();
        world.spawn_many_silent(n * 1000, |i| {
            (Position { x: i as f32, y: 0.0, z: 0.0 },)
        });
        (n * 1000) as u64
    });

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

    struct AutoMoveSys;
    impl AutoSystem for AutoMoveSys {
        type Query = (Read<Velocity>, Write<Position>);
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Self::Query, _>(|(v, p)| {
                p.x += v.x; p.y += v.y;
            });
        }
    }

    bench(&format!("1 ParSystem: movement ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("move", MoveSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

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

    bench("compile() 5 systems (2 par + 3 seq)", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("m1", MoveSys);
        sched.add_par_system("m2", HpSys);
        for i in 0..3 { sched.add_system(format!("seq_{i}"), |_| {}); }
        sched.compile().unwrap();
        1
    });

    bench("compile() 10 mixed systems", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("m1", MoveSys);
        sched.add_par_system("m2", HpSys);
        for i in 0..4 { sched.add_system(format!("seq_{i}"), |_| {}); }
        sched.add_par_system("m3", MoveSys);
        sched.add_par_system("m4", HpSys);
        for i in 0..2 { sched.add_system(format!("seq2_{i}"), |_| {}); }
        sched.compile().unwrap();
        1
    });

    bench("compile() 20 systems (complex graph)", || {
        let mut sched = Scheduler::new();
        for i in 0..4 { sched.add_par_system(format!("par_a{i}"), MoveSys); }
        for i in 0..2 { sched.add_system(format!("seq_a{i}"), |_| {}); }
        for i in 0..4 { sched.add_par_system(format!("par_b{i}"), HpSys); }
        for i in 0..4 { sched.add_system(format!("seq_b{i}"), |_| {}); }
        for i in 0..6 { sched.add_par_system(format!("par_c{i}"), MoveSys); }
        sched.compile().unwrap();
        1
    });

    bench("compile() 10 AutoSystems", || {
        let mut sched = Scheduler::new();
        for i in 0..10 { sched.add_auto_system(format!("auto_{i}"), AutoMoveSys); }
        sched.compile().unwrap();
        1
    });

    // Повторный compile() без изменений — проверка идемпотентности.
    // graph_dirty = false → ранний выход, должно быть ~бесплатно.
    bench("compile() repeat (no changes, early exit)", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("m1", MoveSys);
        sched.add_par_system("m2", HpSys);
        sched.add_system("seq", |_| {});
        sched.compile().unwrap(); // первый — строит граф
        sched.compile().unwrap(); // второй — graph_dirty=false, ранний выход
        1
    });

    bench(&format!("1 AutoSystem ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_auto_system("auto_move", AutoMoveSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("ParSystem vs AutoSystem ({n}k)"), || {
        let (mut world, _) = make_world(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("par", MoveSys);
        sched.add_auto_system("auto", AutoMoveSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

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

// ── compile() overhead — полный корректный тест ───────────────
//
// Три сценария:
//   1. Фиксированный N — честное O(N²) без накопления
//   2. Инкрементальный — добавляем по 1 системе, каждый раз compile()
//      Измеряем реальную стоимость "добавить систему и перекомпилировать"
//   3. Идемпотентность и recompile после изменения

fn bench_compile_overhead() {
    println!("\n── Scheduler compile() overhead ────────────────────────────────────────────────");
    println!("  Каждый тест создаёт новый Scheduler если не указано иное.");
    println!("  Сложность rebuild_graph: O(N²) по числу пар систем.");

    struct SimpleSys;
    impl ParSystem for SimpleSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Position>() }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct OtherSys;
    impl ParSystem for OtherSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Velocity>() }
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    // ── 1. Фиксированный N — базовый O(N²) ───────────────────
    println!("\n  --- Фиксированный N (новый Scheduler каждый раз) ---");

    for &n_sys in &[1usize, 5, 10, 20, 50] {
        bench(&format!("compile() {n_sys:>2} par systems (no conflicts)"), || {
            let mut sched = Scheduler::new();
            for i in 0..n_sys {
                if i % 2 == 0 {
                    sched.add_par_system(format!("sys_{i}"), SimpleSys);
                } else {
                    sched.add_par_system(format!("sys_{i}"), OtherSys);
                }
            }
            sched.compile().unwrap();
            1
        });
    }

    println!();

    for &n_sys in &[2usize, 5, 10, 20] {
        bench(
            &format!("compile() {n_sys:>2} par systems (all Write<Pos> — max conflicts)"),
            || {
                let mut sched = Scheduler::new();
                for i in 0..n_sys {
                    sched.add_par_system(format!("sys_{i}"), SimpleSys);
                }
                sched.compile().unwrap();
                debug_assert_eq!(sched.stages().unwrap().len(), n_sys);
                1
            },
        );
    }

    println!();

    bench("compile() 10 sys: 5 par + 5 seq (max barriers)", || {
        let mut sched = Scheduler::new();
        for i in 0..5 { sched.add_par_system(format!("par_{i}"), OtherSys); }
        for i in 0..5 { sched.add_system(format!("seq_{i}"), |_| {}); }
        sched.compile().unwrap();
        1
    });

    // ── 2. Инкрементальный — добавляем 1 систему за раз ──────
    //
    // Реальный сценарий: игра в процессе разработки добавляет
    // системы одну за другой. Каждый add_*_system инвалидирует план,
    // следующий compile() делает полный rebuild_graph для текущего N.
    //
    // Что измеряем: суммарная стоимость N compile() при росте
    // числа систем от 1 до N. Честно показывает что каждый compile()
    // стоит O(k²) для k систем.
    println!("\n  --- Инкрементальный: добавляем по 1 системе, каждый раз compile() ---");
    println!("  Измеряется суммарная стоимость: compile(1) + compile(2) + ... + compile(N)");

    bench("инкрементальный: 1 система  (1 compile)", || {
        let mut sched = Scheduler::new();
        sched.add_par_system("sys_0", OtherSys);
        sched.compile().unwrap(); // compile для N=1
        1
    });

    bench("инкрементальный: 5 систем  (5 compile)", || {
        let mut sched = Scheduler::new();
        for i in 0..5 {
            sched.add_par_system(format!("sys_{i}"), OtherSys);
            sched.compile().unwrap(); // compile для N=1,2,3,4,5
        }
        1
    });

    bench("инкрементальный: 10 систем (10 compile)", || {
        let mut sched = Scheduler::new();
        for i in 0..10 {
            sched.add_par_system(format!("sys_{i}"), OtherSys);
            sched.compile().unwrap(); // compile для N=1..10
        }
        1
    });

    bench("инкрементальный: 20 систем (20 compile)", || {
        let mut sched = Scheduler::new();
        for i in 0..20 {
            sched.add_par_system(format!("sys_{i}"), OtherSys);
            sched.compile().unwrap(); // compile для N=1..20
        }
        1
    });

    // Для сравнения: один compile() для того же N
    println!("\n  --- Сравнение: N инкрементальных compile() vs 1 compile() для N систем ---");

    bench("1 compile() для 10 систем (baseline)", || {
        let mut sched = Scheduler::new();
        for i in 0..10 { sched.add_par_system(format!("sys_{i}"), OtherSys); }
        sched.compile().unwrap();
        1
    });

    bench("10 compile() (инкрементально, N=1..10)", || {
        let mut sched = Scheduler::new();
        for i in 0..10 {
            sched.add_par_system(format!("sys_{i}"), OtherSys);
            sched.compile().unwrap();
        }
        1
    });

    // Ожидаемое соотношение: 10 инкрементальных ≈ 5-6x дороже 1 compile(10)
    // Потому что: sum(k² for k=1..10) ≈ 385, vs 10² = 100 для одного compile

    bench("1 compile() для 20 систем (baseline)", || {
        let mut sched = Scheduler::new();
        for i in 0..20 { sched.add_par_system(format!("sys_{i}"), OtherSys); }
        sched.compile().unwrap();
        1
    });

    bench("20 compile() (инкрементально, N=1..20)", || {
        let mut sched = Scheduler::new();
        for i in 0..20 {
            sched.add_par_system(format!("sys_{i}"), OtherSys);
            sched.compile().unwrap();
        }
        1
    });

    // ── 3. Идемпотентность и recompile ────────────────────────
    println!("\n  --- Идемпотентность и recompile после изменений ---");

    // Повторный compile() без изменений — graph_dirty=false → ранний выход
    bench("compile() repeat без изменений (graph_dirty=false)", || {
        let mut sched = Scheduler::new();
        for i in 0..10 { sched.add_par_system(format!("sys_{i}"), OtherSys); }
        sched.compile().unwrap(); // первый — строит граф
        sched.compile().unwrap(); // второй — ранний выход, ~бесплатно
        1
    });

    // Добавление 1 системы после compile() → инвалидация → recompile
    bench("add 1 system after compile(10) → recompile(11)", || {
        let mut sched = Scheduler::new();
        for i in 0..10 { sched.add_par_system(format!("sys_{i}"), OtherSys); }
        sched.compile().unwrap();         // compile для 10 систем
        sched.add_par_system("sys_10", OtherSys); // инвалидирует план
        sched.compile().unwrap();         // полный rebuild для 11 систем
        1
    });

    // Добавление зависимости после compile() → recompile
    bench("add_dependency after compile() → recompile", || {
        let mut sched = Scheduler::new();
        for i in 0..5 { sched.add_par_system(format!("sys_{i}"), OtherSys); }
        sched.compile().unwrap();
        let a_id = apex_scheduler::SystemId(0);
        let b_id = apex_scheduler::SystemId(1);
        sched.add_dependency(b_id, a_id); // инвалидирует план
        sched.compile().unwrap();         // rebuild с явной зависимостью
        1
    });
}

// ── Parallel Scheduler ────────────────────────────────────────

#[cfg(feature = "parallel")]
fn bench_parallel_scheduler(n: usize) {
    println!("\n── Parallel Scheduler (rayon) ──────────────────────────────────────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());
    println!("  Workload: лёгкие операции (memory-bandwidth bound)");
    println!("  Ожидание: SEQ ≈ PAR (шина памяти — узкое место, не CPU)");

    struct PhysSys;
    impl ParSystem for PhysSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Velocity>, Write<Position>), _>(|(v, p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                v.x *= 0.99;
                v.y -= 9.8 * 0.016;
            });
        }
    }

    struct HpSys;
    impl ParSystem for HpSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Health>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Health>, _>(|hp| {
                hp.current = (hp.current + 0.1).min(hp.max).max(0.0);
            });
        }
    }

    struct TempSys;
    impl ParSystem for TempSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Temperature>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Temperature>, _>(|t| {
                t.0 += (20.0 - t.0) * 0.001;
            });
        }
    }

    struct ManaSys;
    impl ParSystem for ManaSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Mana>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Mana>, _>(|m| {
                m.current = (m.current + 0.2).min(m.max);
            });
        }
    }

    // ── Лёгкая нагрузка (memory-bound) ────────────────────────

    println!("\n  --- Лёгкая нагрузка (memory-bound, SEQ ≈ PAR ожидается) ---");

    bench(&format!("2 systems SEQ  ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("2 systems PAR  ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("4 systems SEQ  ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_par_system("temp", TempSys);
        sched.add_par_system("mana", ManaSys);
        sched.compile().unwrap();
        assert_eq!(sched.stages().unwrap().len(), 1);
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("4 systems PAR  ({n}k entities)"), || {
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

    // ── Тяжёлая нагрузка (CPU-bound) ──────────────────────────
    //
    // Трансцендентные функции (atan2, tanh, sqrt, powi) — реально
    // CPU-bound, не vectorizable компилятором. Speedup должен быть виден.

    println!("\n  --- Тяжёлая нагрузка (CPU-bound via transcendental, speedup ожидается) ---");

    struct HeavyPhysSys;
    impl ParSystem for HeavyPhysSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Velocity>, Write<Position>), _>(|(v, p)| {
                let dt    = 0.016f32;
                let speed = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                let angle = speed.atan2(1.0);
                let drag  = angle.cos() * 0.99;
                v.x = v.x * drag + angle.sin() * 0.001;
                v.y = v.y * drag - 9.8 * dt;
                v.z = v.z * drag;
                p.x += v.x * dt;
                p.y += v.y * dt;
                p.z += v.z * dt;
                if p.y < 0.0 { p.y = 0.0; v.y = v.y.abs() * 0.8; }
            });
        }
    }

    struct HeavyTempSys;
    impl ParSystem for HeavyTempSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Temperature>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Temperature>, _>(|t| {
                let ambient = 20.0f32;
                let diff    = t.0 - ambient;
                let rate    = (diff * 0.1).tanh() * 0.05;
                t.0        -= rate;
                t.0         = t.0.clamp(
                    ambient - diff.abs().sqrt(),
                    ambient + diff.abs().sqrt(),
                );
            });
        }
    }

    struct HeavyManaSys;
    impl ParSystem for HeavyManaSys {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Mana>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Mana>, _>(|m| {
                let ratio = m.current / m.max;
                let regen = (1.0 - ratio).sqrt() * 0.5;
                m.current = (m.current + regen).min(m.max);
                if ratio > 0.9 {
                    m.current *= 1.0 - (ratio - 0.9).powi(2) * 0.01;
                }
            });
        }
    }

    bench(&format!("1 system HEAVY SEQ ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_phys", HeavyPhysSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("2 systems HEAVY SEQ ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_phys", HeavyPhysSys);
        sched.add_par_system("heavy_temp", HeavyTempSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("2 systems HEAVY PAR ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_phys", HeavyPhysSys);
        sched.add_par_system("heavy_temp", HeavyTempSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("3 systems HEAVY SEQ ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_phys", HeavyPhysSys);
        sched.add_par_system("heavy_temp", HeavyTempSys);
        sched.add_par_system("heavy_mana", HeavyManaSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("3 systems HEAVY PAR ({n}k entities)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("heavy_phys", HeavyPhysSys);
        sched.add_par_system("heavy_temp", HeavyTempSys);
        sched.add_par_system("heavy_mana", HeavyManaSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    println!("\n  --- Speedup summary (PAR / SEQ) ---");
    println!("  Note: лёгкая нагрузка → speedup ~1.0x (memory-bound, ожидаемо)");
    println!("  Note: тяжёлая нагрузка → speedup > 1.5x на 12 ядрах (CPU-bound)");

    bench(&format!("mixed pipeline SEQ ({n}k)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_system("barrier", |_: &mut World| {});
        sched.add_par_system("phys2", PhysSys);
        sched.add_par_system("hp2",   HpSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("mixed pipeline PAR ({n}k)"), || {
        let mut world = make_world_4comp(n * 1000);
        let mut sched = Scheduler::new();
        sched.add_par_system("phys", PhysSys);
        sched.add_par_system("hp",   HpSys);
        sched.add_system("barrier", |_: &mut World| {});
        sched.add_par_system("phys2", PhysSys);
        sched.add_par_system("hp2",   HpSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

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

#[cfg(not(feature = "parallel"))]
fn bench_parallel_scheduler(_n: usize) {
    println!("\n── Parallel Scheduler: feature 'parallel' not enabled ──────────────────────────");
}

// ── Intra-system parallelism ───────────────────────────────────

#[cfg(feature = "parallel")]
fn bench_intra_system_parallel(n: usize) {
    println!("\n── Intra-system Parallelism (par_for_each_component) ──────────────────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());
    println!("  Тест: одна система, несколько архетипов → параллелизм по архетипам");

    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();

    let total    = n * 100;
    let quarter  = total / 4;

    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (Position { x: f, y: f * 0.5, z: 0.0 },
         Velocity { x: 1.0, y: 0.5, z: 0.0 },
         Health { current: 100.0, max: 100.0 })
    });
    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (Position { x: f + 1000.0, y: f * 0.3, z: 0.0 },
         Velocity { x: 0.5, y: 1.0, z: 0.0 },
         Mass(1.0 + (i % 10) as f32 * 0.1))
    });
    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (Position { x: f * 2.0, y: f * 0.7, z: 0.0 },
         Velocity { x: 0.3, y: 0.8, z: 0.0 },
         Player)
    });
    world.spawn_many_silent(quarter, |i| {
        let f = i as f32;
        (Position { x: f * 1.5, y: f * 0.2, z: 0.0 },
         Velocity { x: 0.8, y: 0.2, z: 0.0 },
         Enemy)
    });

    let total_entities = n * 100;

    struct SeqSys;
    impl ParSystem for SeqSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Velocity>, Write<Position>), _>(|(v, p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                let len = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                if len > 10000.0 { p.x /= len; p.y /= len; p.z /= len; }
            });
        }
    }

    struct ParSys;
    impl ParSystem for ParSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.par_for_each_component::<(Read<Velocity>, Write<Position>), _>(|(v, p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                let len = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                if len > 10000.0 { p.x /= len; p.y /= len; p.z /= len; }
            });
        }
    }

    struct AutoParSys;
    impl AutoSystem for AutoParSys {
        type Query = (Read<Velocity>, Write<Position>);
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.par_for_each_component::<Self::Query, _>(|(v, p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                p.z += v.z * 0.016;
                let len = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                if len > 10000.0 { p.x /= len; p.y /= len; p.z /= len; }
            });
        }
    }

    bench(&format!("Sequential for_each ({total_entities}k, 4 archetypes)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("seq", SeqSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    bench(&format!("par_for_each SEQ context ({total_entities}k, 4 archetypes)"), || {
        // run_sequential → main thread → par_iter реально параллелит
        let mut sched = Scheduler::new();
        sched.add_par_system("par", ParSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    bench(&format!("par_for_each PAR context ({total_entities}k, 4 archetypes)"), || {
        // run() → rayon::scope → par_for_each тоже параллелит (вложенный scope)
        let mut sched = Scheduler::new();
        sched.add_par_system("par", ParSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        total_entities as u64
    });

    bench(&format!("AutoSystem + par_for_each ({total_entities}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_auto_system("auto_par", AutoParSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        total_entities as u64
    });

    println!("  Note: par_for_each параллелит по архетипам в обоих контекстах");
    println!("        Rayon work-stealing корректен при вложенных scope");
}

#[cfg(not(feature = "parallel"))]
fn bench_intra_system_parallel(_n: usize) {
    println!("\n── Intra-system Parallelism: feature 'parallel' not enabled ────────────────────");
}

// ── Medium workload ────────────────────────────────────────────

#[cfg(feature = "parallel")]
fn bench_medium_workload(n: usize) {
    println!("\n── Medium Workload (реалистичная игровая нагрузка) ─────────────────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());
    println!("  Workload: 15-25 операций/entity, часть CPU-bound (sqrt, ветвление)");

    struct PhysicsSys;
    impl ParSystem for PhysicsSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Velocity>().write::<Position>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Velocity>, Write<Position>), _>(|(v, p)| {
                let dt = 0.016f32;
                v.y -= 9.8 * dt;
                v.x *= 0.99; v.y *= 0.99; v.z *= 0.99;
                p.x += v.x * dt; p.y += v.y * dt; p.z += v.z * dt;
                if p.y < 0.0 { p.y = 0.0; v.y = v.y.abs() * 0.8; }
                let speed_sq = v.x * v.x + v.y * v.y + v.z * v.z;
                if speed_sq > 100.0 {
                    let s = 10.0 / speed_sq.sqrt();
                    v.x *= s; v.y *= s; v.z *= s;
                }
            });
        }
    }

    struct AISys;
    impl ParSystem for AISys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Position>().write::<Velocity>().write::<Health>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Position>, Write<Velocity>, Write<Health>), _>(
                |(p, v, hp)| {
                    let dx   = 100.0 - p.x;
                    let dy   = 50.0  - p.y;
                    let dist = (dx * dx + dy * dy).sqrt().max(1.0);
                    v.x += dx / dist * 0.1;
                    v.y += dy / dist * 0.1;
                    let sq = v.x * v.x + v.y * v.y;
                    if sq > 25.0 { let s = 5.0 / sq.sqrt(); v.x *= s; v.y *= s; }
                    hp.current = (hp.current + 0.05).min(hp.max);
                    if hp.current < 30.0 { v.x *= 1.2; v.y *= 1.2; }
                },
            );
        }
    }

    struct LogicSys;
    impl ParSystem for LogicSys {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Temperature>().write::<Mana>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Write<Temperature>, Write<Mana>), _>(|(temp, mana)| {
                temp.0 += (20.0 - temp.0) * 0.01;
                let cost = if temp.0 > 30.0 { 0.02 } else { 0.01 };
                mana.current = (mana.current - cost).max(0.0);
                if temp.0 < 25.0 { mana.current = (mana.current + 0.03).min(mana.max); }
                if mana.current < 20.0 { temp.0 += 0.5; }
            });
        }
    }

    let mut world = make_world_4comp(n * 1000);
    world.register_component::<Temperature>();
    world.register_component::<Mana>();
    let mut entities = Vec::new();
    world.query_typed::<Read<Position>>().for_each(|e, _| entities.push(e));
    for &e in &entities {
        world.insert(e, Temperature(20.0));
        world.insert(e, Mana { current: 50.0, max: 100.0 });
    }

    bench(&format!("1 system MEDIUM SEQ ({n}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("2 systems MEDIUM SEQ ({n}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsSys);
        sched.add_par_system("ai",      AISys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("2 systems MEDIUM PAR ({n}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsSys);
        sched.add_par_system("ai",      AISys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("3 systems MEDIUM SEQ ({n}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsSys);
        sched.add_par_system("ai",      AISys);
        sched.add_par_system("logic",   LogicSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("3 systems MEDIUM PAR ({n}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics", PhysicsSys);
        sched.add_par_system("ai",      AISys);
        sched.add_par_system("logic",   LogicSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("mixed pipeline MEDIUM SEQ ({n}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics1", PhysicsSys);
        sched.add_par_system("ai1",      AISys);
        sched.add_system("barrier", |_: &mut World| {});
        sched.add_par_system("physics2", PhysicsSys);
        sched.add_par_system("logic",    LogicSys);
        sched.compile().unwrap();
        sched.run_sequential(&mut world);
        (n * 1000) as u64
    });

    bench(&format!("mixed pipeline MEDIUM PAR ({n}k)"), || {
        let mut sched = Scheduler::new();
        sched.add_par_system("physics1", PhysicsSys);
        sched.add_par_system("ai1",      AISys);
        sched.add_system("barrier", |_: &mut World| {});
        sched.add_par_system("physics2", PhysicsSys);
        sched.add_par_system("logic",    LogicSys);
        sched.compile().unwrap();
        sched.run(&mut world);
        (n * 1000) as u64
    });

    println!("  Note: speedup виден при 3+ системах и достаточной вычислительной нагрузке");
}

#[cfg(not(feature = "parallel"))]
fn bench_medium_workload(_n: usize) {}

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

    const N: usize = 1000; // базовый масштаб: 1M entities

    bench_batch_allocator(N);
    bench_has_relation(N);
    bench_scheduler(N);
    bench_compile_overhead();
    bench_resources(N);
    bench_events(N);
    bench_query(N);
    bench_structural(N);

    #[cfg(feature = "parallel")]
    bench_parallel_scheduler(N);

    #[cfg(feature = "parallel")]
    bench_intra_system_parallel(N);

    #[cfg(feature = "parallel")]
    bench_medium_workload(N);

    println!("\n── Summary ─────────────────────────────────────────────────────────────────────");
    let (mut world, _) = make_world(N * 1000);
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.add_event::<DamageEvent>();
    println!("  {}k entities, {} archetypes", N, world.archetype_count());
    println!("  resources:              {}", world.resource_count());
    println!("  CachedQuery<Pos> len:   {}", world.query_typed::<Read<Position>>().len());
    println!("  current_tick:           {:?}", world.current_tick());
}