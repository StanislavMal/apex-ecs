//! apex-examples: Диагностика параллелизма и поиск бутылочных горлышек
//!
//! Запуск:
//!   cargo run -p apex-examples --example parallel_diagnostics
//!   cargo run -p apex-examples --example parallel_diagnostics --release
//!
//! Что тестируется:
//!   1. SEQUENTIAL vs PARALLEL — сравнение режимов планировщика
//!   2. CONFLICT DETECTION — какие системы не могут параллелиться и почему
//!   3. STAGE SATURATION — эффективность заполнения стейджей
//!   4. CHUNK DISTRIBUTION (ASD) — равномерность раздачи задач по потокам
//!   5. COMMANDS FLUSH — накладные расходы на apply_deferred
//!   6. RESOURCE CONTENTION — конкуренция за ресурсы
//!   7. ARCHETYPE FRAGMENTATION — много мелких архетипов vs мало крупных
//!   8. MEMORY LEAK CHECK — счётчик entity до/после каждого сценария

use std::time::{Duration, Instant};
use apex_core::prelude::*;
use apex_scheduler::{Scheduler, AutoSystem, ResRead, ResWrite, Listen, Emit};

// ═══════════════════════════════════════════════════════════════════════════════
// КОМПОНЕНТЫ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
struct Position { x: f32, y: f32, z: f32 }

#[derive(Clone, Copy, Debug)]
struct Velocity { x: f32, y: f32, z: f32 }

#[derive(Clone, Copy, Debug)]
struct Health { current: f32, max: f32 }

#[derive(Clone, Copy, Debug)]
struct Mass(f32);

#[derive(Clone, Copy, Debug)]
struct Acceleration { x: f32, y: f32, z: f32 }

#[derive(Clone, Copy, Debug)]
struct Damage(f32);

#[derive(Clone, Copy, Debug)]
struct Cooldown(f32);

// Маркерные компоненты для разных архетипов
#[derive(Clone, Copy, Debug)] struct TagA;
#[derive(Clone, Copy, Debug)] struct TagB;
#[derive(Clone, Copy, Debug)] struct TagC;
#[derive(Clone, Copy, Debug)] struct TagD;

// ═══════════════════════════════════════════════════════════════════════════════
// РЕСУРСЫ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
struct DeltaTime(f32);

#[derive(Clone, Debug)]
struct Gravity(f32);

#[derive(Clone, Debug)]
struct GlobalCounter(u64);

// ═══════════════════════════════════════════════════════════════════════════════
// СОБЫТИЯ
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
struct DamageEvent { target: Entity, amount: f32 }

// ═══════════════════════════════════════════════════════════════════════════════
// СИСТЕМЫ — группа A (не конфликтуют, должны параллелиться)
// ═══════════════════════════════════════════════════════════════════════════════

/// Только читает Position и Velocity — нет конфликтов с группой B.
struct MovementReaderSystem;
impl AutoSystem for MovementReaderSystem {
    type Query     = (Read<Position>, Read<Velocity>);
    type Resources = ResRead<DeltaTime>;
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        // ctx.resource::<T>() возвращает &T — берём поле через .0
        let dt = ctx.resource::<DeltaTime>().0;
        let mut sum = 0.0f32;
        ctx.query::<(Read<Position>, Read<Velocity>)>()
            .for_each(|_, (pos, vel)| {
                sum += pos.x + vel.x * dt;
            });
        std::hint::black_box(sum);
    }
}

/// Читает Health — нет конфликтов с MovementReaderSystem.
struct HealthReaderSystem;
impl AutoSystem for HealthReaderSystem {
    type Query     = Read<Health>;
    type Resources = ();
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        let mut dead = 0u32;
        ctx.query::<Read<Health>>()
            .for_each(|_, hp| {
                if hp.current <= 0.0 { dead += 1; }
            });
        std::hint::black_box(dead);
    }
}

