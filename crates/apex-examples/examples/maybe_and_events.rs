//! Apex ECS — Maybe<T> + Events (read_partial, DelayedQueue, send_sync)
//!
//! Демонстрирует расширенные возможности событий:
//! 1. **Maybe<T> / MaybeWrite<T>** — optional-компоненты в Query
//! 2. **Авторегистрация событий** — send_event без add_event
//! 3. **read_partial** — пакетное чтение без потери событий
//! 4. **DelayedQueue** — отложенная доставка через BinaryHeap (FIFO)
//! 5. **send_sync / send_batch_sync** — thread-safe отправка событий
//!
//! ```bash
//! cargo run --example maybe_and_events
//! ```

use apex_core::prelude::*;
// DelayedQueue is an advanced utility, not in the prelude (see docs/CONVENTIONS.md §2).
use apex_core::events::DelayedQueue;
use apex_macros::Component;

// ── Компоненты ─────────────────────────────────────────────────

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

// ── События (без derive Serialize — для send_action_event не нужно) ──

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
    let mut query = Query::<(MaybeWrite<Speed>, With<Enemy>)>::new_mut(&mut world);
    query.for_each_mut(|entity, (speed_opt, _)| {
        if let Some(mut speed) = speed_opt {
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

    world.send_event(ScoreEvent(200));

    // Читаем события (world.tick() инкрементирует тик, flush_all_events() продвигает буферы)
    world.tick();
    world.flush_all_events();

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

    {
        let reader = EventReader::new(world.events_mut::<ScoreEvent>());
        println!("  Score событий для чтения: {}", reader.len());
        for ev in reader.iter() {
            println!("    Score: {}", ev.0);
        }
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

    // ── Демо 6: read_partial — пакетное чтение без потери ─────

    println!("\n--- 6. read_partial: пакетное чтение без потери событий ---");

    // Используем низкоуровневый API (add_reader + read_partial)
    world.add_event::<PowerupEvent>();
    let cursor = world.events_mut::<PowerupEvent>().add_reader();

    // Отправляем 7 событий, tick → они в буфере чтения
    for i in 0..7 {
        world.send_event(PowerupEvent(i));
    }
    world.tick();
    world.flush_all_events();

    let total = world.events::<PowerupEvent>().len_readable();
    println!("  Событий в буфере: {}", total);

    // Читаем по 3 за раз — курсор продвигается ровно на прочитанное
    let mut processed = 0usize;
    loop {
        let guard = world.events_mut::<PowerupEvent>().read_partial(&cursor, 3);
        if guard.is_empty() { break; }
        for ev in guard.iter() {
            print!(" {}", ev.0);
        }
        processed += guard.len();
    } // guard дроп → курсор продвинут ровно на guard.len()
    println!();
    println!("  Прочитано: {} из {} (все обработаны, ни одно не потеряно)", processed, total);

    // ── Демо 7: DelayedQueue — отложенная доставка ─────────────

    println!("\n--- 7. DelayedQueue: отложенная доставка с FIFO-порядком ---");

    world.add_event::<&str>();
    let mut delayed = DelayedQueue::new();
    let str_cursor = world.events_mut::<&str>().add_reader();

    // Отправляем события с задержками
    delayed.send_delayed("alpha",   1, 0);  // deliver_at = 1
    delayed.send_delayed("beta",    1, 0);  // deliver_at = 1
    delayed.send_delayed("gamma",   2, 0);  // deliver_at = 2
    delayed.send_delayed("delta",   2, 0);  // deliver_at = 2
    println!("  Отложено событий: {}", delayed.len());

    // Тик 1 — доставляются только alpha, beta (FIFO между собой)
    delayed.flush_delayed(1, world.events_mut::<&str>());
    println!("  После flush(1): pending={}", world.events_mut::<&str>().len_pending());
    world.tick();
    world.flush_all_events();
    {
        let ev = world.events::<&str>().iter(&str_cursor);
        println!("  Прочитано в тик 1: [{}]", ev.to_vec().join(", "));
        // alpha before beta (FIFO)
        assert_eq!(ev[0], "alpha");
        assert_eq!(ev[1], "beta");
    }

    // Тик 2 — доставляются gamma, delta
    delayed.flush_delayed(2, world.events_mut::<&str>());
    world.events_mut::<&str>().advance_reader_mut(&str_cursor);
    world.tick();
    world.flush_all_events();
    {
        let ev = world.events::<&str>().iter(&str_cursor);
        println!("  Прочитано в тик 2: [{}]", ev.to_vec().join(", "));
        assert_eq!(ev[0], "gamma");
        assert_eq!(ev[1], "delta");
    }
    println!("  DelayedQueue пуста: {}", delayed.is_empty());

    // ── Демо 8: send_sync — thread-safe отправка ──────────────

    println!("\n--- 8. send_sync: thread-safe отправка (через &self) ---");

    world.add_event::<i32>();
    let sync_cursor = world.events_mut::<i32>().add_reader();

    // send_sync доступен через &Events<T> (без &mut)
    let queue_ref: &Events<i32> = world.events::<i32>();
    queue_ref.send_sync(100);
    queue_ref.send_sync(200);
    queue_ref.send_batch_sync(300..=302);

    // После flush_sync данные в pending
    world.events_mut::<i32>().flush_sync();
    println!("  После flush_sync: pending={}", world.events_mut::<i32>().len_pending());

    // После flush доступны для чтения
    world.tick();
    world.flush_all_events();
    {
        let ev = world.events::<i32>().iter(&sync_cursor);
        let vals: Vec<_> = ev.to_vec();
        println!("  Прочитано: {:?}", vals);
        assert_eq!(vals, vec![100, 200, 300, 301, 302]);
    }
    println!("  ✓ send_sync + send_batch_sync работают корректно");

    // ── Итог ────────────────────────────────────────────────────

    println!("\n=== ИТОГ ===");
    println!("✅ Maybe<T> — опциональные компоненты без world.get()");
    println!("✅ MaybeWrite<T> — опциональная мутация");
    println!("✅ send_event — без add_event (авторегистрация)");
    println!("✅ try_resource — безопасный доступ к ресурсам");
    println!("✅ read_partial — пакетное чтение без потери событий");
    println!("✅ DelayedQueue — отложенная доставка с BinaryHeap + FIFO");
    println!("✅ send_sync / send_batch_sync — thread-safe отправка");
    println!();
    println!("  Пример завершён, entity: {}", world.entity_count());
}

// Вспомогательный ресурс
#[derive(Clone, Copy, Debug)]
struct DeltaTime(f32);
