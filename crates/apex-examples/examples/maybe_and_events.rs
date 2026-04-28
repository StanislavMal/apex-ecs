//! Apex ECS — Maybe<T> + Event Auto-Registration
//!
//! Демонстрирует две новые возможности:
//! 1. **Maybe<T> / MaybeWrite<T>** — optional-компоненты в Query
//! 2. **Авторегистрация событий** — send_event без add_event
//!
//! ```bash
//! cargo run --example maybe_and_events
//! ```

use apex_core::prelude::*;

// ── Компоненты ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy, Debug)]
struct Health { current: f32, max: f32 }

#[derive(Clone, Copy, Debug)]
struct Speed(f32);

#[derive(Clone, Copy, Debug)]
struct Player;

#[derive(Clone, Copy, Debug)]
struct Enemy;

// ── События (без derive Serialize — для send_action_event не нужно) ──

#[derive(Clone, Copy, Debug)]
struct ScoreEvent(u32);

#[derive(Clone, Copy, Debug)]
struct CollisionEvent { entity: Entity, damage: f32 }

// ── main ────────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Maybe<T> + Event Auto-Registration ===\n");

    let mut world = World::new();

    // Спавним entity с разными наборами компонентов
    // Регистрация компонентов происходит автоматически (через spawn)
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

    // Создаём entity только с Position (декорации, без здоровья)
    let _tree = world.spawn((Position { x: 200.0, y: 100.0 },));

    println!("  Player:  entity={:?}", player);
    println!("  Enemies: {} entities", enemies.len());
    println!("  World:   {} entities total", world.entity_count());
    println!();

    // ── Демо 1: Maybe<Health> — опциональный компонент ─────────

    println!("--- 1. Maybe<Health>: все entity с Position, Health опционально ---");

    // Один проход по ВСЕМ entity с Position, Health — опционально
    let query = Query::<(Read<Position>, Maybe<Health>)>::new(&world);
    query.for_each(|entity, (pos, hp_opt)| {
        match hp_opt {
            Some(hp) => println!(
                "  entity {:?}: pos=({}, {}) HP={}/{}",
                entity, pos.x, pos.y, hp.current, hp.max
            ),
            None => println!(
                "  entity {:?}: pos=({}, {}) — без Health (декорация)",
                entity, pos.x, pos.y
            ),
        }
    });

    // ── Демо 2: MaybeWrite<Speed> — опциональная мутация ────────

    println!("\n--- 2. MaybeWrite<Speed>: ускоряем только entity со Speed ---");

    // Замедляем все движущиеся entity, у кого есть Speed
    let query = Query::<(MaybeWrite<Speed>, With<Enemy>)>::new(&world);
    query.for_each(|entity, (speed_opt, _)| {
        if let Some(speed) = speed_opt {
            speed.0 *= 0.8;
            println!("  entity {:?}: замедлен до speed={}", entity, speed.0);
        } else {
            // Сюда не попадём — With<Enemy> + MaybeWrite<Speed>
            // Enemy всегда есть Speed, но при других комбинациях могло быть None
        }
    });

    // ── Демо 3: Авторегистрация событий ─────────────────────────

    println!("\n--- 3. Авторегистрация: send_event без add_event ---");

    // Раньше требовалось: world.add_event::<ScoreEvent>();
    // Теперь send_event сам регистрирует тип:
    world.send_event(ScoreEvent(100));
    println!("  ✓ send_event(ScoreEvent) — авто-регистрация");

    world.send_event(CollisionEvent { entity: player, damage: 25.0 });
    println!("  ✓ send_event(CollisionEvent) — авто-регистрация");

    // try_send_event теперь тоже всегда успешен
    assert!(world.try_send_event(ScoreEvent(200)));
    println!("  ✓ try_send_event — всегда true");

    // Читаем события (world.tick() продвигает буферы)
    world.tick();

    // Доступ к событиям — как обычно
    let score_events = world.events::<ScoreEvent>();
    println!("  ScoreEvent'ов после tick(): {}", score_events.len_readable());

    let col_events = world.events::<CollisionEvent>();
    println!("  CollisionEvent'ов после tick(): {}", col_events.len_readable());

    // ── Демо 4: EventReader в системе (никакого add_event не нужно) ──

    println!("\n--- 4. EventReader — читаем события в системе ---");

    // Sequential система читает события — add_event не нужен,
    // send_event уже зарегистрировал тип
    use apex_core::system_param::EventReader;

    let reader = EventReader::new(world.events_mut::<ScoreEvent>());
    println!("  Score событий для чтения: {}", reader.len());
    for ev in reader.iter() {
        println!("    Score: {}", ev.0);
    }

    // ── Демо 5: try_resource ────────────────────────────────────

    println!("\n--- 5. ctx.try_resource — безопасный доступ ---");

    // Показываем, что try_resource работает и на SystemContext
    // (через world для простоты)
    world.insert_resource(DeltaTime(0.016f32));

    if let Some(dt) = world.try_resource::<DeltaTime>() {
        println!("  ✓ world.try_resource<DeltaTime>: dt={}", dt.0);
    }

    // Отсутствующий ресурс — не паника, а None
    if world.try_resource::<String>().is_none() {
        println!("  ✓ world.try_resource<String>: None (ресурс не вставлен)");
    }

    // ── Итог ────────────────────────────────────────────────────

    println!("\n=== ИТОГ ===");
    println!("✅ Maybe<T> — опциональные компоненты без world.get()");
    println!("✅ MaybeWrite<T> — опциональная мутация");
    println!("✅ send_event — без add_event (авторегистрация)");
    println!("✅ try_send_event — всегда успешен");
    println!("✅ try_resource — безопасный доступ к ресурсам");
    println!();
    println!("  Пример завершён, entity: {}", world.entity_count());
}

// Вспомогательный ресурс
#[derive(Clone, Copy, Debug)]
struct DeltaTime(f32);
