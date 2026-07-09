//! Apex ECS — Maybe<T> + Events (read_partial, DelayedQueue, send_sync)
//!
//! Demonstrates advanced event capabilities:
//! 1. **Maybe<T> / MaybeWrite<T>** — optional components in a Query
//! 2. **Event auto-registration** — send_event without add_event
//! 3. **read_partial** — batched reading without dropping events
//! 4. **DelayedQueue** — deferred delivery via BinaryHeap (FIFO)
//! 5. **send_sync / send_batch_sync** — thread-safe event sending
//!
//! ```bash
//! cargo run --example maybe_and_events
//! ```

use apex_core::prelude::*;
// DelayedQueue is an advanced utility, not in the prelude (see docs/CONVENTIONS.md §2).
use apex_core::events::DelayedQueue;
use apex_macros::Component;

// ── Components ─────────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)]
struct Position { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug)]
struct Health { current: f32, max: f32 }

#[derive(Component, Clone, Copy, Debug)]
struct Speed(f32);

#[derive(Component, Clone, Copy, Debug)]
struct Player;

#[derive(Component, Clone, Copy, Debug)]
struct Enemy;

// ── Events (no derive Serialize — not needed for send_action_event) ──

#[derive(Clone, Copy, Debug)]
struct ScoreEvent(u32);

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct CollisionEvent { entity: Entity, damage: f32 }

#[derive(Clone, Copy, Debug)]
struct PowerupEvent(u32);

