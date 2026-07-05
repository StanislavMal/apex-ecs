#![allow(dead_code)]
// Apex ECS — Performance Benchmark (corrected v2)
// Запуск: cargo run -p apex-examples --example perf --release

use std::time::{Duration, Instant};
use apex_core::prelude::*;
use apex_core::access_desc;
use apex_macros::Component;
use apex_scheduler::{seq, sys, Scheduler, StageLabel};

// ── Компоненты ─────────────────────────────────────────────────

#[derive(Component, Clone, Copy)] struct Position    { x: f32, y: f32, z: f32 }
#[derive(Component, Clone, Copy)] struct Velocity    { x: f32, y: f32, z: f32 }
#[derive(Component, Clone, Copy)] struct Health      { current: f32, max: f32  }
#[derive(Component, Clone, Copy)] struct Mass(f32);
#[derive(Component, Clone, Copy)] struct Player;
#[derive(Component, Clone, Copy)] struct Enemy;
#[derive(Component, Clone, Copy)] struct Temperature(f32);
#[derive(Component, Clone, Copy)] struct Mana        { current: f32, max: f32  }

#[derive(Clone, Copy)] struct PhysicsConfig  { gravity: f32, dt: f32 }
#[derive(Clone, Copy, Default)]
                        struct FrameCounter  { count: u64 }
#[derive(Clone, Copy)] struct DamageEvent    { target_id: u32, amount: f32 }
#[derive(Clone, Copy)] struct CollisionEvent { a: u32, b: u32 }

// ── Harness ────────────────────────────────────────────────────
//
// Два harness'а:
//
//   bench_with_setup<S, T, F>(label, setup, f)
//     • setup() → T   : подготовка состояния, не входит в измерение
//     • f(T)   → u64  : только измеряемый код; возвращает ops
//     • Warmup: setup()+f() один раз, результат выброшен
//     • RUNS прогонов, медиана
//
//   bench_seq_par<S, FS, FP>(label, setup, run_seq, run_par)
//     • Измеряет frame_time = время одного run()
//     • setup() → World : свежий мир на каждый прогон
//     • Печатает SEQ / PAR / speedup

const RUNS: usize = 7;

fn bench_with_setup<S, T, F>(label: &str, mut setup: S, mut f: F)
where
    S: FnMut() -> T,
    F: FnMut(T) -> u64,
{
    // warmup
    {
        let state = setup();
        let _ = f(state);
    }

    let mut times: Vec<(Duration, u64)> = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let state   = setup();          // не входит в измерение
        let t0      = Instant::now();
        let ops     = f(state);         // только это измеряем
        let elapsed = t0.elapsed();
        times.push((elapsed, ops));
    }

    times.sort_by_key(|(d, _)| *d);
    let (elapsed, ops) = times[RUNS / 2];

    let ns    = elapsed.as_nanos() as f64;
    let ns_op = if ops > 0 { ns / ops as f64 } else { ns };
    let mops  = if elapsed.as_secs_f64() > 0.0 {
        ops as f64 / elapsed.as_secs_f64() / 1e6
    } else { f64::INFINITY };

    println!(
        "  {:<72} {:>10.2} ns/op  {:>8.2} M ops/s",
        label, ns_op, mops
    );
}

fn bench_seq_par<S, FS, FP>(
    label: &str,
    mut setup: S,
    mut run_seq: FS,
    mut run_par: FP,
) -> (f64, f64)
where
    S:  FnMut() -> World,
    FS: FnMut(&mut World),
    FP: FnMut(&mut World),
{
    // warmup — оба варианта
    { let mut w = setup(); run_seq(&mut w); }
    { let mut w = setup(); run_par(&mut w); }

    let collect_times = |run: &mut dyn FnMut(&mut World),
                         setup:  &mut dyn FnMut() -> World| -> Vec<Duration> {
        let mut v = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let mut w  = setup();
            let t0     = Instant::now();
            run(&mut w);
            std::hint::black_box(w.entity_count());
            v.push(t0.elapsed());
        }
        v.sort();
        v
    };

    let seq_times = collect_times(&mut run_seq, &mut setup);
    let par_times = collect_times(&mut run_par, &mut setup);

    let to_ms = |v: &[Duration]| v[RUNS / 2].as_secs_f64() * 1000.0;
    let seq_ms  = to_ms(&seq_times);
    let par_ms  = to_ms(&par_times);
    let speedup = if par_ms > 0.0 { seq_ms / par_ms } else { f64::INFINITY };

    println!(
        "  {:<68}  SEQ={:.3}ms  PAR={:.3}ms  speedup={:.2}x",
        label, seq_ms, par_ms, speedup
    );
    (seq_ms, par_ms)
}

// ── Фабрики ────────────────────────────────────────────────────

fn make_world_3comp(n: usize) -> World {
    let mut world = World::new();
    world.spawn_many_silent(n, |i| {
        let f = i as f32;
        (
            Position { x: f, y: f * 0.5, z: 0.0 },
            Velocity { x: 1.0, y: 0.5, z: 0.0 },
            Health   { current: 100.0, max: 100.0 },
        )
    });
    world
}

/// Возвращает (World, Vec<Entity>) — Vec нужен для тестов structural changes.
/// Vec строится через query после spawn, чтобы не платить за него в spawn-тестах.
fn make_world_3comp_with_entities(n: usize) -> (World, Vec<Entity>) {
    let world = make_world_3comp(n);
    let mut entities = Vec::with_capacity(n);
    world.query::<Read<Position>>().for_each(|e, _| entities.push(e));
    (world, entities)
}

