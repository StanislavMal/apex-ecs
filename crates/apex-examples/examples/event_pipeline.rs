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
//! ## Как работает пайплайн с double-buffered events
//!
//! Конвейер гарантирует порядок выполнения: collision → armor → [health, sound].
//! - ArmorSystem модифицирует Health (компонент) — изменения видны health
//!   в том же кадре.
//! - ArmorSystem перевыпускает DamageEvent для SoundSystem — SoundSystem
//!   увидит его на следующем кадре (event_writer пишет в текущий буфер,
//!   event_reader читает из предыдущего).
//!
//! cargo run -p apex-examples --example event_pipeline --release

use apex_core::prelude::*;
use apex_scheduler::Scheduler;

// ── Компоненты ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Collider;

#[derive(Clone, Copy, Debug)]
struct Health {
    current: f32,
    max: f32,
}

#[derive(Clone, Copy, Debug)]
struct Armor(f32);

// ── Событие ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
struct DamageEvent {
    target: Entity,
    amount: f32,
}

// ── Producer: CollisionSystem ──────────────────────────────────

struct CollisionSystem;

impl AutoSystem for CollisionSystem {
    type Query     = Read<Collider>;
    type Resources = ();
    type Events    = Emit<DamageEvent>;

    fn run(&mut self, ctx: SystemContext<'_>) {
        let mut writer = ctx.event_writer::<DamageEvent>();
        let count = ctx.query::<Read<Collider>>().len();
        for (entity, _) in ctx.query::<Read<Collider>>().iter() {
            writer.send(DamageEvent { target: entity, amount: 25.0 });
        }
        println!("  [CollisionSystem] emitted {}x DamageEvent(25.0)", count);
    }
}

// ── Transformer: ArmorSystem ───────────────────────────────────
// Читает DamageEvent, МОДИФИЦИРУЕТ Health (компонент), перевыпускает
// модифицированное событие для следующих этапов.

struct ArmorSystem;

impl AutoSystem for ArmorSystem {
    type Query     = (Read<Armor>, Write<Health>);
    type Resources = ();
    type Events    = (Listen<DamageEvent>, Emit<DamageEvent>);

