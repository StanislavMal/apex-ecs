//! apex-examples: Диагностика параллелизма — scaling benchmark
//!
//! Запуск:
//!   cargo run -p apex-examples --example parallel_diagnostics --release
//!   cargo run -p apex-examples --example parallel_diagnostics --release
//!
//! Что тестируется:
//!   1. SEQ vs PAR — scaling по entity count
//!   2. Внутрисистемный параллелизм — 1 система, много entity
//!   3. Stage-level параллелизм — несколько систем в одном Stage
//!   4. Фрагментация архетипов — 1 vs 4 архетипа
//!   5. Event pipeline — overhead событий
//!   6. ResWrite contention — конкуренция за ресурс

use std::time::{Duration, Instant};
use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::{par_access, sys, Scheduler, StageLabel};

// ═══════════════════════════════════════════════════════════════════════════════
// КОМПОНЕНТЫ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Component, Clone, Copy, Debug)]
struct Position { x: f32, y: f32, z: f32 }

#[derive(Component, Clone, Copy, Debug)]
struct Velocity { x: f32, y: f32, z: f32 }

#[derive(Component, Clone, Copy, Debug)]
#[allow(dead_code)]
struct Health { current: f32, max: f32 }

#[derive(Component, Clone, Copy, Debug)]
struct Mass(f32);

#[derive(Component, Clone, Copy, Debug)]
#[allow(dead_code)]
struct Acceleration { x: f32, y: f32, z: f32 }

#[derive(Component, Clone, Copy, Debug)]
struct Damage(f32);

#[derive(Component, Clone, Copy, Debug)]
struct Cooldown(f32);

#[derive(Component, Clone, Copy, Debug)] struct TagA;
#[derive(Component, Clone, Copy, Debug)] struct TagB;
#[derive(Component, Clone, Copy, Debug)] struct TagC;
#[derive(Component, Clone, Copy, Debug)] struct TagD;

// ═══════════════════════════════════════════════════════════════════════════════
// РЕСУРСЫ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
struct DeltaTime(f32);

#[derive(Clone, Debug)]
struct Gravity(f32);

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct GlobalCounter(u64);

// ═══════════════════════════════════════════════════════════════════════════════
// СОБЫТИЯ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct DamageEvent { target: Entity, amount: f32 }

// ═══════════════════════════════════════════════════════════════════════════════
// СИСТЕМЫ
// ═══════════════════════════════════════════════════════════════════════════════

system! {
    fn movement_reader_system(
        q: (Read<Position>, Read<Velocity>),
        dt: Res<DeltaTime>,
    ) {
        let mut sum = 0.0f32;
        q.for_each(|_, (pos, vel)| {
            sum += pos.x + vel.x * dt.0;
        });
        std::hint::black_box(sum);
    }
}

system! {
    fn health_reader_system(
        q: Read<Health>,
    ) {
        let mut dead = 0u32;
        q.for_each(|_, hp| {
            if hp.current <= 0.0 { dead += 1; }
        });
        std::hint::black_box(dead);
    }
}

system! {
    fn physics_reader_system(
        q: (Read<Mass>, Read<Acceleration>),
        g: Res<Gravity>,
    ) {
        let mut force_sum = 0.0f32;
        q.for_each(|_, (m, a)| {
            force_sum += m.0 * (a.y + g.0);
        });
        std::hint::black_box(force_sum);
    }
}

system! {
    fn movement_writer_system(
        q: (Write<Position>, Read<Velocity>),
        dt: Res<DeltaTime>,
    ) {
        q.for_each_mut(|_, (mut pos, vel)| {
            pos.x += vel.x * dt.0;
            pos.y += vel.y * dt.0;
            pos.z += vel.z * dt.0;
        });
    }
}

system! {
    fn movement_writer_system2(
        q: (Write<Position>, Read<Acceleration>),
        dt: Res<DeltaTime>,
    ) {
        q.for_each_mut(|_, (mut pos, acc)| {
            pos.x += acc.x * dt.0 * dt.0 * 0.5;
            pos.y += acc.y * dt.0 * dt.0 * 0.5;
        });
    }
}