fn make_world_5comp(n: usize) -> World {
    let mut world = World::new();
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

// ── Системы ────────────────────────────────────────────────────

system! {
    fn move_sys(
        q: (Read<Velocity>, Write<Position>),
    ) {
        q.for_each_mut(|_, (v, mut p)| {
            p.x += v.x * 0.016;
            p.y += v.y * 0.016;
            p.z += v.z * 0.016;
        });
    }
}

system! {
    fn hp_sys(
        q: Write<Health>,
    ) {
        q.for_each_mut(|_, mut hp| {
            hp.current = hp.current.min(hp.max).max(0.0);
        });
    }
}

system! {
    fn temp_sys(
        q: Write<Temperature>,
    ) {
        q.for_each_mut(|_, mut t| {
            t.0 += (20.0 - t.0) * 0.001;
        });
    }
}

system! {
    fn mana_sys(
        q: Write<Mana>,
    ) {
        q.for_each_mut(|_, mut m| {
            m.current = (m.current + 0.2).min(m.max);
        });
    }
}

system! {
    fn heavy_phys_sys(
        q: (Write<Velocity>, Write<Position>),
    ) {
        q.for_each_mut(|_, (mut v, mut p)| {
            let dt    = 0.016f32;
            let speed = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
            let angle = speed.atan2(1.0);
            let drag  = angle.cos() * 0.99;
            v.x = v.x * drag + angle.sin() * 0.001;
            v.y = v.y * drag - 9.8 * dt;
            v.z *= drag;
            p.x += v.x * dt;
            p.y += v.y * dt;
            p.z += v.z * dt;
            if p.y < 0.0 { p.y = 0.0; v.y = v.y.abs() * 0.8; }
        });
    }
}

system! {
    fn heavy_temp_sys(
        q: Write<Temperature>,
    ) {
        q.for_each_mut(|_, mut t| {
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

system! {
    fn heavy_mana_sys(
        q: Write<Mana>,
    ) {
        q.for_each_mut(|_, mut m| {
            let ratio = m.current / m.max;
            let regen = (1.0 - ratio).sqrt() * 0.5;
            m.current = (m.current + regen).min(m.max);
            if ratio > 0.9 {
                m.current *= 1.0 - (ratio - 0.9).powi(2) * 0.01;
            }
        });
    }
}

system! {
    fn auto_move_sys(
        q: (Read<Velocity>, Write<Position>),
    ) {
        q.for_each_mut(|_, (v, mut p)| {
            p.x += v.x * 0.016;
            p.y += v.y * 0.016;
        });
    }
}

// ── 1. Batch Allocator ─────────────────────────────────────────

fn bench_batch_allocator(n: usize) {
    println!("── Batch Entity Allocator ({n}k entities) ──────────────────────────────────────");

    // setup = () : весь код внутри f(), так как мы измеряем именно спавн
    bench_with_setup(
        &format!("spawn loop      ({n}k) [baseline]"),
        || (),
        |()| {
            let mut world = World::new();



            for i in 0..n * 1000 {
                let f = i as f32;
                world.spawn((
                    Position { x: f, y: f * 0.5, z: 0.0 },
                    Velocity { x: 1.0, y: 0.5, z: 0.0 },
                    Health   { current: 100.0, max: 100.0 },
                ));
            }
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("spawn_many             ({n}k) [batch+collect]"),
        || (),
        |()| {
            let mut world = World::new();



            let v = world.spawn_many(n * 1000, |i| {
                let f = i as f32;
                (
                    Position { x: f, y: f * 0.5, z: 0.0 },
                    Velocity { x: 1.0, y: 0.5, z: 0.0 },
                    Health   { current: 100.0, max: 100.0 },
                )
            });
            let len = v.len();
            std::hint::black_box(len);
            len as u64
        },
    );

    bench_with_setup(
        &format!("spawn_many_silent      ({n}k) [batch, no collect]"),
        || (),
        |()| {
            let mut world = World::new();



            world.spawn_many_silent(n * 1000, |i| {
                let f = i as f32;
                (
                    Position { x: f, y: f * 0.5, z: 0.0 },
                    Velocity { x: 1.0, y: 0.5, z: 0.0 },
                    Health   { current: 100.0, max: 100.0 },
                )
            });
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("spawn_many_silent 1comp ({n}k)"),
        || (),
        |()| {
            let mut world = World::new();

            world.spawn_many_silent(n * 1000, |i| {
                (Position { x: i as f32, y: 0.0, z: 0.0 },)
            });
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("spawn_many_silent 4comp ({n}k)"),
        || (),
        |()| {
            let mut world = World::new();




            world.spawn_many_silent(n * 1000, |i| {
                let f = i as f32;
                (
                    Position { x: f, y: 0.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                    Health   { current: 100.0, max: 100.0 },
                    Mass(1.0),
                )
            });
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("EntityAllocator::allocate_batch ({n}k) [ZST only]"),
        || (),
        |()| {
            let mut world = World::new();

            world.spawn_many_silent(n * 1000, |_| (Player,));
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );
}

// ── 2. has_relation ────────────────────────────────────────────
//
// Исправление: setup строит world + pairs, f() только проверяет.
// Пары строятся в setup → не входят во время измерения.

fn bench_has_relation(n: usize) {
    let checks = n * 1000;
    println!("\n── has_relation ({checks} checks, SubjectIndex) ──────────────────────────────────");

    let build = || {
        let parent_count = n * 100;
        let children_per = 8usize;
        let mut world    = World::new();


        let parents: Vec<Entity> = (0..parent_count)
            .map(|i| world.spawn((Position { x: i as f32, y: 0.0, z: 0.0 },)))
            .collect();

        for &parent in &parents {
            for j in 0..children_per {
                let child = world.spawn(
                    (Position { x: j as f32, y: 0.0, z: 0.0 },)
                );
                world.add_relation(child, ChildOf, parent);
            }
        }
        (world, parents)
    };

    // TRUE: pairs = (child, его настоящий parent)
    bench_with_setup(
        &format!("has_relation TRUE  ({checks})"),
        || {
            // setup: строим мир и пары — не входит в измерение
            let (world, parents) = build();
            let pairs: Vec<(Entity, Entity)> = parents.iter()
                .filter_map(|&p| world.targets_of(ChildOf, p).next().map(|c| (c, p)))
                .take(checks)
                .collect();
            (world, pairs)
        },
        |(world, pairs)| {
            // f: только has_relation
            let mut found = 0u64;
            for &(child, parent) in &pairs {
                if world.has_relation(child, ChildOf, parent) { found += 1; }
            }
            std::hint::black_box(found);
            pairs.len() as u64
        },
    );

    // FALSE: pairs = (child, «соседний» parent — всегда неверный)
    bench_with_setup(
        &format!("has_relation FALSE ({checks}, wrong parent, early-exit)"),
        || {
            let (world, parents) = build();
            let true_pairs: Vec<(Entity, Entity)> = parents.iter()
                .filter_map(|&p| world.targets_of(ChildOf, p).next().map(|c| (c, p)))
                .take(checks)
                .collect();
            // Подменяем parent на соседний — всегда false
            let false_pairs: Vec<(Entity, Entity)> = true_pairs.iter()
                .enumerate()
                .map(|(i, &(child, _))| (child, parents[(i + 1) % parents.len()]))
                .collect();
            (world, false_pairs)
        },
        |(world, pairs)| {
            let mut found = 0u64;
            for &(child, wrong_parent) in &pairs {
                if world.has_relation(child, ChildOf, wrong_parent) { found += 1; }
            }
            std::hint::black_box(found);
            pairs.len() as u64
        },
    );
}

// ── 3. Scheduler throughput ────────────────────────────────────
//
// Исправление: World строится в setup(), f() только запускает run().
// Compile() вынесен за пределы обоих — выполняется один раз.

fn bench_scheduler_throughput(n: usize) {
    println!("\n── Scheduler throughput ({n}k entities) — только run() ──────────────────────────");
    println!("  setup=World, compile вне измерения, f=только sched.run()");

    macro_rules! sched_bench {
        ($label:expr, $build_sched:expr, $world_fn:expr) => {{
            let mut sched = $build_sched;
            sched.compile().unwrap();
            bench_with_setup(
                $label,
                || $world_fn,          // setup → World
                |mut world: World| {   // f → только run
                    sched.run_sequential(&mut world);
                    std::hint::black_box(world.entity_count());
                    (n * 1000) as u64
                },
            );
        }};
    }

    sched_bench!(
        &format!("1 AutoSystem: movement      ({n}k)"),
        { let mut s = Scheduler::new(); s.add_systems(StageLabel::Update, sys("move", move_sys)); s },
        make_world_3comp(n * 1000)
    );

    {
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, sys("move", move_sys));
        sched.add_systems(StageLabel::Update, sys("hp", hp_sys));
        sched.compile().unwrap();
        let stages = sched.stages().unwrap().len();
        debug_assert_eq!(stages, 1, "ожидаем 1 Stage без конфликтов");
        bench_with_setup(
            &format!("2 AutoSystem no-conflict    ({n}k, 1 Stage)"),
            || make_world_3comp(n * 1000),
            |mut world: World| {
                sched.run_sequential(&mut world);
                std::hint::black_box(world.entity_count());
                (n * 1000) as u64
            },
        );
    }

    {
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, apex_scheduler::par_access(
            "physics",
            access_desc!(read<PhysicsConfig>, write<Position>),
            |ctx| {
                let dt = ctx.resource::<PhysicsConfig>().dt;
                ctx.query_unchecked::<Write<Position>>().for_each_mut(|_, mut pos| { pos.x += dt; });
            },
        ));
        sched.compile().unwrap();
        bench_with_setup(
            &format!("FnParSystem + resource     ({n}k)"),
            || {
                let mut world = make_world_3comp(n * 1000);
                world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
                world
            },
            |mut world: World| {
                sched.run_sequential(&mut world);
                std::hint::black_box(world.entity_count());
                (n * 1000) as u64
            },
        );
    }

    {
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, seq("move", |world: &mut World| {
            Query::<(Read<Velocity>, Write<Position>)>::new_mut(world)
                .for_each_mut(|_, (v, mut p)| { p.x += v.x; p.y += v.y; });
        }));
        sched.compile().unwrap();
        bench_with_setup(
            &format!("1 Sequential system        ({n}k)"),
            || make_world_3comp(n * 1000),
            |mut world: World| {
                sched.run_sequential(&mut world);
                std::hint::black_box(world.entity_count());
                (n * 1000) as u64
            },
        );
    }

    sched_bench!(
        &format!("1 AutoSystem               ({n}k)"),
        { let mut s = Scheduler::new(); s.add_systems(StageLabel::Update, sys("auto", auto_move_sys)); s },
        make_world_3comp(n * 1000)
    );

    // Debug plan
    {
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, sys("physics", move_sys));
        sched.add_systems(StageLabel::Update, sys("hp_clamp", hp_sys));
        sched.add_systems(StageLabel::Update, seq("commands", |_w: &mut World| {}));
        sched.add_systems(StageLabel::Update, sys("ai", move_sys));
        sched.compile().unwrap();
        println!("  Mixed pipeline plan:\n{}", sched.debug_plan());
    }
}

// ── 4. compile() overhead ─────────────────────────────────────

fn bench_compile_overhead() {
    println!("\n── Scheduler compile() overhead ────────────────────────────────────────────────");

    struct SimpleSys;
    impl AutoSystem for SimpleSys {
        type Query = Write<Position>;
        type Resources = ();
        type Events = ();
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    struct OtherSys;
    impl AutoSystem for OtherSys {
        type Query = Write<Velocity>;
        type Resources = ();
        type Events = ();
        fn run(&mut self, _: SystemContext<'_>) {}
    }

    println!("  --- A. Фиксированный N ---");

    for &n_sys in &[1usize, 5, 10, 20, 50] {
        bench_with_setup(
            &format!("compile() {n_sys:>2} par (no conflicts)"),
            || (),
            |()| {
                let mut sched = Scheduler::new();
                for i in 0..n_sys {
                    if i % 2 == 0 { sched.add_systems(StageLabel::Update, sys(format!("s{i}"), SimpleSys)); }
                    else           { sched.add_systems(StageLabel::Update, sys(format!("s{i}"), OtherSys));  }
                }
                sched.compile().unwrap();
                std::hint::black_box(sched.stages().unwrap().len());
                1
            },
        );
    }

    println!();
    for &n_sys in &[2usize, 5, 10, 20] {
        bench_with_setup(
            &format!("compile() {n_sys:>2} par (all Write<Pos>, max conflicts)"),
            || (),
            |()| {
                let mut sched = Scheduler::new();
                for i in 0..n_sys { sched.add_systems(StageLabel::Update, sys(format!("s{i}"), SimpleSys)); }
                sched.compile().unwrap();
                debug_assert_eq!(sched.stages().unwrap().len(), n_sys);
                std::hint::black_box(sched.stages().unwrap().len());
                1
            },
        );
    }

    println!();
    bench_with_setup(
        "compile() 10 sys: 5 par + 5 seq",
        || (),
        |()| {
            let mut sched = Scheduler::new();
            for i in 0..5 { sched.add_systems(StageLabel::Update, sys(format!("p{i}"), OtherSys)); }
            for i in 0..5 { sched.add_systems(StageLabel::Update, seq(format!("s{i}"), |_w: &mut World| {})); }
            sched.compile().unwrap();
            std::hint::black_box(sched.stages().unwrap().len());
            1
        },
    );

    println!("\n  --- B. Инкрементальный ---");
    println!("  Суммарно: compile(1)+compile(2)+...+compile(N), новый Scheduler per прогон");

    for &n_sys in &[1usize, 5, 10, 20] {
        bench_with_setup(
            &format!("инкрементальный {n_sys:>2} sys ({n_sys} compile())"),
            || (),
            |()| {
                let mut sched = Scheduler::new();
                for i in 0..n_sys {
                    sched.add_systems(StageLabel::Update, sys(format!("s{i}"), OtherSys));
                    sched.compile().unwrap();
                }
                std::hint::black_box(sched.stages().unwrap().len());
                1
            },
        );
    }

    println!("\n  --- C. batch vs инкрементальный ---");
    println!("  Теория O(N²): инкрем. должен быть ~3.85x дороже для N=10");

    for &n_sys in &[10usize, 20] {
        bench_with_setup(
            &format!("batch:          1 compile() для {n_sys} sys"),
            || (),
            |()| {
                let mut sched = Scheduler::new();
                for i in 0..n_sys { sched.add_systems(StageLabel::Update, sys(format!("s{i}"), OtherSys)); }
                sched.compile().unwrap();
                std::hint::black_box(sched.stages().unwrap().len());
                1
            },
        );
        bench_with_setup(
            &format!("инкрементальный: {n_sys} compile() для {n_sys} sys"),
            || (),
            |()| {
                let mut sched = Scheduler::new();
                for i in 0..n_sys {
                    sched.add_systems(StageLabel::Update, sys(format!("s{i}"), OtherSys));
                    sched.compile().unwrap();
                }
                std::hint::black_box(sched.stages().unwrap().len());
                1
            },
        );
        println!();
    }

    println!("  --- D. Идемпотентность: только 2й compile() ---");
    println!("  setup=первый compile(), f=только второй compile()");

    for &n_sys in &[5usize, 10, 20] {
        bench_with_setup(
            &format!("2й compile() (graph_dirty=false, N={n_sys})"),
            || {
                // setup: первый compile — не входит в измерение
                let mut sched = Scheduler::new();
                for i in 0..n_sys { sched.add_systems(StageLabel::Update, sys(format!("s{i}"), OtherSys)); }
                sched.compile().unwrap();
                sched
            },
            |mut sched: Scheduler| {
                // f: только второй compile()
                sched.compile().unwrap();
                std::hint::black_box(sched.stages().unwrap().len());
                1
            },
        );
    }

    println!("\n  --- E. recompile после изменений ---");
    println!("  setup=compile(N), f=add+recompile(N+1)");

    bench_with_setup(
        "add 1 system → recompile (N=10 → 11)",
        || {
            let mut sched = Scheduler::new();
            for i in 0..10 { sched.add_systems(StageLabel::Update, sys(format!("s{i}"), OtherSys)); }
            sched.compile().unwrap();
            sched
        },
        |mut sched: Scheduler| {
            sched.add_systems(StageLabel::Update, sys("s_new", OtherSys));
            sched.compile().unwrap();
            std::hint::black_box(sched.stages().unwrap().len());
            1
        },
    );

    bench_with_setup(
        "after() → recompile (N=5)",
        || {
            let mut sched = Scheduler::new();
            sched.add_systems(StageLabel::Update, (sys("sa", OtherSys), sys("sb", OtherSys)));
            for i in 2..5 { sched.add_systems(StageLabel::Update, sys(format!("s{i}"), OtherSys)); }
            sched.compile().unwrap();
            sched
        },
        |mut sched: Scheduler| {
            sched.after("sb", "sa").unwrap();
            sched.compile().unwrap();
            std::hint::black_box(sched.stages().unwrap().len());
            1
        },
    );
}

// ── 5. Resources ───────────────────────────────────────────────

fn bench_resources(n: usize) {
    println!("\n── Resources ({n}k операций) ─────────────────────────────────────────────────────");

    // World с ресурсами — мал, всегда горячий в кеше.
    // setup возвращает World, f только читает/пишет ресурс.
    bench_with_setup(
        &format!("resource::<T>() read      ({n}k)"),
        || {
            let mut w = World::new();
            w.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
            w
        },
        |world: World| {
            let mut sum = 0.0f64;
            for _ in 0..n * 1000 {
                sum += world.resource::<PhysicsConfig>().gravity as f64;
            }
            std::hint::black_box(sum);
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("resource_mut::<T>() write ({n}k)"),
        || {
            let mut w = World::new();
            w.insert_resource(FrameCounter::default());
            w
        },
        |mut world: World| {
            for i in 0..n * 1000 {
                world.resource_mut::<FrameCounter>().count = i as u64;
            }
            std::hint::black_box(world.resource::<FrameCounter>().count);
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("has_resource::<T>()       ({n}k)"),
        || {
            let mut w = World::new();
            w.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
            w
        },
        |world: World| {
            let mut found = 0u64;
            for _ in 0..n * 1000 {
                if world.has_resource::<PhysicsConfig>() { found += 1; }
            }
            std::hint::black_box(found);
            (n * 1000) as u64
        },
    );
}

// ── 6. Events ──────────────────────────────────────────────────

fn bench_events(n: usize) {
    println!("\n── Events ({n}k событий) ─────────────────────────────────────────────────────────");

    bench_with_setup(
        &format!("send→tick→iter        ({n}k)"),
        || {
            let mut w = World::new();
            w.add_event::<DamageEvent>();
            w
        },
        |mut world: World| {
            let cursor = world.events_mut::<DamageEvent>().add_reader();
            for i in 0..n * 1000 {
                world.send_event(DamageEvent { target_id: i as u32, amount: 10.0 });
            }
            world.tick();
            world.flush_all_events();
            let mut sum = 0.0f32;
            for ev in world.events::<DamageEvent>().iter(&cursor) { sum += ev.amount; }
            std::hint::black_box(sum);
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("send→tick→iter (prev) ({n}k)"),
        || {
            let mut w = World::new();
            w.add_event::<DamageEvent>();
            w
        },
        |mut world: World| {
            let cursor = world.events_mut::<DamageEvent>().add_reader();
            for i in 0..n * 1000 {
                world.send_event(DamageEvent { target_id: i as u32, amount: 5.0 });
            }
            world.tick();
            world.flush_all_events();
            let mut sum = 0.0f32;
            for ev in world.events::<DamageEvent>().iter(&cursor) { sum += ev.amount; }
            std::hint::black_box(sum);
            (n * 1000) as u64
        },
    );
}

// ── 7. Query ───────────────────────────────────────────────────
//
// Исправления:
//   • World строится в setup() — не входит во время query
//   • «0 results» тест измеряет стоимость обхода архетипов без совпадений:
//     ops = количество entity в мире (платим за обход, даже если 0 подходит)
//   • Query::new vs CachedQuery: оба измеряются честно (world готов в setup)

fn bench_query(n: usize) {
    println!("\n── Query ({n}k entities) ─────────────────────────────────────────────────────────");

    bench_with_setup(
        &format!("Query::new + for_each              ({n}k)"),
        || make_world_3comp(n * 1000),
        |world: World| {
            let mut sum = 0.0f32;
            Query::<Read<Position>>::new(&world)
                .for_each(|_, p| { sum += p.x; });
            std::hint::black_box(sum);
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("CachedQuery + for_each             ({n}k)"),
        || make_world_3comp(n * 1000),
        |world: World| {
            let mut sum = 0.0f32;
            world.query::<Read<Position>>()
                .for_each(|_, p| { sum += p.x; });
            std::hint::black_box(sum);
            (n * 1000) as u64
        },
    );

    bench_with_setup(
        &format!("Query<(Read<Vel>, Write<Pos>)>    ({n}k)"),
        || make_world_3comp(n * 1000),
        |mut world: World| {
            Query::<(Read<Velocity>, Write<Position>)>::new_mut(&mut world)
                .for_each_mut(|_, (v, mut p)| { p.x += v.x; p.y += v.y; });
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );

    // «0 results»: With<Player> не совпадает ни с одним архетипом.
    // ops = кол-во entity = стоимость обхода архетипов (реальная работа планировщика).
    // Не делим на 1 — честный ns/entity для «пустого» запроса.
    bench_with_setup(
        &format!("Query<With<Player>> 0 results     ({n}k entities, обход архетипов)"),
        || {
            // Мир с Player-entity — чтобы архетип существовал, но был отдельным
            let mut world = make_world_3comp(n * 1000);

            // Добавляем несколько Player-entity в отдельный архетип
            // (не совпадающий с запросом Read<Position> + With<Player>
            //  потому что у них нет Position)
            for _ in 0..100 { world.spawn((Player,)); }
            world
        },
        |world: World| {
            let mut c = 0u64;
            // Запрос: нужны entity с Position И Player — таких нет
            Query::<(Read<Position>, With<Player>)>::new(&world)
                .for_each(|_, _| { c += 1; });
            std::hint::black_box(c);
            // ops = кол-во entity в мире (обошли все архетипы)
            world.entity_count() as u64
        },
    );
}

// ── 8. Structural changes ──────────────────────────────────────
//
// Исправления:
//   • despawn и Commands тесты: entities строятся в setup() (Vec не входит)
//   • insert: сначала спавн без Mass (в setup), потом insert Mass (в f)

fn bench_structural(n: usize) {
    println!("\n── Structural changes ({n}k entity) ─────────────────────────────────────────────");

    // insert: спавн в setup, insert в f
    bench_with_setup(
        &format!("insert component  ({n}k) [архетип-переход per entity]"),
        || {
            // setup: спавн без Mass
            let mut world = World::new();



            let mut entities = Vec::with_capacity(n * 1000);
            for i in 0..n * 1000 {
                let e = world.spawn((
                    Position { x: i as f32, y: 0.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                ));
                entities.push(e);
            }
            (world, entities)
        },
        |(mut world, entities): (World, Vec<Entity>)| {
            // f: только insert — переход архетипа для каждой entity
            for &e in &entities { world.insert(e, Mass(1.0)); }
            std::hint::black_box(world.entity_count());
            entities.len() as u64
        },
    );

    // despawn: спавн в setup, despawn в f
    bench_with_setup(
        &format!("despawn           ({n}k)"),
        || make_world_3comp_with_entities(n * 1000),
        |(mut world, entities): (World, Vec<Entity>)| {
            for e in entities { world.despawn(e); }
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );

    // Commands: query в setup (собираем entity), apply в f
    bench_with_setup(
        &format!("Commands::despawn + apply ({n}k)"),
        || {
            let (world, entities) = make_world_3comp_with_entities(n * 1000);
            // Собираем команды в setup — они дешёвые (просто Vec::push)
            let mut cmds = Commands::with_capacity(n * 1000);
            for &e in &entities { cmds.despawn(e); }
            (world, cmds)
        },
        |(mut world, mut cmds): (World, Commands)| {
            // f: только apply — реальная работа
            cmds.apply(&mut world);
            std::hint::black_box(world.entity_count());
            (n * 1000) as u64
        },
    );
}

// ── 9. Параллельный планировщик ────────────────────────────────

fn bench_parallel_scheduler(n: usize) {
    println!(
        "\n── Parallel Scheduler — frame time (rayon threads: {}) ─────────",
        rayon::current_num_threads()
    );
    println!("  Метрика: frame_time = время sched.run() | speedup = seq/par");
    println!("  compile() в setup() — не входит в измерение");

    // ── Вспомогательная функция ──────────────────────────────
    // Принимает готовый скомпилированный Scheduler, прогоняет bench_seq_par.
    // compile() вызывается один раз в setup — честно.
    fn run_bench(
        label: &str,
        setup: impl FnMut() -> World,
        mut seq_sched: Scheduler,
        mut par_sched: Scheduler,
    ) -> (f64, f64)
    where
        // типы не нужны — замыкания строятся внутри
    {
        bench_seq_par(
            label,
            setup,
            move |w| seq_sched.run_sequential(w),
            move |w| par_sched.run(w),
        )
    }

    // ── Строим пары (seq_sched, par_sched) с одинаковым набором систем ──
    macro_rules! make_scheds {
        ($($add:expr),+ $(,)?) => {{
            let build = || {
                let mut s = Scheduler::new();
                $( $add(&mut s); )+
                s.compile().unwrap();
                s
            };
            (build(), build())
        }};
    }

    // ── Лёгкая нагрузка (memory-bound) ──────────────────────
    println!("\n  --- Лёгкая нагрузка (memory-bound) ---");

    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("move", move_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("hp", hp_sys)); }
        );
        bench_seq_par(
            &format!("2 лёгких системы ({n}k)  Move+Hp"),
            || make_world_5comp(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("move", move_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("hp", hp_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", temp_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("mana", mana_sys)); }
        );
        // Проверяем 1 Stage
        debug_assert_eq!(seq.stages().unwrap().len(), 1);
        bench_seq_par(
            &format!("4 лёгких системы ({n}k)  Move+Hp+Temp+Mana  [1 Stage]"),
            || make_world_5comp(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    // ── Тяжёлая нагрузка — ИЗОЛИРОВАННЫЕ архетипы ────────────
    //
    // Ключевое исправление: каждая система работает со СВОИМ архетипом.
    // Это устраняет false sharing кеш-линий между системами.
    //
    // Архетип A: Pos + Vel         → heavy_phys_sys (write Pos, write Vel)
    // Архетип B: Temp              → heavy_temp_sys (write Temp)
    // Архетип C: Mana              → heavy_mana_sys (write Mana)
    //
    // Системы не пересекаются ни по компонентам, ни по памяти.

    println!("\n  --- Тяжёлая нагрузка (CPU-bound, ИЗОЛИРОВАННЫЕ архетипы) ---");
    println!("  Архетип A: Pos+Vel → HeavyPhys | Архетип B: Temp → HeavyTemp");

    let make_isolated_world = |n: usize| {
        let mut world = World::new();





        // Архетип A: только Pos + Vel (для heavy_phys_sys)
        world.spawn_many_silent(n, |i| {
            let f = i as f32;
            (
                Position { x: f, y: f * 0.5, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
            )
        });
        // Архетип B: только Temp (для heavy_temp_sys)
        world.spawn_many_silent(n, |i| {
            (Temperature(20.0 + i as f32 * 0.001),)
        });
        // Архетип C: только Mana (для heavy_mana_sys)
        world.spawn_many_silent(n, |i| {
            (Mana { current: i as f32 % 100.0, max: 100.0 },)
        });
        world
    };

    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", heavy_phys_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", heavy_temp_sys)); }
        );
        bench_seq_par(
            &format!("2 CPU-bound, изолированные архетипы ({n}k each)"),
            || make_isolated_world(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", heavy_phys_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", heavy_temp_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("mana", heavy_mana_sys)); }
        );
        bench_seq_par(
            &format!("3 CPU-bound, изолированные архетипы ({n}k each)"),
            || make_isolated_world(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    // ── Для сравнения: те же системы, общий архетип ───────────
    //
    // Разница с изолированным тестом показывает стоимость false sharing.

    println!("\n  --- Тяжёлая нагрузка (CPU-bound, ОБЩИЙ архетип — для сравнения) ---");
    println!("  Все компоненты в одном архетипе → false sharing кеш-линий");

    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", heavy_phys_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", heavy_temp_sys)); }
        );
        bench_seq_par(
            &format!("2 CPU-bound, общий архетип Pos+Vel+Temp+Mana ({n}k)"),
            || make_world_5comp(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", heavy_phys_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", heavy_temp_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("mana", heavy_mana_sys)); }
        );
        bench_seq_par(
            &format!("3 CPU-bound, общий архетип Pos+Vel+Temp+Mana ({n}k)"),
            || make_world_5comp(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    // ── Сравнение for_each vs par_for_each ──────────────────────────────
    //
    // Ключевой тест: проверяем, даёт ли par_for_each (intra-system
    // parallelism) прирост по сравнению с for_each (sequential)
    // в межсистемном режиме.
    //
    // Системы: HeavyPhysParSys (par_for_each) + HeavyTempParSys (par_for_each)
    // vs heavy_phys_sys (for_each) + heavy_temp_sys (for_each)
    //
    // в то время как for_each использует только 1 ядро на систему.

    println!("\n  --- Сравнение for_each vs par_for_each (межсистемный) ---");

    struct HeavyPhysParSys;
    impl AutoSystem for HeavyPhysParSys {
        type Query = (Write<Velocity>, Write<Position>);
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<(Write<Velocity>, Write<Position>)>().par_for_each_mut(|_, (mut v, mut p)| {
                let dt    = 0.016f32;
                let speed = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                let angle = speed.atan2(1.0);
                let drag  = angle.cos() * 0.99;
                v.x = v.x * drag + angle.sin() * 0.001;
                v.y = v.y * drag - 9.8 * dt;
                v.z *= drag;
                p.x += v.x * dt;
                p.y += v.y * dt;
                p.z += v.z * dt;
                if p.y < 0.0 { p.y = 0.0; v.y = v.y.abs() * 0.8; }
            });
        }
    }

    struct HeavyTempParSys;
    impl AutoSystem for HeavyTempParSys {
        type Query = Write<Temperature>;
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<Write<Temperature>>().par_for_each_mut(|_, mut t| {
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

    struct HeavyManaParSys;
    impl AutoSystem for HeavyManaParSys {
        type Query = Write<Mana>;
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<Write<Mana>>().par_for_each_mut(|_, mut m| {
                let ratio = m.current / m.max;
                let regen = (1.0 - ratio).sqrt() * 0.5;
                m.current = (m.current + regen).min(m.max);
                if ratio > 0.9 {
                    m.current *= 1.0 - (ratio - 0.9).powi(2) * 0.01;
                }
            });
        }
    }

    // Тест 1: for_each vs par_for_each, изолированные архетипы, 2 системы
    {
        let (seq_sched, par_sched) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", heavy_phys_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", heavy_temp_sys)); }
        );
        let (seq_par_sched, par_par_sched) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", HeavyPhysParSys).par_for_each_used()); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", HeavyTempParSys).par_for_each_used()); },
        );
        bench_seq_par(
            &format!("[for_each] 2 CPU-bound, изол. архетипы ({n}k each)"),
            || make_isolated_world(n * 1000),
            { let mut s = seq_sched; move |w| s.run_sequential(w) },
            { let mut s = par_sched; move |w| s.run(w) },
        );
        bench_seq_par(
            &format!("[par_for_each] 2 CPU-bound, изол. архетипы ({n}k each)"),
            || make_isolated_world(n * 1000),
            { let mut s = seq_par_sched; move |w| s.run_sequential(w) },
            { let mut s = par_par_sched; move |w| s.run(w) },
        );
    }

    // Тест 2: for_each vs par_for_each, изолированные архетипы, 3 системы
    {
        let (seq_sched, par_sched) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", heavy_phys_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", heavy_temp_sys)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("mana", heavy_mana_sys)); }
        );
        let (seq_par_sched, par_par_sched) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("phys", HeavyPhysParSys).par_for_each_used()); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("temp", HeavyTempParSys).par_for_each_used()); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("mana", HeavyManaParSys).par_for_each_used()); },
        );
        bench_seq_par(
            &format!("[for_each] 3 CPU-bound, изол. архетипы ({n}k each)"),
            || make_isolated_world(n * 1000),
            { let mut s = seq_sched; move |w| s.run_sequential(w) },
            { let mut s = par_sched; move |w| s.run(w) },
        );
        bench_seq_par(
            &format!("[par_for_each] 3 CPU-bound, изол. архетипы ({n}k each)"),
            || make_isolated_world(n * 1000),
            { let mut s = seq_par_sched; move |w| s.run_sequential(w) },
            { let mut s = par_par_sched; move |w| s.run(w) },
        );
    }

    // ── Максимальный межсистемный параллелизм: N систем = N ядер ──
    //
    // Ключевой тест: создаём 12 систем, каждая пишет в СВОЙ уникальный
    // компонент. Каждая система работает со своим архетипом.
    // Это позволяет измерить максимальный achievable speedup
    // межсистемного параллелизма без конкуренции за данные.
    //
    // Если speedup << 8x — проблема в планировщике (rayon::par_iter),
    // а не в SubWorld или кеш-конкуренции.

    println!("\n  --- Максимальный межсистемный параллелизм: 12 систем × 1 компонент ---");

    // ── 12 уникальных компонентов ─────────────────────────────
    #[derive(Component, Clone, Copy)] struct C0(f32);
    #[derive(Component, Clone, Copy)] struct C1(f32);
    #[derive(Component, Clone, Copy)] struct C2(f32);
    #[derive(Component, Clone, Copy)] struct C3(f32);
    #[derive(Component, Clone, Copy)] struct C4(f32);
    #[derive(Component, Clone, Copy)] struct C5(f32);
    #[derive(Component, Clone, Copy)] struct C6(f32);
    #[derive(Component, Clone, Copy)] struct C7(f32);
    #[derive(Component, Clone, Copy)] struct C8(f32);
    #[derive(Component, Clone, Copy)] struct C9(f32);
    #[derive(Component, Clone, Copy)] struct C10(f32);
    #[derive(Component, Clone, Copy)] struct C11(f32);

    // ── 12 систем, каждая пишет в свой компонент ──────────────
    macro_rules! make_solo_sys {
        ($name:ident, $comp:ty) => {
            system! {
                fn $name(
                    q: Write<$comp>,
                ) {
                    q.for_each_mut(|_, mut c| {
                        c.0 = (c.0 * 1.01 + 0.5).sin();
                    });
                }
            }
        };
    }

    make_solo_sys!(SoloSys0, C0);
    make_solo_sys!(SoloSys1, C1);
    make_solo_sys!(SoloSys2, C2);
    make_solo_sys!(SoloSys3, C3);
    make_solo_sys!(SoloSys4, C4);
    make_solo_sys!(SoloSys5, C5);
    make_solo_sys!(SoloSys6, C6);
    make_solo_sys!(SoloSys7, C7);
    make_solo_sys!(SoloSys8, C8);
    make_solo_sys!(SoloSys9, C9);
    make_solo_sys!(SoloSys10, C10);
    make_solo_sys!(SoloSys11, C11);

    // ── Мир с 12 архетипами, по одному компоненту в каждом ────
    let make_12arch_world = |n: usize| {
        let mut world = World::new();













        world.spawn_many_silent(n, |i| (C0(i as f32),));
        world.spawn_many_silent(n, |i| (C1(i as f32),));
        world.spawn_many_silent(n, |i| (C2(i as f32),));
        world.spawn_many_silent(n, |i| (C3(i as f32),));
        world.spawn_many_silent(n, |i| (C4(i as f32),));
        world.spawn_many_silent(n, |i| (C5(i as f32),));
        world.spawn_many_silent(n, |i| (C6(i as f32),));
        world.spawn_many_silent(n, |i| (C7(i as f32),));
        world.spawn_many_silent(n, |i| (C8(i as f32),));
        world.spawn_many_silent(n, |i| (C9(i as f32),));
        world.spawn_many_silent(n, |i| (C10(i as f32),));
        world.spawn_many_silent(n, |i| (C11(i as f32),));
        world
    };

    // ── Тест 1: 2 системы ─────────────────────────────────────
    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s0", SoloSys0)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s1", SoloSys1)); }
        );
        bench_seq_par(
            &format!("2 solo-системы, 2 архетипа ({n}k each)"),
            || make_12arch_world(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    // ── Тест 2: 4 системы ─────────────────────────────────────
    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s0", SoloSys0)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s1", SoloSys1)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s2", SoloSys2)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s3", SoloSys3)); }
        );
        bench_seq_par(
            &format!("4 solo-системы, 4 архетипа ({n}k each)"),
            || make_12arch_world(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    // ── Тест 3: 8 систем ─────────────────────────────────────
    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s0", SoloSys0)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s1", SoloSys1)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s2", SoloSys2)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s3", SoloSys3)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s4", SoloSys4)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s5", SoloSys5)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s6", SoloSys6)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s7", SoloSys7)); }
        );
        bench_seq_par(
            &format!("8 solo-систем, 8 архетипов ({n}k each)"),
            || make_12arch_world(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    // ── Тест 4: 12 систем (полная загрузка всех ядер) ─────────
    {
        let (seq, par) = make_scheds!(
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s0", SoloSys0)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s1", SoloSys1)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s2", SoloSys2)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s3", SoloSys3)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s4", SoloSys4)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s5", SoloSys5)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s6", SoloSys6)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s7", SoloSys7)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s8", SoloSys8)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s9", SoloSys9)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s10", SoloSys10)); },
            |s: &mut Scheduler| { s.add_systems(StageLabel::Update, sys("s11", SoloSys11)); }
        );
        bench_seq_par(
            &format!("12 solo-систем, 12 архетипов ({n}k each)"),
            || make_12arch_world(n * 1000),
            { let mut s = seq; move |w| s.run_sequential(w) },
            { let mut s = par; move |w| s.run(w) },
        );
    }

    // ── Pipeline с барьером ───────────────────────────────────
    println!("\n  --- Pipeline с Sequential барьером ---");

    {
        let build_sched = || {
            let mut s = Scheduler::new();
            s.add_systems(StageLabel::Update, sys("p1", move_sys));
            s.add_systems(StageLabel::Update, sys("h1", hp_sys));
            s.add_systems(StageLabel::Update, seq("barrier", |_: &mut World| {}));
            s.add_systems(StageLabel::Update, sys("p2", move_sys));
            s.add_systems(StageLabel::Update, sys("h2", hp_sys));
            s.compile().unwrap();
            s
        };
        let mut seq = build_sched();
        let mut par = build_sched();
        bench_seq_par(
            &format!("[Move+Hp] → barrier → [Move2+Hp2] ({n}k)"),
            || make_world_5comp(n * 1000),
            move |w| seq.run_sequential(w),
            move |w| par.run(w),
        );
    }

    // ── Debug plan ────────────────────────────────────────────
    {
        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, sys("move", move_sys));
        sched.add_systems(StageLabel::Update, sys("hp", hp_sys));
        sched.add_systems(StageLabel::Update, sys("temp", temp_sys));
        sched.add_systems(StageLabel::Update, sys("mana", mana_sys));
        sched.add_systems(StageLabel::Update, seq("commands", |_w: &mut World| {}));
        sched.add_systems(StageLabel::Update, sys("move2", move_sys));
        sched.add_systems(StageLabel::Update, sys("hp2", hp_sys));
        let test_world = make_world_5comp(n * 1000);
        sched.compile_with_world(&test_world).unwrap();
        println!("\n  Pipeline plan:\n{}", sched.debug_plan());
    }
}

