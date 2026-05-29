//! Apex ECS — Event Pipeline Example
//!
//! Демонстрирует конвейерную обработку событий:
//! Producer → Transformer → [Consumer, Consumer (parallel)].
//!
//! Сценарий: CollisionSystem эмитирует DamageEvent → ArmorSystem применяет
//! броню (модифицирует Health напрямую) и перевыпускает событие →
//! HealthSystem читает Health (изменённый ArmorSystem в том же кадре) +
//! SoundSystem читает события.
//!
//! ## Как работает пайплайн с per-Stage flush (v0.1.0)
//!
//! Конвейер гарантирует порядок выполнения: collision → armor → [health, sound].
//! Scheduler автоматически флашит события после каждого Stage,
//! поэтому события, отправленные на Stage N, видны на Stage N+1 того же кадра.
//!
//! - ArmorSystem модифицирует Health (компонент) — изменения видны health
//!   в том же кадре.
//! - ArmorSystem перевыпускает DamageEvent → SoundSystem увидит его
//!   на следующем Stage того же кадра (per-stage flush).
//!
//! cargo run -p apex-examples --example event_pipeline --release

use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::{Scheduler, StageLabel, sys};

// ── Компоненты ─────────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)]
struct Collider;

#[derive(Component, Clone, Copy, Debug)]
struct Health {
    current: f32,
    max: f32,
}

#[derive(Component, Clone, Copy, Debug)]
struct Armor(f32);

// ── Событие ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
struct DamageEvent {
    target: Entity,
    amount: f32,
}

// ── Producer: CollisionSystem ──────────────────────────────────

system! {
    fn collision_system(
        q: Read<Collider>,
        writer: &mut Vec<DamageEvent>,
    ) {
        let count = q.len();
        for (entity, _) in q.iter() {
            writer.send(DamageEvent { target: entity, amount: 25.0 });
        }
        println!("  [CollisionSystem] emitted {}x DamageEvent(25.0)", count);
    }
}

// ── Transformer: ArmorSystem ───────────────────────────────────
// Читает DamageEvent, МОДИФИЦИРУЕТ Health (компонент), перевыпускает
// модифицированное событие для следующих этапов.

system! {
    fn armor_system(
        q: (Read<Armor>, Write<Health>),
        reader: &[DamageEvent],
        writer: &mut Vec<DamageEvent>,
    ) {
        let mut count = 0usize;

        for ev in reader.iter() {
            count += 1;

            let mut reduced = ev.amount;
            q.for_each(|entity, (armor, hp)| {
                if entity == ev.target {
                    let reduction = (armor.0 / (armor.0 + 100.0)).min(0.8);
                    reduced = ev.amount * (1.0 - reduction);
                    hp.current = (hp.current - reduced).max(0.0);
                }
            });

            writer.send(DamageEvent { target: ev.target, amount: reduced });
            println!("  [ArmorSystem]  entity={:?} dmg={:.1} armor={:.0} → reduced={:.1}",
                ev.target, ev.amount,
                q.iter()
                    .find(|(e, _)| *e == ev.target)
                    .map(|(_, (a, _))| a.0)
                    .unwrap_or(0.0),
                reduced);
        }
        if count == 0 {
            println!("  [ArmorSystem]  (no events to process)");
        }
    }
}

// ── Consumer: HealthSystem ─────────────────────────────────────
// Просто читает Health — видит изменения ArmorSystem того же кадра.

system! {
    fn health_system(
        q: Read<Health>,
    ) {
        for (entity, hp) in q.iter() {
            println!("  [HealthSystem] entity={:?} HP={:.1}/{}", entity, hp.current, hp.max);
        }
    }
}

// ── Consumer: SoundSystem ──────────────────────────────────────

system! {
    fn sound_system(
        q: Read<Collider>,
        reader: &[DamageEvent],
    ) {
        let events: Vec<_> = reader.iter().to_vec();
        if !events.is_empty() {
            println!("  [SoundSystem]  {} sounds (first amount={:.1})", events.len(), events[0].amount);
        }
    }
}

// ── main ───────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Event Pipeline Example ===\n");
    println!("Pipeline: CollisionSystem → ArmorSystem → [HealthSystem, SoundSystem]\n");
    println!("Техника: трансформер (ArmorSystem) модифицирует Health (компонент),\n\
              поэтому изменения видны HealthSystem в том же кадре.\n");

    let mut world = World::new();
    world.add_event::<DamageEvent>();

    // Два персонажа: игрок с бронёй, враг без брони
    let _player = world.spawn((Collider, Health { current: 100.0, max: 100.0 }, Armor(50.0)));
    let _enemy  = world.spawn((Collider, Health { current: 80.0, max: 80.0 },  Armor(0.0)));

    // ── Scheduler ────────────────────────────────────────────────

    let mut sched = Scheduler::new();

    sched.add_systems(StageLabel::Update, (
        sys("collision", collision_system),
        sys("armor",     armor_system),
        sys("health",    health_system),
        sys("sound",     sound_system),
    ));

    // Конвейер событий: явный порядок выполнения
    Scheduler::event_pipeline::<DamageEvent>()
        .produced_by("collision")
        .transformed_by("armor")
        .consumed_by("health")
        .consumed_by("sound")
        .build(&mut sched);

    sched.compile_with_world(&world).unwrap();

    println!("--- Execution plan ---\n{}", sched.debug_plan());

    // ── Tick 1 ───────────────────────────────────────────────────
    // tick() инкрементирует счётчик, sched.run() автоматически
    // флашит события после каждого Stage.
    //   1. Collision пишет DamageEvent (pending буфер)
    //   2. Armor читает из events (пока пусто), пишет в Health
    //   3. HealthSystem читает Health после Armor
    //   4. SoundSystem читает из events (пусто)
    //
    // На Tick 1: коллизий ещё не генерировалось, буферы пусты.

    println!("\n--- Tick 1 (стартовый, буфер пуст) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Tick 2 ───────────────────────────────────────────────────
    // sched.run() на Tick 1 сгенерировал события, которые стали
    // доступны в events-буфере после per-stage flush.
    // Теперь Armor видит события от Collision с предыдущего Stage.
    //   1. Collision пишет 3x DamageEvent (pending)
    //   2. Armor читает 3 события, модифицирует Health, пишет 3 reduced
    //   3. HealthSystem читает Health — видит ПОНИЖЕННЫЙ урон
    //   4. SoundSystem читает события из предыдущего Stage

    println!("\n--- Tick 2 (Armor модифицирует Health, Sound читает оригиналы) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Tick 3 ───────────────────────────────────────────────────
    // В events-буфере теперь: original от Collision + reduced от Armor
    //   1. Collision пишет 3x DamageEvent
    //   2. Armor читает 6 событий, модифицирует Health, пишет 6 reduced
    //   3. HealthSystem читает Health — пониженный урон
    //   4. SoundSystem читает 6 событий

    println!("\n--- Tick 3 (Sound видит оригиналы + редуцированные Tick2) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Итоги ────────────────────────────────────────────────────
    println!("\n=== Итоги ===");
    println!(" - Порядок выполнения: collision → armor → [health, sound] (гарантирован pipeline)");
    println!(" - Scheduler флашит события после каждого Stage, поэтому события видны в том же кадре");
    println!(" - ArmorSystem модифицирует Health — HealthSystem читает изменённые значения в том же кадре");
    println!(" - health и sound в одном Stage — выполняются параллельно (нет пересечения доступа)");

    println!("\nEntities: {}", world.entity_count());
}
