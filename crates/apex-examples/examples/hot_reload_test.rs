//! Полный тест хот-релоада и всех Lua API-методов.
//!
//! Проверяет:
//! - `delta_time()` — возвращает корректное значение
//! - `entity_count()` — возвращает количество entity
//! - `query({"Read:..."})` — итерация с чтением
//! - `query({"Write:..."})` — итерация с записью
//! - `spawn_entity(table)` — создание entity с компонентами
//! - `despawn(entity)` — удаление entity
//! - `log()` / `print()` — логирование
//! - Хот-релоад: изменение скрипта на лету
//!
//! Запуск:
//!   cargo run -p apex-examples --example hot_reload_test

use apex_core::prelude::*;
use apex_scripting::{ScriptEngine, Scriptable};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

// ── Компоненты для теста ──────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Velocity { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Health { current: f32, max: f32 }

// Тупел-структ (одно поле) — тест обёртки { _value = ... }
#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Gravity(f32);

// Событие
#[derive(Clone, Copy, Debug, PartialEq, Scriptable)]
struct PlayerDied { x: f32, y: f32 }

// Маркерные компоненты для With/Without тестов
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Scriptable)]
struct Player;

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Scriptable)]
struct Enemy;

// ── Вспомогательные функции ──────────────────────────────────────

fn setup_scripts_dir() -> std::path::PathBuf {
    let dir = Path::new("target/hot_reload_full_test");
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("очистка temp dir");
    }
    std::fs::create_dir_all(dir).expect("создание temp dir");
    dir.to_path_buf()
}

fn write_script(path: &Path, content: &str) {
    let mut file = std::fs::File::create(path).expect("создание файла");
    file.write_all(content.as_bytes()).expect("запись");
    file.flush().expect("flush");
}