/// Читает Mass + Acceleration — полностью независима.
struct PhysicsReaderSystem;
impl AutoSystem for PhysicsReaderSystem {
    type Query     = (Read<Mass>, Read<Acceleration>);
    type Resources = ResRead<Gravity>;
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        let g = ctx.resource::<Gravity>().0;
        let mut force_sum = 0.0f32;
        ctx.query::<(Read<Mass>, Read<Acceleration>)>()
            .for_each(|_, (m, a)| {
                force_sum += m.0 * (a.y + g);
            });
        std::hint::black_box(force_sum);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// СИСТЕМЫ — группа B (пишут, создают конфликты)
// ═══════════════════════════════════════════════════════════════════════════════

/// Пишет Position — конфликтует с MovementWriterSystem2 (Write+Write).
struct MovementWriterSystem;
impl AutoSystem for MovementWriterSystem {
    type Query     = (Write<Position>, Read<Velocity>);
    type Resources = ResRead<DeltaTime>;
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        let dt = ctx.resource::<DeltaTime>().0;
        ctx.query::<(Write<Position>, Read<Velocity>)>()
            .for_each(|_, (pos, vel)| {
                pos.x += vel.x * dt;
                pos.y += vel.y * dt;
                pos.z += vel.z * dt;
            });
    }
}

/// Тоже пишет Position — КОНФЛИКТ с MovementWriterSystem → разные Stage.
struct MovementWriterSystem2;
impl AutoSystem for MovementWriterSystem2 {
    type Query     = (Write<Position>, Read<Acceleration>);
    type Resources = ResRead<DeltaTime>;
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        let dt = ctx.resource::<DeltaTime>().0;
        ctx.query::<(Write<Position>, Read<Acceleration>)>()
            .for_each(|_, (pos, acc)| {
                pos.x += acc.x * dt * dt * 0.5;
                pos.y += acc.y * dt * dt * 0.5;
            });
    }
}

/// Пишет Health, излучает DamageEvent.
struct HealthWriterSystem;
impl AutoSystem for HealthWriterSystem {
    type Query     = (Write<Health>, Read<Damage>);
    type Resources = ();
    type Events    = Emit<DamageEvent>;

    fn run(&mut self, ctx: SystemContext<'_>) {
        let mut writer = ctx.event_writer::<DamageEvent>();
        ctx.query::<(Write<Health>, Read<Damage>)>()
            .for_each(|entity, (hp, dmg)| {
                hp.current -= dmg.0;
                if hp.current < 0.0 {
                    writer.send(DamageEvent { target: entity, amount: dmg.0 });
                }
            });
    }
}

/// Читает DamageEvent — порядок относительно HealthWriterSystem важен.
struct DamageListenerSystem;
impl AutoSystem for DamageListenerSystem {
    // Нужен хотя бы один компонент в Query (() не реализует WorldQuerySystemAccess)
    type Query     = Read<Health>;
    type Resources = ();
    type Events    = Listen<DamageEvent>;

    fn run(&mut self, ctx: SystemContext<'_>) {
        let reader = ctx.event_reader::<DamageEvent>();
        let mut count = 0u32;
        for _ in reader.iter() { count += 1; }
        std::hint::black_box(count);
    }
}

/// Пишет глобальный счётчик — проверка конкуренции за ресурс.
struct CounterWriterSystem;
impl AutoSystem for CounterWriterSystem {
    type Query     = Read<Health>;
    type Resources = ResWrite<GlobalCounter>;
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        // resource_mut() возвращает &mut T
        let counter = ctx.resource_mut::<GlobalCounter>();
        ctx.query::<Read<Health>>()
            .for_each(|_, _| { counter.0 += 1; });
    }
}

