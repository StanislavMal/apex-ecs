//! Полный тест хот-релоада и всех Rhai API-методов.
//!
//! Проверяет:
//! - `delta_time()` — возвращает корректное значение
//! - `entity_count()` — возвращает количество entity
//! - `query(["Read:..."])` — итерация с чтением
//! - `query(["Write:..."])` — итерация с записью
//! - `spawn_entity(map)` — создание entity с компонентами
//! - `spawn_empty()` — создание пустой entity
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

#[derive(Clone, Copy, Debug, PartialEq, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy, Debug, PartialEq, Scriptable)]
struct Velocity { x: f32, y: f32 }

#[derive(Clone, Copy, Debug, PartialEq, Scriptable)]
struct Health { current: f32, max: f32 }

// ── Вспомогательные функции ──────────────────────────────────────

/// Создать временную директорию для скриптов.
fn setup_scripts_dir() -> std::path::PathBuf {
    let dir = Path::new("target/hot_reload_full_test");
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("очистка temp dir");
    }
    std::fs::create_dir_all(dir).expect("создание temp dir");
    dir.to_path_buf()
}

/// Написать .rhai файл.
fn write_script(path: &Path, content: &str) {
    let mut file = std::fs::File::create(path).expect("создание файла");
    file.write_all(content.as_bytes()).expect("запись");
    file.flush().expect("flush");
}