// ── 10. Intra-system параллелизм ───────────────────────────────

fn bench_intra_system_parallel(n: usize) {
    println!("\n── Intra-system Parallelism — par_for_each ────────────────────────────────────");
    println!("  rayon threads: {}", rayon::current_num_threads());
    println!("  setup=World, f=только run() | speedup = seq_frame / par_frame");

    let make_multiarch = || {
        let quarter = n * 25;
        let mut world = World::new();







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
        world
    };

    struct LightSeqSys;
    impl AutoSystem for LightSeqSys {
        type Query = (Read<Velocity>, Write<Position>);
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<(Read<Velocity>, Write<Position>)>().for_each_mut(|_, (v, mut p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                let len = (p.x * p.x + p.y * p.y).sqrt();
                if len > 10000.0 { p.x /= len; p.y /= len; }
            });
        }
    }

    struct LightParSys;
    impl AutoSystem for LightParSys {
        type Query = (Read<Velocity>, Write<Position>);
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<(Read<Velocity>, Write<Position>)>().par_for_each_mut(|_, (v, mut p)| {
                p.x += v.x * 0.016;
                p.y += v.y * 0.016;
                let len = (p.x * p.x + p.y * p.y).sqrt();
                if len > 10000.0 { p.x /= len; p.y /= len; }
            });
        }
    }

    struct HeavySeqSys;
    impl AutoSystem for HeavySeqSys {
        type Query = (Read<Velocity>, Write<Position>);
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<(Read<Velocity>, Write<Position>)>().for_each_mut(|_, (v, mut p)| {
                let dt    = 0.016f32;
                let speed = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                let angle = speed.atan2(1.0);
                let drag  = angle.cos() * 0.99;
                p.x += v.x * drag * dt;
                p.y += v.y * drag * dt - 9.8 * dt * dt * 0.5;
                p.z += v.z * drag * dt;
            });
        }
    }

    struct HeavyIntraParSys;
    impl AutoSystem for HeavyIntraParSys {
        type Query = (Read<Velocity>, Write<Position>);
        type Resources = ();
        type Events = ();
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query_unchecked::<(Read<Velocity>, Write<Position>)>().par_for_each_mut(|_, (v, mut p)| {
                let dt    = 0.016f32;
                let speed = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                let angle = speed.atan2(1.0);
                let drag  = angle.cos() * 0.99;
                p.x += v.x * drag * dt;
                p.y += v.y * drag * dt - 9.8 * dt * dt * 0.5;
                p.z += v.z * drag * dt;
            });
        }
    }

    println!("  --- Лёгкая нагрузка (memory-bound) ---");
    bench_seq_par(
        &format!("for_each vs par_for_each ({n}k, 4 archetypes, sqrt)"),
        make_multiarch,
        |world| {
            let mut s = Scheduler::new();
            s.add_systems(StageLabel::Update, sys("seq", LightSeqSys));
            s.compile().unwrap();
            s.run_sequential(world);
        },
        |world| {
            let mut s = Scheduler::new();
            s.add_systems(StageLabel::Update, sys("par", LightParSys));
            s.compile().unwrap();
            s.run_sequential(world); // intra-sys par через rayon внутри системы
        },
    );

    println!("  --- Тяжёлая нагрузка (CPU-bound: atan2 + cos) ---");
    bench_seq_par(
        &format!("for_each vs par_for_each ({n}k, 4 archetypes, atan2+cos)"),
        make_multiarch,
        |world| {
            let mut s = Scheduler::new();
            s.add_systems(StageLabel::Update, sys("seq", HeavySeqSys));
            s.compile().unwrap();
            s.run_sequential(world);
        },
        |world| {
            let mut s = Scheduler::new();
            s.add_systems(StageLabel::Update, sys("par", HeavyIntraParSys));
            s.compile().unwrap();
            s.run_sequential(world);
        },
    );

    println!("  Note: speedup при CPU-bound и кол-во entity >> max_chunk_size");
}