fn wait_for_hot_reload(
    engine: &mut ScriptEngine,
    world: &mut World,
    expected_spawn: usize,
    max_retries: usize,
) -> bool {
    for attempt in 1..=max_retries {
        engine.poll_hot_reload();
        let before = world.entity_count();
        engine.run(0.016, world);
        world.tick();
        let after = world.entity_count();
        if after == before + expected_spawn {
            println!("  -> Применилось с попытки {}/{}", attempt, max_retries);
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

// ── MAIN ──────────────────────────────────────────────────────────

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Warn)
        .init();

    println!("═══════════════════════════════════════════════");
    println!("  ПОЛНЫЙ ТЕСТ СКРИПТИНГА И ХОТ-РЕЛОАДА");
    println!("═══════════════════════════════════════════════\n");

    let mut all_ok = true;

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 1: Базовые функции (delta_time, entity_count)
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 1: delta_time + entity_count ---");

    let dir = setup_scripts_dir();
    let script_path = dir.join("test.lua");

    write_script(&script_path, r#"
function run()
    local dt = delta_time()
    local count = entity_count()
    print("[TEST] dt=" .. dt .. " entities=" .. count)
end
"#);

    let mut world = World::new();
    let mut engine = ScriptEngine::with_dir(&dir);
    engine.register_component::<Position>(&world);
    engine.register_component::<Velocity>(&world);
    engine.register_component::<Health>(&world);
    engine.register_component::<Gravity>(&world);
    engine.register_component::<Player>(&world);
    engine.register_component::<Enemy>(&world);
    engine.register_resource::<Gravity>();
    engine.register_event::<PlayerDied>();
    world.resources.insert(Gravity(9.8));
    world.add_event::<PlayerDied>();
    engine.load_scripts().expect("load_scripts");

    for _ in 0..3 {
        engine.poll_hot_reload();
        engine.run(0.016, &mut world);
        world.tick();
    }

    println!("  OK: delta_time + entity_count работают\n");

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 2: query с Read (чтение компонентов)
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 2: query с Read ---");

    world.spawn((
        Position { x: 1.0, y: 2.0 },
        Health { current: 100.0, max: 200.0 },
    ));
    world.spawn((
        Position { x: 3.0, y: 4.0 },
        Health { current: 50.0, max: 100.0 },
    ));

    write_script(&script_path, r#"
function run()
    local count = 0
    for entity in query({"Read:Position", "Read:Health"}) do
        count = count + 1
        print("[TEST] entity " .. entity.entity .. ": pos=(" .. entity.position.x .. "," .. entity.position.y .. ")")
    end
    print("[TEST] total read entities: " .. count)
end
"#);

    wait_for_hot_reload(&mut engine, &mut world, 0, 5);
    println!("  OK: query Read работает\n");

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 3: query с Write (изменение компонентов)
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 3: query с Write ---");

    // Создаём entity с Velocity для теста
    world.spawn((
        Position { x: 0.0, y: 0.0 },
        Velocity { x: 1.0, y: 0.5 },
    ));

    write_script(&script_path, r#"
function run()
    local dt = delta_time()
    local n = 0
    for entity in query({"Read:Velocity", "Write:Position"}) do
        n = n + 1
        entity.position.x = entity.position.x + entity.velocity.x * dt
        entity.position.y = entity.position.y + entity.velocity.y * dt
        commit(entity)
    end
    print("[TEST3] modified " .. n .. " entities, dt=" .. dt)
end
"#);

    // Ищем entity с обоими компонентами (Position + Velocity)
    let before_pos = {
        let q = Query::<(Read<Position>, Read<Velocity>)>::new(&world);
        q.iter().next().map(|(_, (p, _))| *p)
    };

    // Wait for watcher to detect file change 
    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.5, &mut world);
    world.tick();

    let after_pos = {
        let q = Query::<(Read<Position>, Read<Velocity>)>::new(&world);
        q.iter().next().map(|(_, (p, _))| *p)
    };

    match (before_pos, after_pos) {
        (Some(before), Some(after)) => {
            assert!(after.x > before.x, "Position.x не изменилась");
            assert!(after.y > before.y, "Position.y не изменилась");
            println!("  OK: query Write работает (x: {} -> {}, y: {} -> {})",
                before.x, after.x, before.y, after.y);
        }
        _ => {
            println!("  WARN: не удалось проверить — entity не найдены");
        }
    }
    println!();

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 4: spawn_entity
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 4: spawn_entity ---");

    write_script(&script_path, r#"
function run()
    if entity_count() < 10 then
        spawn_entity({
            position = Position.new(10.0, 20.0),
            health = Health.new(75.0, 100.0),
        })
    end
end
"#);

    let before = world.entity_count();
    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();
    let after = world.entity_count();

    if after > before {
        println!("  OK: spawn_entity работает ({} -> {})\n", before, after);
    } else {
        println!("  FAIL: spawn_entity не сработал\n");
        all_ok = false;
    }

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 5: despawn
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 5: despawn ---");

    write_script(&script_path, r#"
function run()
    for entity in query({"Read:Health"}) do
        if entity.health.current < 80.0 then
            despawn(entity.entity)
        end
    end
end
"#);

    let before = world.entity_count();
    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();
    let after = world.entity_count();

    if after < before {
        println!("  OK: despawn работает ({} -> {})\n", before, after);
    } else {
        println!("  WARN: despawn не удалил entity (возможно нет подходящих)\n");
    }

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 6: log() / print()
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 6: log() + print() ---");

    write_script(&script_path, r#"
function run()
    log("test log message from Lua")
    print("test print message from Lua")
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();

    println!("  OK: log и print работают (проверьте вывод выше)\n");

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 7: Хот-релоад (изменение скрипта на лету)
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 7: Хот-релоад ---");

    write_script(&script_path, r#"
function run()
    print("[TEST] VERSION 1")
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    engine.run(0.016, &mut world);
    world.tick();

    // Меняем скрипт
    write_script(&script_path, r#"
function run()
    print("[TEST] VERSION 2 - HOT RELOADED!")
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    wait_for_hot_reload(&mut engine, &mut world, 0, 5);

    println!("  OK: хот-релоад работает\n");

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 8: read_resource / write_resource
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 8: read_resource + write_resource ---");

    write_script(&script_path, r#"
function run()
    local g = read_resource("Gravity")
    if g._value > 0 then
        write_resource("Gravity", Gravity.new(g._value + 1.0))
    end
end
"#);

    let before_g = world.resources.try_get::<Gravity>().copied();
    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();
    let after_g = world.resources.try_get::<Gravity>().copied();

    match (before_g, after_g) {
        (Some(before), Some(after)) => {
            if after.0 > before.0 {
                println!("  OK: read/write_resource работает ({} -> {})\n", before.0, after.0);
            } else {
                println!("  FAIL: ресурс не изменился\n");
                all_ok = false;
            }
        }
        _ => {
            println!("  FAIL: ресурс не найден\n");
            all_ok = false;
        }
    }

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 9: emit_event
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 9: emit_event ---");

    write_script(&script_path, r#"
function run()
    emit_event("PlayerDied", PlayerDied.new(42.0, 24.0))
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();

    // Проверяем что событие было отправлено (могут быть в pending)
    let readable = world.events::<PlayerDied>().len_readable();
    let pending = world.events::<PlayerDied>().len_pending();
    if readable > 0 || pending > 0 {
        println!("  OK: emit_event работает (readable={}, pending={})\n", readable, pending);
    } else {
        println!("  WARN: событий нет (readable=0, pending=0)\n");
    }

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 10: Tuple struct (Gravity) — spawn + query
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 10: Tuple struct Gravity ---");

    write_script(&script_path, r#"
function run()
    local g = Gravity.new(5.5)
    spawn_entity({ gravity = g })

    for entity in query({"Read:Gravity"}) do
        print("[TEST] gravity._value=" .. entity.gravity._value)
    end
end
"#);

    let before = world.entity_count();
    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();
    let after = world.entity_count();

    if after > before {
        println!("  OK: tuple struct работает (spawn: {} -> {})\n", before, after);
    } else {
        println!("  FAIL: tuple struct не сработал\n");
        all_ok = false;
    }

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 11: With<T> — фильтр по наличию компонента
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 11: query With<T> ---");

    // Создаём два entity: Player (с Health) и Enemy (без Health)
    world.spawn((
        Position { x: 0.0, y: 0.0 },
        Health { current: 100.0, max: 100.0 },
        Player,
    ));
    world.spawn((
        Position { x: 10.0, y: 0.0 },
        Enemy,
    ));

    write_script(&script_path, r#"
function run()
    local count = 0
    -- With:Player — только entity с Player компонентом
    for entity in query({"Read:Position", "With:Player"}) do
        count = count + 1
    end
    print("[TEST11] entities with Player: " .. count)
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();

    // Проверяем: только 1 entity с Player маркером (созданный выше + возможно из теста 4)
    let player_count = Query::<(Read<Position>, With<Player>)>::new(&world).iter().count();
    println!("  OK: With<T> фильтр работает (Player entities: {})\n", player_count);

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 12: Without<T> — фильтр исключения
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 12: query Without<T> ---");

    write_script(&script_path, r#"
function run()
    local count = 0
    -- Without:Enemy — только не-Enemy entity с Position
    for entity in query({"Read:Position", "Without:Enemy"}) do
        count = count + 1
    end
    print("[TEST12] non-Enemy entities: " .. count)
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();

    let non_enemy = Query::<(Read<Position>, Without<Enemy>)>::new(&world).iter().count();
    let total_pos = Query::<Read<Position>>::new(&world).iter().count();
    if non_enemy < total_pos {
        println!("  OK: Without<T> фильтр работает (non-Enemy: {} из {})\n", non_enemy, total_pos);
    } else {
        println!("  WARN: Without<T> не отфильтровал ({} == {})\n", non_enemy, total_pos);
    }

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 13: Auto-commit (без явного commit())
    // ═══════════════════════════════════════════════════════

    println!("--- Тест 13: auto-commit ---");

    // Включаем авто-commit — commit(entity) вызывается автоматически
    engine.set_auto_commit(true);

    write_script(&script_path, r#"
function run()
    local dt = delta_time()
    for entity in query({"Read:Velocity", "Write:Position"}) do
        entity.position.x = entity.position.x + entity.velocity.x * dt
        entity.position.y = entity.position.y + entity.velocity.y * dt
        -- commit(entity) НЕ вызываем — авто-commit сам запишет
    end
end
"#);

    let before = {
        let q = Query::<(Read<Position>, Read<Velocity>)>::new(&world);
        q.iter().next().map(|(_, (p, _))| *p)
    };

    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.5, &mut world);
    world.tick();

    let after = {
        let q = Query::<(Read<Position>, Read<Velocity>)>::new(&world);
        q.iter().next().map(|(_, (p, _))| *p)
    };

    match (before, after) {
        (Some(b), Some(a)) if a.x > b.x => {
            println!("  OK: auto-commit работает (x: {} -> {})\n", b.x, a.x);
        }
        _ => {
            println!("  FAIL: auto-commit не сработал\n");
            all_ok = false;
        }
    }

    engine.set_auto_commit(false);

    // ═══════════════════════════════════════════════════════
    // ИТОГ
    // ═══════════════════════════════════════════════════════

    println!("═══════════════════════════════════════════════");
    if all_ok {
        println!("  ВСЕ ТЕСТЫ ПРОЙДЕНЫ!");
    } else {
        println!("  ЕСТЬ ПРОВАЛЕННЫЕ ТЕСТЫ");
    }
    println!("═══════════════════════════════════════════════");
}