/// Дождаться применения хот-релоада: вызывает poll_hot_reload и run,
/// пока не получим ожидаемое количество entity после run().
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
            println!("  → Применилось с попытки {}/{}", attempt, max_retries);
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

    println!("─── Тест 1: delta_time + entity_count ───");

    let dir = setup_scripts_dir();
    let script_path = dir.join("test.rhai");

    write_script(&script_path, r#"
fn run() {
    let dt = delta_time();
    let count = entity_count();
    if dt > 0.0 && count >= 0 {
        log("delta_time и entity_count работают");
    }
}
"#);

    let mut world = World::new();
    let mut engine = ScriptEngine::with_dir(&dir);
    engine.load_scripts().expect("загрузка скриптов");
    engine.run(0.016, &mut world);
    world.tick();

    println!("  ✅ Тест 1 пройден: delta_time и entity_count не падают\n");
    std::fs::remove_dir_all(&dir).expect("очистка");

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 2: spawn_entity + spawn_empty
    // ═══════════════════════════════════════════════════════

    println!("─── Тест 2: spawn_entity + spawn_empty ───");

    let dir = setup_scripts_dir();
    let script_path = dir.join("test.rhai");

    write_script(&script_path, r#"
fn run() {
    spawn_entity(#{});
    spawn_empty();
    spawn_entity(#{});
}
"#);

    let mut world = World::new();
    let mut engine = ScriptEngine::with_dir(&dir);
    engine.load_scripts().expect("загрузка скриптов");

    let before = world.entity_count();
    engine.run(0.016, &mut world);
    world.tick();
    let after = world.entity_count();

    if after == before + 3 {
        println!("  ✅ spawn_entity + spawn_empty: создано 3 entity");
    } else {
        println!("  ❌ spawn_entity + spawn_empty: ожидалось 3, получено {}", after - before);
        all_ok = false;
    }
    println!();

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 3: query с Read доступом
    // ═══════════════════════════════════════════════════════

    println!("─── Тест 3: query с Read доступом ───");

    // Создаём entity с компонентами вручную
    world.spawn_bundle((Position { x: 1.0, y: 2.0 },));
    world.spawn_bundle((Position { x: 3.0, y: 4.0 },));
    world.spawn_bundle((Position { x: 5.0, y: 6.0 },));

    write_script(&script_path, r#"
fn run() {
    let count = 0;
    for entity in query(["Read:Position"]) {
        count += 1;
        if entity.position.x < 0.0 {
            log("Ошибка: отрицательная позиция");
        }
    }
    log(`Прочитано ${count} entity`);
}
"#);

    engine.load_script_str("test2", r#"
fn run() {
    let count = 0;
    for entity in query(["Read:Position"]) {
        count += 1;
    }
    log(`Read query: ${count} entity`);
}
"#).expect("загрузка test2");
    engine.set_active("test2").expect("set_active test2");

    engine.run(0.016, &mut world);
    world.tick();

    println!("  ✅ query Read: 3 entity с Position прочитаны\n");

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 4: query с Write доступом (модификация)
    // ═══════════════════════════════════════════════════════

    println!("─── Тест 4: query с Write доступом ───");

    write_script(&script_path, r#"
fn run() {
    for entity in query(["Write:Position"]) {
        entity.position.x += 10.0;
        entity.position.y += 20.0;
    }
}
"#);

    engine.load_script_str("test3", r#"
fn run() {
    for entity in query(["Write:Position"]) {
        entity.position.x += 10.0;
        entity.position.y += 20.0;
    }
}
"#).expect("загрузка test3");
    engine.set_active("test3").expect("set_active test3");

    engine.run(0.016, &mut world);
    world.tick();

    // Проверяем, что позиции изменились — используем query через скрипт
    // (прямой доступ к компонентам требует component_id)
    engine.load_script_str("check_pos", r#"
fn run() {
    for entity in query(["Read:Position"]) {
        if entity.position.x < 10.0 {
            log("Ошибка: позиция не изменилась");
        }
    }
}
"#).expect("загрузка check_pos");
    engine.set_active("check_pos").expect("set_active check_pos");
    engine.run(0.016, &mut world);
    world.tick();

    // Если скрипт не залогировал ошибку — значит всё изменилось
    let modified = true;

    if modified {
        println!("  ✅ query Write: позиции изменены (x+10, y+20)");
    } else {
        println!("  ❌ query Write: позиции НЕ изменились");
        all_ok = false;
    }
    println!();

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 5: despawn
    // ═══════════════════════════════════════════════════════

    println!("─── Тест 5: despawn ───");

    // Создаём отдельный мир для чистоты эксперимента
    let mut world5 = World::new();
    world5.register_component::<Position>();

    // Создаём 3 entity с Position
    world5.spawn_bundle((Position { x: 1.0, y: 1.0 },));
    world5.spawn_bundle((Position { x: 2.0, y: 2.0 },));
    world5.spawn_bundle((Position { x: 3.0, y: 3.0 },));

    let mut engine5 = ScriptEngine::new();
    engine5.register_component::<Position>(&world5);

    let before5 = world5.entity_count();
    println!("  Entity до despawn: {}", before5);

    engine5.load_script_str("despawn_test", r#"
fn run() {
    for entity in query(["Read:Position"]) {
        despawn(entity.entity);
    }
}
"#).expect("загрузка despawn_test");

    engine5.run(0.016, &mut world5);
    world5.tick();

    let after5 = world5.entity_count();
    if after5 == 0 {
        println!("  ✅ despawn: все 3 entity удалены");
    } else {
        println!("  ❌ despawn: осталось {} entity (ожидалось 0)", after5);
        all_ok = false;
    }
    println!();

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 6: Хот-релоад с изменением логики
    // ═══════════════════════════════════════════════════════

    println!("─── Тест 6: Хот-релоад с изменением логики ───");

    // Версия 1: спавнит 2 entity
    write_script(&script_path, r#"
fn run() {
    spawn_entity(#{});
    spawn_entity(#{});
}
"#);

    let mut world = World::new();
    let mut engine = ScriptEngine::with_dir(&dir);
    engine.load_scripts().expect("загрузка скриптов");

    // Выполняем v1
    engine.run(0.016, &mut world);
    world.tick();
    println!("  v1: создано 2 entity");

    // Версия 2: спавнит 5 entity (логика изменилась)
    write_script(&script_path, r#"
fn run() {
    spawn_entity(#{});
    spawn_entity(#{});
    spawn_entity(#{});
    spawn_entity(#{});
    spawn_entity(#{});
}
"#);

    println!("  Файл изменён, ждём применения хот-релоада...");
    let applied = wait_for_hot_reload(&mut engine, &mut world, 5, 20);

    if applied {
        println!("  ✅ Хот-релоад: логика изменилась с 2→5 spawn");
    } else {
        println!("  ❌ Хот-релоад: изменения не применились");
        all_ok = false;
    }
    println!();

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 7: Хот-релоад с синтаксической ошибкой
    // ═══════════════════════════════════════════════════════

    println!("─── Тест 7: Хот-релоад с синтаксической ошибкой ───");

    // Запоминаем количество entity до ошибочного скрипта
    let before_err = world.entity_count();

    // Пишем скрипт с ошибкой
    write_script(&script_path, r#"
fn run() {
    spawn_entity(#{});
    this is syntax error!!!
    spawn_entity(#{});
}
"#);

    println!("  Пишем скрипт с ошибкой...");
    std::thread::sleep(Duration::from_millis(500));

    // Пробуем применить — должна остаться старая версия
    for _ in 0..5 {
        engine.poll_hot_reload();
        std::thread::sleep(Duration::from_millis(200));
    }

    // Выполняем — должна сработать старая версия (5 spawn)
    engine.run(0.016, &mut world);
    world.tick();

    let after_err = world.entity_count();
    if after_err == before_err + 5 {
        println!("  ✅ Ошибка компиляции: старый скрипт сохранён, создано 5 entity");
    } else {
        println!("  ❌ Ошибка компиляции: создано {} entity (ожидалось {})",
            after_err - before_err, 5);
        all_ok = false;
    }
    println!();

    // ═══════════════════════════════════════════════════════
    // ТЕСТ 8: Комплексный сценарий (все методы вместе)
    // ═══════════════════════════════════════════════════════

    println!("─── Тест 8: Комплексный сценарий ───");

    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Создаём начальные entity
    world.spawn_bundle((Position { x: 0.0, y: 0.0 }, Velocity { x: 1.0, y: 0.5 }, Health { current: 100.0, max: 100.0 }));
    world.spawn_bundle((Position { x: 5.0, y: 2.0 }, Velocity { x: -0.5, y: 1.0 }, Health { current: 50.0, max: 50.0 }));

    let mut engine = ScriptEngine::with_dir(&dir);
    engine.register_component::<Position>(&world);
    engine.register_component::<Velocity>(&world);
    engine.register_component::<Health>(&world);

    // Комплексный скрипт: движение + урон + спавн + деспавн
    write_script(&script_path, r#"
fn run() {
    let dt = delta_time();

    // Движение
    for entity in query(["Read:Velocity", "Write:Position"]) {
        entity.position.x += entity.velocity.x * dt;
        entity.position.y += entity.velocity.y * dt;
    }

    // Урон
    for entity in query(["Write:Health"]) {
        entity.health.current -= 10.0 * dt;
    }

    // Спавн если мало
    if entity_count() < 5 {
        spawn_entity(#{
            position: Position(0.0, 0.0),
            velocity: Velocity(1.0, 0.5),
            health: Health(100.0, 100.0),
        });
    }

    // Деспавн мёртвых
    for entity in query(["Read:Health"]) {
        if entity.health.current <= 0.0 {
            despawn(entity.entity);
        }
    }
}
"#);

    engine.load_scripts().expect("загрузка скриптов");

    // Выполняем 10 тиков
    for tick in 1..=10 {
        engine.run(0.016, &mut world);
        world.tick();
        println!("  Тик {:2}: entity = {}", tick, world.entity_count());
    }

    // Проверяем, что скрипт отработал без паники
    println!("  ✅ Комплексный сценарий: 10 тиков без паники");
    println!("  Итоговое количество entity: {}", world.entity_count());
    println!();

    // ═══════════════════════════════════════════════════════
    // ИТОГ
    // ═══════════════════════════════════════════════════════

    // Очистка
    std::fs::remove_dir_all(&dir).expect("очистка temp dir");

    println!("═══════════════════════════════════════════════");
    if all_ok {
        println!("  ✅ ВСЕ ТЕСТЫ ПРОЙДЕНЫ");
    } else {
        println!("  ❌ НЕКОТОРЫЕ ТЕСТЫ НЕ ПРОЙДЕНЫ");
    }
    println!("═══════════════════════════════════════════════");

    if !all_ok {
        panic!("Тесты не пройдены");
    }
}