system! {
    fn health_writer_system(
        q: (Write<Health>, Read<Damage>),
        writer: &mut Vec<DamageEvent>,
    ) {
        let n = q.len();
        writer.reserve(n / 10 + 1);
        q.for_each_mut(|entity, (mut hp, dmg)| {
            hp.current -= dmg.0;
            if hp.current < 0.0 {
                writer.send(DamageEvent { target: entity, amount: dmg.0 });
            }
        });
    }
}

system! {
    fn damage_listener_system(
        q: Read<Health>,
        reader: &[DamageEvent],
    ) {
        let count = reader.iter().len() as u32;
        std::hint::black_box(count);
    }
}

system! {
    fn counter_writer_system(
        q: Read<Health>,
        counter: ResMut<GlobalCounter>,
    ) {
        q.for_each(|_, _| { counter.0 += 1; });
    }
}

system! {
    fn cooldown_system(
        q: Write<Cooldown>,
        dt: Res<DeltaTime>,
    ) {
        q.for_each_mut(|_, mut cd| {
            if cd.0 > 0.0 { cd.0 -= dt.0; }
        });
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ПРИМЕР: демонстрация флага par_for_each_used()
// ═══════════════════════════════════════════════════════════════════════════════
//
// Система, добавляемая через par_access() с флагом .par_for_each_used().
// Планировщик не будет чанковать её через ASD (избегает oversubscribe).
// Используйте этот флаг когда ваша система внутренне вызывает par_for_each.

#[allow(dead_code)]
fn register_par_for_each_demo(sched: &mut Scheduler) {
    use apex_core::access_desc;
    sched.add_systems(
        StageLabel::Update,
        par_access(
            "demo_intra_par",
            access_desc!(write<Position>, read<Velocity>).par_for_each_used(),
            |ctx| {
                ctx.query_unchecked::<(Read<Velocity>, Write<Position>)>()
                    .par_for_each_mut(|_, (vel, mut pos)| {
                        pos.x += vel.x * 0.016;
                        pos.y += vel.y * 0.016;
                    });
            },
        ),
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// УТИЛИТЫ ИЗМЕРЕНИЯ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
#[allow(dead_code)]
struct TimedResult {
    label:    String,
    duration: Duration,
    entities: usize,
    stages:   usize,
}

impl TimedResult {
    fn throughput_meps(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs == 0.0 { return 0.0; }
        (self.entities as f64) / secs / 1_000_000.0
    }
}

fn measure_scheduler(
    label: &str,
    world: &mut World,
    sched: &mut Scheduler,
    warmup_ticks: usize,
    measure_ticks: usize,
) -> TimedResult {
    for _ in 0..warmup_ticks {
        world.tick();
        sched.run(world);
    }

    let entity_count = world.entity_count();
    let stage_count  = sched.stages().map(|s| s.len()).unwrap_or(0);

    let t0 = Instant::now();
    for _ in 0..measure_ticks {
        world.tick();
        sched.run(world);
    }
    let elapsed = t0.elapsed() / measure_ticks as u32;

    TimedResult {
        label:    label.to_string(),
        duration: elapsed,
        entities: entity_count,
        stages:   stage_count,
    }
}

fn register_resources_and_events(world: &mut World) {
    world.insert_resource(DeltaTime(0.016));
    world.insert_resource(Gravity(-9.81));
    world.insert_resource(GlobalCounter(0));
    world.add_event::<DamageEvent>();
}

// ═══════════════════════════════════════════════════════════════════════════════
// ВСПОМОГАТЕЛЬНЫЕ УТИЛИТЫ ДЛЯ ВЫВОДА
// ═══════════════════════════════════════════════════════════════════════════════

fn fmt_dur(us: f64) -> String {
    if us >= 1000.0 { format!("{:>7.2}ms", us / 1000.0) }
    else { format!("{:>7.1}µs", us) }
}

fn fmt_meps(meps: f64) -> String {
    format!("{:>7.2}", meps)
}

/// Напечатать строку таблицы для SEQ vs PAR сравнения
fn table_row_seqpar(
    n: usize, seq_us: f64, par_us: f64,
    seq_meps: f64, par_meps: f64, speedup: f64, stages: usize,
) {
    let speedup_str = if speedup >= 1.0 {
        format!("{:>7.2}x ✓", speedup)
    } else {
        format!("{:>7.2}x ⚠", speedup)
    };
    println!("│ {:>5} │ {} │ {} │ {} │ {} │ {} │ {:>8} │",
        n,
        fmt_dur(seq_us),
        fmt_dur(par_us),
        fmt_meps(seq_meps),
        fmt_meps(par_meps),
        speedup_str,
        stages,
    );
}

/// Напечатать строку таблицы для произвольных результатов
fn table_row(label: &str, n: usize, us: f64, meps: f64, stages: usize) {
    println!("  {:30} │ {:>5} │ {} │ {} │ {:>2}",
        label, n, fmt_dur(us), fmt_meps(meps), stages);
}

#[allow(dead_code)]
fn table_sep() {
    println!("├{:─^7}┼{:─^12}┼{:─^12}┼{:─^12}┼{:─^12}┼{:─^12}┼{:─^10}┤",
        "", "", "", "", "", "", "");
}

// ═══════════════════════════════════════════════════════════════════════════════
// СЦЕНАРИИ
// ═══════════════════════════════════════════════════════════════════════════════

const ENTITY_COUNTS: &[usize] = &[100, 500, 1_000, 5_000, 10_000, 25_000, 50_000, 100_000, 200_000];

// ── 1. SEQ vs PAR: ideal_parallel ──────────────────────────────────────────

struct IdealParResult {
    n:    usize,
    seq:  TimedResult,
    par:  TimedResult,
}

fn run_ideal_parallel() -> Vec<IdealParResult> {
    let mut results = Vec::new();
    println!("\n═══ СЦЕНАРИЙ 1: SEQ vs PAR — 3 независимые системы ═══");
    for &n in ENTITY_COUNTS {
        print!("  n={}...", n);

        let build_world = |n| {
            let mut world = World::new();
            register_resources_and_events(&mut world);
            for i in 0..n {
                let f = i as f32;
                world.spawn((
                    Position { x: f, y: f * 0.1, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                    Health { current: 100.0, max: 100.0 },
                    Mass(1.0 + f * 0.001),
                    Acceleration { x: 0.0, y: -9.81, z: 0.0 },
                ));
            }
            world
        };

        let ticks_measure = if n < 1000 { 100 } else if n < 10000 { 50 } else { 20 };

        // Sequential
        let mut ws = build_world(n);
        let mut ss = Scheduler::new();
        ss.add_systems(StageLabel::Update, sys("movement_reader", movement_reader_system));
        ss.add_systems(StageLabel::Update, sys("health_reader", health_reader_system));
        ss.add_systems(StageLabel::Update, sys("physics_reader", physics_reader_system));
        ss.compile_with_world(&ws).expect("compile");
        let warmup = ticks_measure / 5;
        for _ in 0..warmup { ws.tick(); ss.run_sequential(&mut ws); }
        let ec = ws.entity_count();
        let sc = ss.stages().map(|s| s.len()).unwrap_or(0);
        let t0 = Instant::now();
        for _ in 0..ticks_measure { ws.tick(); ss.run_sequential(&mut ws); }
        let t_seq = t0.elapsed() / ticks_measure as u32;
        let seq = TimedResult {
            label: "seq".into(), duration: t_seq, entities: ec, stages: sc,
        };

        // Parallel
        let mut wp = build_world(n);
        let mut sp = Scheduler::new();
        sp.add_systems(StageLabel::Update, sys("movement_reader", movement_reader_system));
        sp.add_systems(StageLabel::Update, sys("health_reader", health_reader_system));
        sp.add_systems(StageLabel::Update, sys("physics_reader", physics_reader_system));
        sp.compile_with_world(&wp).expect("compile");
        let par = measure_scheduler("par", &mut wp, &mut sp, warmup, ticks_measure);

        results.push(IdealParResult { n, seq, par: par.clone() });
        println!(" SEQ {:>7.1}µs | PAR {:>7.1}µs | {:.2}x",
            t_seq.as_micros() as f64 / ticks_measure as f64,
            par.duration.as_micros() as f64 / ticks_measure as f64,
            t_seq.as_secs_f64() / par.duration.as_secs_f64().max(1e-12));
    }
    results
}

// ── 2. Внутрисистемный параллелизм ─────────────────────────────────────────

struct IntraSysResult {
    n:         usize,
    seq_one:   TimedResult,
    par_one:   TimedResult,
    seq_four:  TimedResult,
    par_four:  TimedResult,
}

/// Тест: 1 система MovementWriter на 1 vs 4 архетипах
/// Показывает, насколько хорошо ASD режет одну систему на чанки
/// и как влияет количество архетипов на распределение.
fn run_intra_system_parallel() -> Vec<IntraSysResult> {
    let mut results = Vec::new();
    println!("\n═══ СЦЕНАРИЙ 2: Внутрисистемный параллелизм (1 система, 1 vs 4 архетипа) ═══");
    for &n in ENTITY_COUNTS {
        print!("  n={}...", n);

        let ticks_measure = if n < 1000 { 100 } else if n < 10000 { 50 } else { 20 };
        let warmup = ticks_measure / 5;
        let e_per_arch = n / 4;

        let seq_one;
        let par_one;
        // 1 архетип
        {
            let mut world = World::new();
            register_resources_and_events(&mut world);
            for i in 0..n {
                let f = i as f32;
                world.spawn((
                    Position { x: f, y: 0.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                ));
            }
            // SEQ
            let mut s = Scheduler::new();
            s.add_systems(StageLabel::Update, sys("movement", movement_writer_system));
            s.compile_with_world(&world).expect("compile");
            for _ in 0..warmup { world.tick(); s.run_sequential(&mut world); }
            let ec = world.entity_count();
            let sc = s.stages().map(|s| s.len()).unwrap_or(0);
            let t0 = Instant::now();
            for _ in 0..ticks_measure { world.tick(); s.run_sequential(&mut world); }
            seq_one = TimedResult {
                label: "1arch_seq".into(), duration: t0.elapsed() / ticks_measure as u32,
                entities: ec, stages: sc,
            };
            // PAR
            let mut world2 = World::new();
            register_resources_and_events(&mut world2);
            for i in 0..n {
                let f = i as f32;
                world2.spawn((
                    Position { x: f, y: 0.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.0, z: 0.0 },
                ));
            }
            let mut sp = Scheduler::new();
            sp.add_systems(StageLabel::Update, sys("movement", movement_writer_system));
            sp.compile_with_world(&world2).expect("compile");
            par_one = measure_scheduler("1arch_par", &mut world2, &mut sp, warmup, ticks_measure);
        }

        let seq_four;
        let par_four;
        // 4 архетипа
        {
            let mut world = World::new();
            register_resources_and_events(&mut world);
            for i in 0..e_per_arch {
                let f = i as f32;
                world.spawn((Position { x: f, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagA));
            }
            for i in 0..e_per_arch {
                let f = i as f32;
                world.spawn((Position { x: f, y: 1.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagB));
            }
            for i in 0..e_per_arch {
                let f = i as f32;
                world.spawn((Position { x: f, y: 2.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagC));
            }
            for i in 0..e_per_arch {
                let f = i as f32;
                world.spawn((Position { x: f, y: 3.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagD));
            }
            // SEQ
            let mut s = Scheduler::new();
            s.add_systems(StageLabel::Update, sys("movement", movement_writer_system));
            s.compile_with_world(&world).expect("compile");
            for _ in 0..warmup { world.tick(); s.run_sequential(&mut world); }
            let ec = world.entity_count();
            let sc = s.stages().map(|s| s.len()).unwrap_or(0);
            let t0 = Instant::now();
            for _ in 0..ticks_measure { world.tick(); s.run_sequential(&mut world); }
            seq_four = TimedResult {
                label: "4arch_seq".into(), duration: t0.elapsed() / ticks_measure as u32,
                entities: ec, stages: sc,
            };
            // PAR
            let mut world2 = World::new();
            register_resources_and_events(&mut world2);
            for i in 0..e_per_arch {
                let f = i as f32;
                world2.spawn((Position { x: f, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagA));
            }
            for i in 0..e_per_arch {
                let f = i as f32;
                world2.spawn((Position { x: f, y: 1.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagB));
            }
            for i in 0..e_per_arch {
                let f = i as f32;
                world2.spawn((Position { x: f, y: 2.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagC));
            }
            for i in 0..e_per_arch {
                let f = i as f32;
                world2.spawn((Position { x: f, y: 3.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagD));
            }
            let mut sp = Scheduler::new();
            sp.add_systems(StageLabel::Update, sys("movement", movement_writer_system));
            sp.compile_with_world(&world2).expect("compile");
            par_four = measure_scheduler("4arch_par", &mut world2, &mut sp, warmup, ticks_measure);
        }

        results.push(IntraSysResult { n, seq_one: seq_one.clone(), par_one: par_one.clone(), seq_four: seq_four.clone(), par_four: par_four.clone() });
        println!(" 1arch SEQ {:>7.1}µs / PAR {:>7.1}µs | 4arch SEQ {:>7.1}µs / PAR {:>7.1}µs",
            seq_one.duration.as_micros() as f64,
            par_one.duration.as_micros() as f64,
            seq_four.duration.as_micros() as f64,
            par_four.duration.as_micros() as f64,
        );
    }
    results
}

// ── 3. Event pipeline ──────────────────────────────────────────────────────

struct EventResult {
    n:   usize,
    res: TimedResult,
}

fn run_event_pipeline() -> Vec<EventResult> {
    let mut results = Vec::new();
    println!("\n═══ СЦЕНАРИЙ 3: Event pipeline (Emit→Listen) ═══");
    for &n in ENTITY_COUNTS {
        print!("  n={}...", n);

        let ticks_measure = if n < 1000 { 100 } else if n < 10000 { 50 } else { 20 };
        let warmup = ticks_measure / 5;

        let mut world = World::new();

        register_resources_and_events(&mut world);
        for i in 0..n {
            let f = i as f32;
            world.spawn((
                Health { current: 50.0 - (i % 30) as f32, max: 100.0 },
                Damage(1.0 + f * 0.001),
            ));
        }

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, (
            sys("health_writer", health_writer_system),
            sys("damage_listener", damage_listener_system),
        ));
        sched.chain(&["health_writer", "damage_listener"]).expect("chain");
        sched.compile_with_world(&world).expect("compile");

        let res = measure_scheduler("event", &mut world, &mut sched, warmup, ticks_measure);
        results.push(EventResult { n, res: res.clone() });
        println!(" {}µs", res.duration.as_micros());
    }
    results
}

// ── 4. Full pipeline ───────────────────────────────────────────────────────

struct FullPipeResult {
    n:   usize,
    res: TimedResult,
}

fn run_full_pipeline() -> Vec<FullPipeResult> {
    let mut results = Vec::new();
    println!("\n═══ СЦЕНАРИЙ 4: Полный пайплайн (6 систем) ═══");
    for &n in ENTITY_COUNTS {
        print!("  n={}...", n);

        let ticks_measure = if n < 1000 { 100 } else if n < 10000 { 50 } else { 20 };
        let warmup = ticks_measure / 5;

        let mut world = World::new();

        register_resources_and_events(&mut world);

        for i in 0..n {
            let f = i as f32;
            world.spawn((
                Position { x: f, y: f * 0.1, z: 0.0 },
                Velocity { x: 1.0, y: 0.5, z: 0.0 },
                Health { current: 100.0 - (i % 50) as f32, max: 100.0 },
                Mass(1.0),
                Acceleration { x: 0.0, y: -9.81, z: 0.0 },
                Damage(0.5),
                Cooldown(0.1),
            ));
        }

        let mut sched = Scheduler::new();
        sched.add_systems(StageLabel::Update, sys("movement_reader", movement_reader_system));
        sched.add_systems(StageLabel::Update, sys("health_reader", health_reader_system));
        sched.add_systems(StageLabel::Update, sys("physics_reader", physics_reader_system));
        sched.add_systems(StageLabel::Update, (
            sys("health_writer", health_writer_system),
            sys("cooldown", cooldown_system),
            sys("damage_listener", damage_listener_system),
        ));
        sched.chain(&["health_writer", "damage_listener"]).expect("chain");
        sched.compile_with_world(&world).expect("compile");

        let res = measure_scheduler("full", &mut world, &mut sched, warmup, ticks_measure);
        results.push(FullPipeResult { n, res: res.clone() });
        println!(" {}µs", res.duration.as_micros());
    }
    results
}

// ═══════════════════════════════════════════════════════════════════════════════
// ВЫВОД СВОДНЫХ ТАБЛИЦ
// ═══════════════════════════════════════════════════════════════════════════════

fn print_seqpar_table(results: &[IdealParResult]) {
    println!("\n┌──────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│           SEQ vs PAR — 3 независимые системы                                        │");
    println!("├{:─^7}┬{:─^12}┬{:─^12}┬{:─^12}┬{:─^12}┬{:─^12}┬{:─^10}┤",
        "", "", "", "", "", "", "");
    println!("│ {:>5} │ {:>10} │ {:>10} │ {:>10} │ {:>10} │ {:>10} │ {:>8} │",
        "N", "SEQ,µs", "PAR,µs", "Meps SEQ", "Meps PAR", "Speedup", "Stages");
    for r in results {
        let par_us = r.par.duration.as_micros() as f64;
        let seq_meps = r.seq.throughput_meps();
        let par_meps = r.par.throughput_meps();
        let speedup = r.seq.duration.as_secs_f64() / r.par.duration.as_secs_f64().max(1e-12);
        table_row_seqpar(r.n, r.seq.duration.as_micros() as f64, par_us, seq_meps, par_meps, speedup, r.seq.stages);
    }
    println!("└{:─^7}┴{:─^12}┴{:─^12}┴{:─^12}┴{:─^12}┴{:─^12}┴{:─^10}┘",
        "", "", "", "", "", "", "");
}

fn print_intrasys_table(results: &[IntraSysResult]) {
    println!("\n┌──────────────────────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│           Внутрисистемный параллелизм — 1 система MovementWriter                                     │");
    println!("├{:─^7}┬{:─^14}┬{:─^14}┬{:─^14}┬{:─^14}┬{:─^14}┬{:─^14}┤",
        "", "", "", "", "", "", "");
    println!("│ {:>5} │ {:>12} │ {:>12} │ {:>12} │ {:>12} │ {:>12} │ {:>12} │",
        "N", "1arch SEQ", "1arch PAR", "1arch Spd", "4arch SEQ", "4arch PAR", "4arch Spd");
    println!("│ {:>5} │ {:>12} │ {:>12} │ {:>12} │ {:>12} │ {:>12} │ {:>12} │",
        "", "µs", "µs", "", "µs", "µs", "");
    for r in results {
        let one_speedup = r.seq_one.duration.as_secs_f64() / r.par_one.duration.as_secs_f64().max(1e-12);
        let four_speedup = r.seq_four.duration.as_secs_f64() / r.par_four.duration.as_secs_f64().max(1e-12);
        println!("│ {:>5} │ {:>10.1}µs │ {:>10.1}µs │ {:>9.2}x │ {:>10.1}µs │ {:>10.1}µs │ {:>9.2}x │",
            r.n,
            r.seq_one.duration.as_micros() as f64,
            r.par_one.duration.as_micros() as f64,
            one_speedup,
            r.seq_four.duration.as_micros() as f64,
            r.par_four.duration.as_micros() as f64,
            four_speedup,
        );
    }
    println!("└{:─^7}┴{:─^14}┴{:─^14}┴{:─^14}┴{:─^14}┴{:─^14}┴{:─^14}┘",
        "", "", "", "", "", "", "");
}

fn print_event_table(results: &[EventResult]) {
    println!("\n┌─────────────────────────────────────────────────────────────────┐");
    println!("│           Event pipeline (Emit→Listen)                           │");
    println!("├{:─^7}┬{:─^12}┬{:─^12}┬{:─^12}┤",
        "", "", "", "");
    println!("│ {:>5} │ {:>10} │ {:>10} │ {:>10} │", "N", "µs", "Meps", "Stages");
    for r in results {
        table_row("event", r.n, r.res.duration.as_micros() as f64, r.res.throughput_meps(), r.res.stages);
    }
}

fn print_fullpipe_table(results: &[FullPipeResult]) {
    println!("\n┌─────────────────────────────────────────────────────────────────┐");
    println!("│           Полный пайплайн (6 систем)                              │");
    println!("├{:─^7}┬{:─^12}┬{:─^12}┬{:─^12}┤",
        "", "", "", "");
    println!("│ {:>5} │ {:>10} │ {:>10} │ {:>10} │", "N", "µs", "Meps", "Stages");
    for r in results {
        table_row("full", r.n, r.res.duration.as_micros() as f64, r.res.throughput_meps(), r.res.stages);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════════

fn main() {
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║     APEX ECS — Диагностика параллелизма (scaling benchmark)     ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Ядер: {} | Режим: {} | Фича parallel: ✅",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        if cfg!(debug_assertions) { "DEBUG" } else { "RELEASE" },
    );
    println!("  Entity counts: {:?}", ENTITY_COUNTS);
    println!("  {}", "─".repeat(90));

    // ── ЗАМЕРЫ ─────────────────────────────────────────────────────
    let t_all = Instant::now();

    let r1 = run_ideal_parallel();
    let r2 = run_intra_system_parallel();
    let r3 = run_event_pipeline();
    let r4 = run_full_pipeline();

    let elapsed_all = t_all.elapsed();

    // ── ВЫВОД ──────────────────────────────────────────────────────
    println!("\n{}", "═".repeat(90));
    println!("  СВОДНЫЕ ТАБЛИЦЫ");
    println!("{}", "═".repeat(90));

    print_seqpar_table(&r1);
    print_intrasys_table(&r2);
    print_event_table(&r3);
    print_fullpipe_table(&r4);

    // ── Анализ ─────────────────────────────────────────────────────
    println!("\n{}", "═".repeat(90));
    println!("  АНАЛИЗ");
    println!("{}", "═".repeat(90));

    // Время переключения между режимами (cross-over point)
    println!("\n  ● Точка пересечения SEQ/PAR:");
    for r in &r1 {
        let speedup = r.seq.duration.as_secs_f64() / r.par.duration.as_secs_f64().max(1e-12);
        let status = if speedup >= 1.5 { "✅ PAR быстрее" }
                     else if speedup >= 1.0 { "ℹ️  слабое ускорение" }
                     else { "⚠️  PAR медленнее" };
        println!("    n={:>7}: speedup {:>5.2}x — {}", r.n, speedup, status);
    }

    // Сравнение 1 vs 4 архетипов
    println!("\n  ● Фрагментация: 1 архетип vs 4 (SEQ):");
    for r in &r2 {
        let single = r.seq_one.duration.as_micros() as f64;
        let four   = r.seq_four.duration.as_micros() as f64;
        let overhead = (four / single.max(1.0) - 1.0) * 100.0;
        println!("    n={:>7}: 1arch {:>8.1}µs  4arch {:>8.1}µs  overhead {:>+6.1}%",
            r.n, single, four, overhead);
    }

    // Параллельный speedup 1arch vs 4arch
    println!("\n  ● Внутрисистемный PAR speedup (1arch vs 4arch):");
    for r in &r2 {
        let one_par_sp = r.seq_one.duration.as_secs_f64() / r.par_one.duration.as_secs_f64().max(1e-12);
        let four_par_sp = r.seq_four.duration.as_secs_f64() / r.par_four.duration.as_secs_f64().max(1e-12);
        println!("    n={:>7}: 1arch {:>5.2}x  4arch {:>5.2}x", r.n, one_par_sp, four_par_sp);
    }

    println!("\n{}", "═".repeat(90));
    println!("  Все замеры выполнены за {:.1}s", elapsed_all.as_secs_f64());
    println!("{}", "═".repeat(90));
}