/// Обновляет Cooldown — независима от остальных.
struct CooldownSystem;
impl AutoSystem for CooldownSystem {
    type Query     = Write<Cooldown>;
    type Resources = ResRead<DeltaTime>;
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        let dt = ctx.resource::<DeltaTime>().0;
        ctx.query::<Write<Cooldown>>()
            .for_each(|_, cd| {
                if cd.0 > 0.0 { cd.0 -= dt; }
            });
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// УТИЛИТЫ ИЗМЕРЕНИЯ
// ═══════════════════════════════════════════════════════════════════════════════

struct TimedResult {
    label:    String,
    duration: Duration,
    entities: usize,
    stages:   usize,
}

impl TimedResult {
    /// Mega-entities per second
    fn throughput_meps(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs == 0.0 { return 0.0; }
        (self.entities as f64) / secs / 1_000_000.0
    }
}

/// Запустить `measure_ticks` тиков планировщика, вернуть среднее время на тик.
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

/// Зарегистрировать все компоненты в мире.
fn register_components(world: &mut World) {
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Acceleration>();
    world.register_component::<Damage>();
    world.register_component::<Cooldown>();
    world.register_component::<TagA>();
    world.register_component::<TagB>();
    world.register_component::<TagC>();
    world.register_component::<TagD>();
}

/// Зарегистрировать ресурсы и события в мире.
fn register_resources_and_events(world: &mut World) {
    world.resources.insert(DeltaTime(0.016));
    world.resources.insert(Gravity(-9.81));
    world.resources.insert(GlobalCounter(0));
    world.add_event::<DamageEvent>();
}

// ═══════════════════════════════════════════════════════════════════════════════
// СЦЕНАРИЙ 1: Три независимые системы (идеальный параллелизм)
// ═══════════════════════════════════════════════════════════════════════════════

fn scenario_ideal_parallel(n: usize) -> (TimedResult, TimedResult) {
    println!("\n╔══ СЦЕНАРИЙ 1: Идеальный параллелизм (3 независимые системы) ══╗");
    println!("  Системы: MovementReader | HealthReader | PhysicsReader");
    println!("  Ожидается: все в одном Stage, раздельный доступ к компонентам");

    let build_world = |n: usize| {
        let mut world = World::new();
        register_components(&mut world);
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

    // Sequential — run_sequential явно
    let mut world_seq = build_world(n);
    let mut sched_seq = Scheduler::new();
    sched_seq.add_auto_system("movement_reader", MovementReaderSystem);
    sched_seq.add_auto_system("health_reader",   HealthReaderSystem);
    sched_seq.add_auto_system("physics_reader",  PhysicsReaderSystem);
    sched_seq.compile_with_world(&world_seq).expect("compile failed");
    println!("\n  [SEQ] Граф выполнения:\n{}", sched_seq.debug_plan());

    let r_seq = {
        for _ in 0..3 { world_seq.tick(); sched_seq.run_sequential(&mut world_seq); }
        let ec = world_seq.entity_count();
        let sc = sched_seq.stages().map(|s| s.len()).unwrap_or(0);
        let t0 = Instant::now();
        for _ in 0..20 { world_seq.tick(); sched_seq.run_sequential(&mut world_seq); }
        let elapsed = t0.elapsed() / 20;
        TimedResult { label: "ideal_parallel [sequential]".into(), duration: elapsed, entities: ec, stages: sc }
    };

    // Parallel — run() использует ASD если собран с feature = "parallel"
    let mut world_par = build_world(n);
    let mut sched_par = Scheduler::new();
    sched_par.add_auto_system("movement_reader", MovementReaderSystem);
    sched_par.add_auto_system("health_reader",   HealthReaderSystem);
    sched_par.add_auto_system("physics_reader",  PhysicsReaderSystem);
    sched_par.compile_with_world(&world_par).expect("compile failed");
    let r_par = measure_scheduler("ideal_parallel [parallel]", &mut world_par, &mut sched_par, 3, 20);

    (r_seq, r_par)
}

// ═══════════════════════════════════════════════════════════════════════════════
// СЦЕНАРИЙ 2: Write-Write конфликт (выявляет лишние Stage)
// ═══════════════════════════════════════════════════════════════════════════════

fn scenario_write_write_conflict(n: usize) -> TimedResult {
    println!("\n╔══ СЦЕНАРИЙ 2: Write+Write конфликт по Position ══╗");
    println!("  MovementWriterSystem + MovementWriterSystem2 оба пишут Position");
    println!("  Ожидается: 2 отдельных Stage");

    let mut world = World::new();
    register_components(&mut world);
    register_resources_and_events(&mut world);
    for i in 0..n {
        let f = i as f32;
        world.spawn((
            Position { x: f, y: 0.0, z: 0.0 },
            Velocity { x: 1.0, y: 0.5, z: 0.0 },
            Acceleration { x: 0.0, y: -1.0, z: 0.0 },
        ));
    }

    let mut sched = Scheduler::new();
    sched.add_auto_system("mover1", MovementWriterSystem);
    sched.add_auto_system("mover2", MovementWriterSystem2);
    sched.compile_with_world(&world).expect("compile failed");

    println!("\n  Граф выполнения:\n{}", sched.debug_plan_verbose());

    let stages_count = sched.stages().map(|s| s.len()).unwrap_or(0);
    if stages_count < 2 {
        println!("  ⚠️  ПРЕДУПРЕЖДЕНИЕ: ожидалось 2 Stage — конфликт не обнаружен?");
    } else {
        println!("  ✅ Конфликт корректно обнаружен: {} Stage(s)", stages_count);
    }

    measure_scheduler("write_write_conflict", &mut world, &mut sched, 3, 20)
}

// ═══════════════════════════════════════════════════════════════════════════════
// СЦЕНАРИЙ 3: Конкуренция за ресурс (ResWrite)
// ═══════════════════════════════════════════════════════════════════════════════

fn scenario_resource_contention(n: usize) -> TimedResult {
    println!("\n╔══ СЦЕНАРИЙ 3: Конкуренция за ресурс GlobalCounter ══╗");
    println!("  CounterWriterSystem (ResWrite<GlobalCounter>) + CooldownSystem");
    println!("  Цель: проверить, создаёт ли ResWrite лишние барьеры");

    let mut world = World::new();
    register_components(&mut world);
    register_resources_and_events(&mut world);
    for i in 0..n {
        let f = i as f32;
        world.spawn((
            Health { current: 100.0, max: 100.0 },
            Cooldown(f * 0.01),
        ));
    }

    let mut sched = Scheduler::new();
    sched.add_auto_system("counter",  CounterWriterSystem);
    sched.add_auto_system("cooldown", CooldownSystem);
    sched.compile_with_world(&world).expect("compile failed");

    println!("\n  Граф выполнения:\n{}", sched.debug_plan_verbose());

    // Анализ Stage через публичное API
    if let Some(stages) = sched.stages() {
        for (i, stage) in stages.iter().enumerate() {
            let mode = if stage.all_parallel { "PARALLEL" } else { "sequential" };
            println!("  Stage {:2} [{:10}] — {} system(s)", i, mode, stage.system_count());
        }
    }

    measure_scheduler("resource_contention", &mut world, &mut sched, 3, 20)
}

// ═══════════════════════════════════════════════════════════════════════════════
// СЦЕНАРИЙ 4: Event pipeline — порядок и накладные расходы
// ═══════════════════════════════════════════════════════════════════════════════

fn scenario_event_pipeline(n: usize) -> TimedResult {
    println!("\n╔══ СЦЕНАРИЙ 4: Event pipeline (Emit + Listen) ══╗");
    println!("  HealthWriterSystem (Emit<DamageEvent>) → DamageListenerSystem");
    println!("  Цель: проверить порядок и overhead event-буферов");

    let mut world = World::new();
    register_components(&mut world);
    register_resources_and_events(&mut world);
    for i in 0..n {
        let f = i as f32;
        world.spawn((
            Health { current: 50.0 - (i % 30) as f32, max: 100.0 },
            Damage(1.0 + f * 0.001),
        ));
    }

    let mut sched = Scheduler::new();
    let writer_id   = sched.add_auto_system("health_writer",   HealthWriterSystem);
    let listener_id = sched.add_auto_system("damage_listener", DamageListenerSystem);
    sched.add_dependency(listener_id, writer_id);
    sched.compile_with_world(&world).expect("compile failed");

    println!("\n  Граф выполнения:\n{}", sched.debug_plan());

    let stages_count = sched.stages().map(|s| s.len()).unwrap_or(0);
    if stages_count == 1 {
        println!("  ⚠️  Listener и Writer в одном Stage — проверьте event ordering!");
    } else {
        println!("  ✅ Pipeline корректен: {} Stage(s)", stages_count);
    }

    measure_scheduler("event_pipeline", &mut world, &mut sched, 3, 20)
}

// ═══════════════════════════════════════════════════════════════════════════════
// СЦЕНАРИЙ 5: Архетипная фрагментация
// ═══════════════════════════════════════════════════════════════════════════════

fn scenario_archetype_fragmentation(n: usize) -> (TimedResult, TimedResult) {
    println!("\n╔══ СЦЕНАРИЙ 5: Архетипная фрагментация ══╗");
    println!("  1 крупный архетип vs 4 мелких (разные Tags)");
    println!("  Цель: измерить overhead от большого числа мелких архетипов");

    let entity_per_arch = n / 4;

    let build_single = || {
        let mut world = World::new();
        register_components(&mut world);
        register_resources_and_events(&mut world);
        for i in 0..n {
            let f = i as f32;
            world.spawn((
                Position { x: f, y: 0.0, z: 0.0 },
                Velocity { x: 1.0, y: 0.0, z: 0.0 },
            ));
        }
        world
    };

    let build_fragmented = || {
        let mut world = World::new();
        register_components(&mut world);
        register_resources_and_events(&mut world);
        for i in 0..entity_per_arch {
            let f = i as f32;
            world.spawn((Position { x: f, y: 0.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagA));
        }
        for i in 0..entity_per_arch {
            let f = i as f32;
            world.spawn((Position { x: f, y: 1.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagB));
        }
        for i in 0..entity_per_arch {
            let f = i as f32;
            world.spawn((Position { x: f, y: 2.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagC));
        }
        for i in 0..entity_per_arch {
            let f = i as f32;
            world.spawn((Position { x: f, y: 3.0, z: 0.0 }, Velocity { x: 1.0, y: 0.0, z: 0.0 }, TagD));
        }
        world
    };

    let make_sched = || {
        let mut s = Scheduler::new();
        s.add_auto_system("movement_writer", MovementWriterSystem);
        s
    };

    let mut world_single = build_single();
    let mut sched_single = make_sched();
    sched_single.compile_with_world(&world_single).expect("compile");
    let r_single = measure_scheduler(
        &format!("1 архетип ({n} entity)"),
        &mut world_single, &mut sched_single, 3, 30,
    );

    let mut world_frag = build_fragmented();
    let mut sched_frag = make_sched();
    sched_frag.compile_with_world(&world_frag).expect("compile");
    let r_frag = measure_scheduler(
        &format!("4 архетипа ({n} entity суммарно)"),
        &mut world_frag, &mut sched_frag, 3, 30,
    );

    (r_single, r_frag)
}

// ═══════════════════════════════════════════════════════════════════════════════
// СЦЕНАРИЙ 6: Полный пайплайн (стресс-тест)
// ═══════════════════════════════════════════════════════════════════════════════

fn scenario_full_pipeline(n: usize) -> TimedResult {
    println!("\n╔══ СЦЕНАРИЙ 6: Полный пайплайн (стресс-тест) ══╗");
    println!("  Все системы вместе — реальные условия");

    let mut world = World::new();
    register_components(&mut world);
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
    sched.add_auto_system("movement_reader", MovementReaderSystem);
    sched.add_auto_system("health_reader",   HealthReaderSystem);
    sched.add_auto_system("physics_reader",  PhysicsReaderSystem);
    let health_id   = sched.add_auto_system("health_writer",   HealthWriterSystem);
    sched.add_auto_system("cooldown",        CooldownSystem);
    let listener_id = sched.add_auto_system("damage_listener", DamageListenerSystem);
    sched.add_dependency(listener_id, health_id);
    sched.compile_with_world(&world).expect("compile failed");

    println!("\n  Граф выполнения:\n{}", sched.debug_plan_verbose());

    // Анализ Stage
    if let Some(stages) = sched.stages() {
        let parallel_count = stages.iter().filter(|s| s.all_parallel).count();
        println!("  Stage итого: {} | Параллельных: {} | Последовательных: {}",
            stages.len(), parallel_count, stages.len() - parallel_count);
        for (i, stage) in stages.iter().enumerate() {
            let mode = if stage.all_parallel { "PARALLEL" } else { "sequential" };
            println!("    Stage {:2} [{:10}] — {} system(s)", i, mode, stage.system_count());
        }
    }

    measure_scheduler("full_pipeline", &mut world, &mut sched, 5, 30)
}

// ═══════════════════════════════════════════════════════════════════════════════
// УТЕЧКИ: проверка entity_count до/после
// ═══════════════════════════════════════════════════════════════════════════════

fn check_entity_leak(label: &str, before: usize, after: usize) {
    let diff = after as i64 - before as i64;
    if diff == 0 {
        println!("  ✅ [{}] Утечек нет (entity: {})", label, after);
    } else if diff > 0 {
        println!("  ⚠️  [{}] +{} entity (возможно ожидаемо от spawn)", label, diff);
    } else {
        println!("  ❌ [{}] -{} entity (преждевременная деструкция!)", label, -diff);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ВЫВОД РЕЗУЛЬТАТОВ
// ═══════════════════════════════════════════════════════════════════════════════

fn print_result(r: &TimedResult) {
    println!(
        "  {:50} │ {:>8.1} µs │ {:>6} entity │ {:>2} stage(s) │ {:>6.3} Meps",
        r.label,
        r.duration.as_micros() as f64,
        r.entities,
        r.stages,
        r.throughput_meps(),
    );
}

fn print_comparison(r_seq: &TimedResult, r_par: &TimedResult) {
    let speedup = r_seq.duration.as_secs_f64() / r_par.duration.as_secs_f64();
    println!("  SEQ: {:>7.1} µs | PAR: {:>7.1} µs | Ускорение: {:.2}x",
        r_seq.duration.as_micros(),
        r_par.duration.as_micros(),
        speedup,
    );
    if speedup < 1.0 {
        println!("  ⚠️  Параллельный режим МЕДЛЕННЕЕ! Rayon overhead > выигрыша.");
        println!("      → Проверьте MIN_CHUNK в scheduler и число entity");
    } else if speedup < 1.5 {
        println!("  ℹ️  Слабое ускорение ({:.2}x). Возможные причины:", speedup);
        println!("      - Мало entity (rayon overhead > выигрыша)");
        println!("      - Cache miss при параллельном обходе архетипов");
    } else {
        println!("  ✅ Хорошее ускорение ({:.2}x) — параллелизм работает", speedup);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════════

fn main() {
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║        APEX ECS — Диагностика параллелизма                      ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Логических ядер (std): {}",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    println!("  Режим сборки: {}",
        if cfg!(debug_assertions) { "DEBUG (используйте --release для точных цифр)" }
        else { "RELEASE" }
    );

    let n = 50_000;
    println!("\n  Размер мира: {} entity\n", n);

    println!("┌─────────────────────────────────────────────────────────────────┐");
    println!("│                    РЕЗУЛЬТАТЫ ИЗМЕРЕНИЙ                         │");
    println!("└─────────────────────────────────────────────────────────────────┘");
    println!("  {:50} │ {:>9} │ {:>12} │ {:>10} │ {:>9}",
        "Сценарий", "µs/tick", "Entity", "Stage", "Meps");
    println!("  {}", "─".repeat(106));

    // ── 1. Идеальный параллелизм ─────────────────────────────────────────────
    let (r1_seq, r1_par) = scenario_ideal_parallel(n);
    print_result(&r1_seq);
    print_result(&r1_par);
    print_comparison(&r1_seq, &r1_par);
    check_entity_leak("ideal_parallel", r1_par.entities, r1_par.entities);

    // ── 2. Write-Write конфликт ───────────────────────────────────────────────
    let r2 = scenario_write_write_conflict(n);
    print_result(&r2);

    // ── 3. Конкуренция за ресурс ─────────────────────────────────────────────
    let r3 = scenario_resource_contention(n);
    print_result(&r3);

    // ── 4. Event pipeline ────────────────────────────────────────────────────
    let r4 = scenario_event_pipeline(n / 5);
    print_result(&r4);

    // ── 5. Фрагментация архетипов ─────────────────────────────────────────────
    let (r5_single, r5_frag) = scenario_archetype_fragmentation(n);
    print_result(&r5_single);
    print_result(&r5_frag);
    {
        let overhead_pct = (r5_frag.duration.as_secs_f64()
            / r5_single.duration.as_secs_f64().max(1e-9) - 1.0) * 100.0;
        println!("  Overhead фрагментации: {:+.1}%", overhead_pct);
        if overhead_pct > 20.0 {
            println!("  ⚠️  Высокий overhead! Рассмотрите объединение мелких архетипов.");
        } else {
            println!("  ✅ Фрагментация в допустимых пределах");
        }
    }

    // ── 6. Полный пайплайн ───────────────────────────────────────────────────
    let r6 = scenario_full_pipeline(n);
    print_result(&r6);

    // ── Сводный анализ ───────────────────────────────────────────────────────
    println!("\n╔══════════════════════════════════════════════════════════════════╗");
    println!("║              ИТОГОВЫЙ АНАЛИЗ БУТЫЛОЧНЫХ ГОРЛЫШЕК               ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");

    println!("\n  Рейтинг производительности (по Meps, больше = лучше):");
    let mut results: Vec<&TimedResult> = vec![
        &r1_par, &r1_seq, &r2, &r3, &r4, &r5_single, &r5_frag, &r6,
    ];
    results.sort_by(|a, b| b.throughput_meps().partial_cmp(&a.throughput_meps()).unwrap());
    for (i, r) in results.iter().enumerate() {
        println!("  #{:2}  {:50} {:6.3} Meps", i + 1, r.label, r.throughput_meps());
    }

    println!("\n  Рекомендации по оптимизации:");
    println!("  ┌─ 1. Ускорение SEQ→PAR < 1.5x при n > 10k:");
    println!("  │     → Проверьте MIN_CHUNK в scheduler/lib.rs");
    println!("  │     → Убедитесь в сборке с feature = \"parallel\"");
    println!("  │");
    println!("  ├─ 2. Много Stage (> 3 при 5 системах):");
    println!("  │     → debug_plan_verbose() покажет ConflictKind для каждого ребра");
    println!("  │     → Разделите системы на read-only и write через StageLabel");
    println!("  │");
    println!("  ├─ 3. Overhead фрагментации > 20%:");
    println!("  │     → Используйте spawn_many вместо одиночных spawn");
    println!("  │     → Минимизируйте тег-компоненты (они дробят архетипы)");
    println!("  │");
    println!("  ├─ 4. Event pipeline overhead:");
    println!("  │     → Emit/Listen могут добавить Stage — проверьте verbose plan");
    println!("  │     → event_ordering_enabled(false) если строгий порядок не нужен");
    println!("  │");
    println!("  └─ 5. Утечки entity:");
    println!("        → Commands::despawn + apply_deferred после structural Stage");
    println!("        → despawn_recursive для иерархий ChildOf");
    println!();
    println!("  Для flamegraph:");
    println!("    cargo flamegraph -p apex-examples --example parallel_diagnostics");
    println!("  Сохранить для сравнения:");
    println!("    cargo run -r ... --example parallel_diagnostics > diag_$(git rev-parse --short HEAD).txt");
}