    fn run(&mut self, ctx: SystemContext<'_>) {
        let reader = ctx.event_reader::<DamageEvent>();
        let mut writer = ctx.event_writer::<DamageEvent>();
        let mut count = 0usize;

        for ev in reader.iter() {
            count += 1;

            // Находим конкретную entity из события и применяем урон только ей
            let mut reduced = ev.amount;
            ctx.query::<(Read<Armor>, Write<Health>)>().for_each(|entity, (armor, hp)| {
                if entity == ev.target {
                    let reduction = (armor.0 / (armor.0 + 100.0)).min(0.8);
                    reduced = ev.amount * (1.0 - reduction);
                    hp.current = (hp.current - reduced).max(0.0);
                }
            });

            writer.send(DamageEvent { target: ev.target, amount: reduced });
            println!("  [ArmorSystem]  entity={:?} dmg={:.1} armor={:.0} → reduced={:.1}",
                ev.target, ev.amount,
                ctx.query::<(Read<Armor>, Write<Health>)>().iter()
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

struct HealthSystem;

impl AutoSystem for HealthSystem {
    type Query     = Read<Health>;
    type Resources = ();
    type Events    = ();

    fn run(&mut self, ctx: SystemContext<'_>) {
        for (entity, hp) in ctx.query::<Read<Health>>().iter() {
            println!("  [HealthSystem] entity={:?} HP={:.1}/{}", entity, hp.current, hp.max);
        }
    }
}

// ── Consumer: SoundSystem ──────────────────────────────────────

struct SoundSystem;

impl AutoSystem for SoundSystem {
    type Query     = Read<Collider>;
    type Resources = ();
    type Events    = Listen<DamageEvent>;

    fn run(&mut self, ctx: SystemContext<'_>) {
        let reader = ctx.event_reader::<DamageEvent>();
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
    world.register_component::<Collider>();
    world.register_component::<Health>();
    world.register_component::<Armor>();
    world.add_event::<DamageEvent>();

    // Два персонажа: игрок с бронёй, враг без брони
    let _player = world.spawn((Collider, Health { current: 100.0, max: 100.0 }, Armor(50.0)));
    let _enemy  = world.spawn((Collider, Health { current: 80.0, max: 80.0 },  Armor(0.0)));

    // ── Scheduler ────────────────────────────────────────────────

    let mut sched = Scheduler::new();

    let collision_id = sched.add_auto_system("collision", CollisionSystem);
    let armor_id     = sched.add_auto_system("armor",     ArmorSystem);
    let health_id    = sched.add_auto_system("health",    HealthSystem);
    let sound_id     = sched.add_auto_system("sound",     SoundSystem);

    // Конвейер событий: явный порядок выполнения
    Scheduler::event_pipeline::<DamageEvent>()
        .produced_by(collision_id, "collision")
        .transformed_by(armor_id,   "armor")        // Listen<Damage> + Write<Health> + Emit<Damage>
        .consumed_by(health_id,    "health")        // Read<Health> — видит изменения armor того же кадра
        .consumed_by(sound_id,     "sound")         // Listen<Damage> — прочитает на след. кадре
        .build(&mut sched);

    sched.compile_with_world(&world).unwrap();

    println!("--- Execution plan ---\n{}", sched.debug_plan());

    // ── Tick 1 ───────────────────────────────────────────────────
    // world.tick() → swap буферов.
    // Затем системы:
    //   1. Collision пишет DamageEvent (текущий буфер)
    //   2. Armor читает ⟵ из предыдущего (пусто), пишет в Health (компонент)
    //   3. HealthSystem читает Health — видит значения после Armor
    //      (но Armor ничего не сделал, т.к. events пуст)
    //   4. SoundSystem читает ⟵ из предыдущего (пусто)

    println!("\n--- Tick 1 (стартовый, буфер пуст) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Tick 2 ───────────────────────────────────────────────────
    // world.tick() → prev = события Tick1 (3x DamageEvent)
    // Системы:
    //   1. Collision пишет 3x DamageEvent (текущий буфер)
    //   2. Armor читает 3 события, модифицирует Health, пишет 3 reduced
    //   3. HealthSystem читает Health — видит ПОНЖЕННЫЙ урон!
    //   4. SoundSystem читает ⟵ из предыдущего — видит ОРИГИНАЛЬНЫЕ события
    //      (reduced события Armor ушли в текущий буфер, будут на Tick3)

    println!("\n--- Tick 2 (Armor модифицирует Health, Sound читает оригиналы) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Tick 3 ───────────────────────────────────────────────────
    // world.tick() → prev = 3x original (от Collision Tick2) + 3x reduced (от Armor Tick2)
    //   1. Collision пишет 3x DamageEvent
    //   2. Armor читает 6 событий, модифицирует Health, пишет 6 reduced
    //   3. HealthSystem читает Health — пониженный урон (от Armor Tick2 + Tick3)
    //   4. SoundSystem читает 6 событий — видит original+reduced от Tick2

    println!("\n--- Tick 3 (Sound видит оригиналы + редуцированные Tick2) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Итоги ────────────────────────────────────────────────────
    println!("\n=== Итоги ===");
    println!(" - Порядок выполнения: collision → armor → [health, sound] (гарантирован pipeline)");
    println!(" - ArmorSystem модифицирует Health — HealthSystem читает изменённые значения в том же кадре");
    println!(" - SoundSystem использует event_reader — видит события со смещением в 1 кадр");
    println!(" - health и sound в одном Stage — выполняются параллельно (нет пересечения доступа)");

    println!("\nEntities: {}", world.entity_count());
}
