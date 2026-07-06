//! Apex ECS — Event Pipeline Example
//!
//! Demonstrates pipelined event processing:
//! Producer → Transformer → [Consumer, Consumer (parallel)].
//!
//! Scenario: CollisionSystem emits DamageEvent → ArmorSystem applies
//! armor (modifies Health directly) and re-emits the event →
//! HealthSystem reads Health (modified by ArmorSystem in the same frame) +
//! SoundSystem reads the events.
//!
//! ## How the pipeline works with per-Stage flush (v0.1.0)
//!
//! The pipeline guarantees execution order: collision → armor → [health, sound].
//! The Scheduler automatically flushes events after each Stage,
//! so events sent at Stage N are visible at Stage N+1 of the same frame.
//!
//! - ArmorSystem modifies Health (a component) — changes are visible to health
//!   in the same frame.
//! - ArmorSystem re-emits DamageEvent → SoundSystem will see it
//!   at the next Stage of the same frame (per-stage flush).
//!
//! cargo run -p apex-examples --example event_pipeline --release

use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::{Scheduler, StageLabel, sys};

// ── Components ─────────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)]
struct Collider;

#[derive(Component, Clone, Copy, Debug)]
struct Health {
    current: f32,
    max: f32,
}

#[derive(Component, Clone, Copy, Debug)]
struct Armor(f32);

// ── Event ──────────────────────────────────────────────────────

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
        q.for_each(|entity, _| {
            writer.send(DamageEvent { target: entity, amount: 25.0 });
        });
        println!("  [CollisionSystem] emitted {}x DamageEvent(25.0)", count);
    }
}

// ── Transformer: ArmorSystem ───────────────────────────────────
// Reads DamageEvent, MODIFIES Health (a component), re-emits the
// modified event for the following stages.

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
            q.for_each_mut(|entity, (armor, mut hp)| {
                if entity == ev.target {
                    let reduction = (armor.0 / (armor.0 + 100.0)).min(0.8);
                    reduced = ev.amount * (1.0 - reduction);
                    hp.current = (hp.current - reduced).max(0.0);
                }
            });

            writer.send(DamageEvent { target: ev.target, amount: reduced });
            println!("  [ArmorSystem]  entity={:?} dmg={:.1} armor={:.0} → reduced={:.1}",
                ev.target, ev.amount,
                {
                    let mut armor_val = 0.0;
                    q.for_each_mut(|e, (a, _)| if e == ev.target { armor_val = a.0 });
                    armor_val
                },
                reduced);
        }
        if count == 0 {
            println!("  [ArmorSystem]  (no events to process)");
        }
    }
}

// ── Consumer: HealthSystem ─────────────────────────────────────
// Simply reads Health — sees ArmorSystem's changes from the same frame.

system! {
    fn health_system(
        q: Read<Health>,
    ) {
        q.for_each(|entity, hp| {
            println!("  [HealthSystem] entity={:?} HP={:.1}/{}", entity, hp.current, hp.max);
        });
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
    println!("Technique: the transformer (ArmorSystem) modifies Health (a component),\n\
              so the changes are visible to HealthSystem in the same frame.\n");

    let mut world = World::new();
    world.add_event::<DamageEvent>();

    // Two characters: a player with armor, an enemy without armor
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

    // Event pipeline: explicit execution order
    Scheduler::event_pipeline::<DamageEvent>()
        .produced_by("collision")
        .transformed_by("armor")
        .consumed_by("health")
        .consumed_by("sound")
        .build(&mut sched);

    sched.compile_with_world(&world).unwrap();

    println!("--- Execution plan ---\n{}", sched.debug_plan());

    // ── Tick 1 ───────────────────────────────────────────────────
    // tick() increments the counter, sched.run() automatically
    // flushes events after each Stage.
    //   1. Collision writes DamageEvent (pending buffer)
    //   2. Armor reads from events (empty so far), writes to Health
    //   3. HealthSystem reads Health after Armor
    //   4. SoundSystem reads from events (empty)
    //
    // On Tick 1: no collisions generated yet, buffers are empty.

    println!("\n--- Tick 1 (initial, buffer empty) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Tick 2 ───────────────────────────────────────────────────
    // sched.run() on Tick 1 generated events that became
    // available in the events buffer after the per-stage flush.
    // Now Armor sees the events from Collision at the previous Stage.
    //   1. Collision writes 3x DamageEvent (pending)
    //   2. Armor reads 3 events, modifies Health, writes 3 reduced
    //   3. HealthSystem reads Health — sees the REDUCED damage
    //   4. SoundSystem reads events from the previous Stage

    println!("\n--- Tick 2 (Armor modifies Health, Sound reads originals) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Tick 3 ───────────────────────────────────────────────────
    // The events buffer now holds: originals from Collision + reduced from Armor
    //   1. Collision writes 3x DamageEvent
    //   2. Armor reads 6 events, modifies Health, writes 6 reduced
    //   3. HealthSystem reads Health — reduced damage
    //   4. SoundSystem reads 6 events

    println!("\n--- Tick 3 (Sound sees originals + reduced from Tick2) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── Summary ──────────────────────────────────────────────────
    println!("\n=== Summary ===");
    println!(" - Execution order: collision → armor → [health, sound] (guaranteed by pipeline)");
    println!(" - The Scheduler flushes events after each Stage, so events are visible in the same frame");
    println!(" - ArmorSystem modifies Health — HealthSystem reads the changed values in the same frame");
    println!(" - health and sound in the same Stage — run in parallel (no access overlap)");

    println!("\nEntities: {}", world.entity_count());
}