// ── main ────────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Maybe<T> + Event Auto-Registration ===\n");

    let mut world = World::new();

    // Spawn entities with different sets of components.
    // Component registration happens automatically (via spawn).
    let player = world.spawn((
        Position { x: 0.0, y: 0.0 },
        Health  { current: 100.0, max: 100.0 },
        Speed(2.5),
        Player,
    ));

    let enemies = world.spawn_many(3, |i| {
        let offset = (i + 1) as f32 * 50.0;
        (
            Position { x: offset, y: 0.0 },
            Health   { current: 30.0, max: 30.0 },
            Speed(1.0 + i as f32 * 0.5),
            Enemy,
        )
    });

    // Create an entity with only Position (decoration, no health)
    let _tree = world.spawn((Position { x: 200.0, y: 100.0 },));

    println!("  Player:  entity={:?}", player);
    println!("  Enemies: {} entities", enemies.len());
    println!("  World:   {} entities total", world.entity_count());
    println!();

    // ── Demo 1: Maybe<Health> — optional component ─────────

    println!("--- 1. Maybe<Health>: all entities with Position, Health optional ---");

    // A single pass over ALL entities with Position; Health is optional
    let query = Query::<(&Position, Maybe<Health>)>::new(&world);
    query.for_each(|entity, (pos, hp_opt)| {
        match hp_opt {
            Some(hp) => println!(
                "  entity {:?}: pos=({}, {}) HP={}/{}",
                entity, pos.x, pos.y, hp.current, hp.max
            ),
            None => println!(
                "  entity {:?}: pos=({}, {}) — no Health (decoration)",
                entity, pos.x, pos.y
            ),
        }
    });

    // ── Demo 2: MaybeWrite<Speed> — optional mutation ────────

    println!("\n--- 2. MaybeWrite<Speed>: speed up only entities that have Speed ---");

    // Slow down all moving entities that have Speed
    let mut query = Query::<(MaybeWrite<Speed>, With<Enemy>)>::new_mut(&mut world);
    query.for_each_mut(|entity, (speed_opt, _)| {
        if let Some(mut speed) = speed_opt {
            speed.0 *= 0.8;
            println!("  entity {:?}: slowed to speed={}", entity, speed.0);
        } else {
            // We won't reach here — With<Enemy> + MaybeWrite<Speed>
            // Enemy always has Speed, but with other combinations it could be None
        }
    });

    // ── Demo 3: Event auto-registration ─────────────────────────

    println!("\n--- 3. Auto-registration: send_event without add_event ---");

    // Previously required: world.add_event::<ScoreEvent>();
    // Now send_event registers the type itself:
    world.send_event(ScoreEvent(100));
    println!("  ✓ send_event(ScoreEvent) — auto-registration");

    world.send_event(CollisionEvent { entity: player, damage: 25.0 });
    println!("  ✓ send_event(CollisionEvent) — auto-registration");

    world.send_event(ScoreEvent(200));

    // Read events (world.tick() increments the tick, flush_all_events() advances the buffers)
    world.tick();
    world.flush_all_events();

    // Access events — as usual
    let score_events = world.events::<ScoreEvent>();
    println!("  ScoreEvents after tick(): {}", score_events.len_readable());

    let col_events = world.events::<CollisionEvent>();
    println!("  CollisionEvents after tick(): {}", col_events.len_readable());

    // ── Demo 4: EventReader in a system (no add_event needed) ──

    println!("\n--- 4. EventReader — read events in a system ---");

    // A sequential system reads events — add_event is not needed,
    // send_event already registered the type
    use apex_core::system_param::EventReader;

    {
        let reader = EventReader::new(world.events_mut::<ScoreEvent>());
        println!("  Score events to read: {}", reader.len());
        for ev in reader.iter() {
            println!("    Score: {}", ev.0);
        }
    }

    // ── Demo 5: try_resource ────────────────────────────────────

    println!("\n--- 5. ctx.try_resource — safe access ---");

    // Show that try_resource works on SystemContext too
    // (via world for simplicity)
    world.insert_resource(DeltaTime(0.016f32));

    if let Some(dt) = world.try_resource::<DeltaTime>() {
        println!("  ✓ world.try_resource<DeltaTime>: dt={}", dt.0);
    }

    // Missing resource — not a panic, but None
    if world.try_resource::<String>().is_none() {
        println!("  ✓ world.try_resource<String>: None (resource not inserted)");
    }

    // ── Demo 6: read_partial — batched reading without dropping ─────

    println!("\n--- 6. read_partial: batched reading without dropping events ---");

    // Use the low-level API (add_reader + read_partial)
    world.add_event::<PowerupEvent>();
    let cursor = world.events_mut::<PowerupEvent>().add_reader();

    // Send 7 events, tick → they are in the read buffer
    for i in 0..7 {
        world.send_event(PowerupEvent(i));
    }
    world.tick();
    world.flush_all_events();

    let total = world.events::<PowerupEvent>().len_readable();
    println!("  Events in buffer: {}", total);

    // Read 3 at a time — the cursor advances exactly by what was read
    let mut processed = 0usize;
    loop {
        let guard = world.events_mut::<PowerupEvent>().read_partial(&cursor, 3);
        if guard.is_empty() { break; }
        for ev in guard.iter() {
            print!(" {}", ev.0);
        }
        processed += guard.len();
    } // guard drop → cursor advanced exactly by guard.len()
    println!();
    println!("  Read: {} of {} (all processed, none dropped)", processed, total);

    // ── Demo 7: DelayedQueue — deferred delivery ─────────────

    println!("\n--- 7. DelayedQueue: deferred delivery with FIFO ordering ---");

    world.add_event::<&str>();
    let mut delayed = DelayedQueue::new();
    let str_cursor = world.events_mut::<&str>().add_reader();

    // Send events with delays
    delayed.send_delayed("alpha",   1, 0);  // deliver_at = 1
    delayed.send_delayed("beta",    1, 0);  // deliver_at = 1
    delayed.send_delayed("gamma",   2, 0);  // deliver_at = 2
    delayed.send_delayed("delta",   2, 0);  // deliver_at = 2
    println!("  Deferred events: {}", delayed.len());

    // Tick 1 — only alpha, beta are delivered (FIFO among themselves)
    delayed.flush_delayed(1, world.events_mut::<&str>());
    println!("  After flush(1): pending={}", world.events_mut::<&str>().len_pending());
    world.tick();
    world.flush_all_events();
    {
        let ev = world.events::<&str>().iter(&str_cursor);
        println!("  Read at tick 1: [{}]", ev.to_vec().join(", "));
        // alpha before beta (FIFO)
        assert_eq!(ev[0], "alpha");
        assert_eq!(ev[1], "beta");
    }

    // Tick 2 — gamma, delta are delivered
    delayed.flush_delayed(2, world.events_mut::<&str>());
    world.events_mut::<&str>().advance_reader_mut(&str_cursor);
    world.tick();
    world.flush_all_events();
    {
        let ev = world.events::<&str>().iter(&str_cursor);
        println!("  Read at tick 2: [{}]", ev.to_vec().join(", "));
        assert_eq!(ev[0], "gamma");
        assert_eq!(ev[1], "delta");
    }
    println!("  DelayedQueue empty: {}", delayed.is_empty());

    // ── Demo 8: send_sync — thread-safe sending ──────────────

    println!("\n--- 8. send_sync: thread-safe sending (via &self) ---");

    world.add_event::<i32>();
    let sync_cursor = world.events_mut::<i32>().add_reader();

    // send_sync is available via &Events<T> (without &mut)
    let queue_ref: &Events<i32> = world.events::<i32>();
    queue_ref.send_sync(100);
    queue_ref.send_sync(200);
    queue_ref.send_batch_sync(300..=302);

    // After flush_sync the data is in pending
    world.events_mut::<i32>().flush_sync();
    println!("  After flush_sync: pending={}", world.events_mut::<i32>().len_pending());

    // After flush available for reading
    world.tick();
    world.flush_all_events();
    {
        let ev = world.events::<i32>().iter(&sync_cursor);
        let vals: Vec<_> = ev.to_vec();
        println!("  Read: {:?}", vals);
        assert_eq!(vals, vec![100, 200, 300, 301, 302]);
    }
    println!("  ✓ send_sync + send_batch_sync work correctly");

    // ── Summary ────────────────────────────────────────────────────

    println!("\n=== SUMMARY ===");
    println!("✅ Maybe<T> — optional components without world.get()");
    println!("✅ MaybeWrite<T> — optional mutation");
    println!("✅ send_event — without add_event (auto-registration)");
    println!("✅ try_resource — safe access to resources");
    println!("✅ read_partial — batched reading without dropping events");
    println!("✅ DelayedQueue — deferred delivery with BinaryHeap + FIFO");
    println!("✅ send_sync / send_batch_sync — thread-safe sending");
    println!();
    println!("  Example finished, entity: {}", world.entity_count());
}

// Helper resource
#[derive(Clone, Copy, Debug)]
struct DeltaTime(f32);