// ── main ───────────────────────────────────────────────────────

fn main() {
    // Chunk sizing is now per-world config (wave 3 §1.7): the former global
    // `PAR_CHUNK_SIZE` atomic is gone. To honor `APEX_PAR_CHUNK_SIZE` in a bench,
    // apply `world.set_chunk_config(ChunkConfig::from_env())` at world setup.

    println!("=== Apex ECS — Performance Benchmark v2 ===");
    println!("Build: {}",
        if cfg!(debug_assertions) { "DEBUG ⚠  (запускайте с --release)" }
        else                      { "RELEASE ✓" }
    );
    println!("Mode:  PARALLEL (rayon threads: {})", rayon::current_num_threads());
    let max_chunk = apex_core::world::ChunkConfig::from_env().max_chunk_size;
    if max_chunk == apex_core::world::DEFAULT_MAX_CHUNK_SIZE {
        println!("max_chunk_size: auto (DEFAULT_MAX_CHUNK_SIZE={max_chunk}) (set APEX_PAR_CHUNK_SIZE to override)");
    } else {
        println!("max_chunk_size: {max_chunk} (from APEX_PAR_CHUNK_SIZE)");
    }
    println!();

    const N: usize = 100; // → N*1000 entity в большинстве тестов

    bench_batch_allocator(N);
    bench_has_relation(N);
    bench_scheduler_throughput(N);
    bench_compile_overhead();
    bench_resources(N);
    bench_events(N);
    bench_query(N);
    bench_structural(N);
    bench_parallel_scheduler(N);
    bench_intra_system_parallel(N);

    println!("\n── Методология ─────────────────────────────────────────────────────────────────");
    println!("  • {} прогонов на тест, медиана", RUNS);
    println!("  • warmup = 1 прогон перед измерением (не входит в медиану)");
    println!("  • bench_with_setup: setup() не входит во время — только f()");
    println!("  • bench_seq_par: frame_time = время одного run(), speedup = seq/par");
    println!("  • has_relation: pairs строятся в setup(), f() — только проверки");
    println!("  • scheduler throughput: World в setup(), f() — только sched.run()");
    println!("  • structural: spawn/entities в setup(), f() — только insert/despawn/apply");
    println!("  • query «0 results»: ops = entity_count (стоимость обхода архетипов)");
}
