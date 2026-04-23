//! apex-examples: Rhai scripting integration
//!
//! Демонстрирует:
//! - `#[derive(Scriptable)]` — регистрация компонентов для скриптов
//! - `ScriptEngine::register_component::<T>()` — подключение к движку
//! - `ScriptEngine::load_script_str()` — встроенные скрипты
//! - `ScriptEngine::with_dir()` — загрузка из файлов + хот-релоад
//! - Game loop с `poll_hot_reload()` + `run()`
//!
//! Запуск:
//!   cargo run -p apex-examples --example scripting

use apex_core::prelude::*;
use apex_scripting::{ScriptEngine, Scriptable};

// ── Компоненты ─────────────────────────────────────────────────────────────
//
// #[derive(Scriptable)] генерирует ScriptableRegistrar:
//   - Position(x, y) конструктор в Rhai
//   - query(["Read:Position"]) распознаёт компонент
//   - entity.position.x читается/пишется через Dynamic Map

#[derive(Clone, Copy, Debug, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy, Debug, Scriptable)]
struct Velocity { x: f32, y: f32 }

#[derive(Clone, Copy, Debug, Scriptable)]
struct Health { current: f32, max: f32 }

// ── Скрипт ────────────────────────────────────────────────────────────────

const GAME_SCRIPT: &str = r#"
fn run() {
    let dt = delta_time();

    // Движение: Position += Velocity * dt
    for entity in query(["Read:Velocity", "Write:Position"]) {
        entity.position.x += entity.velocity.x * dt;
        entity.position.y += entity.velocity.y * dt;
    }

    // Урон по HP
    for entity in query(["Write:Health"]) {
        entity.health.current -= 0.5 * dt;
    }

    // Спавн если мало entity
    if entity_count() < 5 {
        spawn(#{
            position: Position(0.0, 0.0),
            velocity: Velocity(1.0, 0.5),
            health:   Health(100.0, 100.0),
        });
        print(`Spawned entity, total: ${entity_count()}`);
    }

    // Деспавн мёртвых
    for entity in query(["Read:Health"]) {
        if entity.health.current <= 0.0 {
            despawn(entity.entity);
        }
    }
}
"#;

fn main() {
    // Простой stderr-логгер для наглядности
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    println!("=== Apex ECS — Rhai Scripting ===\n");

    // ── Мир ──────────────────────────────────────────────────────

    let mut world = World::new();

    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Создаём несколько entity вручную
    world.spawn_bundle((
        Position { x: 0.0,  y: 0.0 },
        Velocity { x: 1.0,  y: 0.5 },
        Health   { current: 100.0, max: 100.0 },
    ));
    world.spawn_bundle((
        Position { x: 5.0,  y: 2.0 },
        Velocity { x: -0.5, y: 1.0 },
        Health   { current: 50.0,  max: 50.0  },
    ));

    println!("Начальное количество entity: {}", world.entity_count());

    // ── ScriptEngine ─────────────────────────────────────────────
    //
    // Вариант A: загрузка из строки (тесты, встроенные скрипты)
    let mut engine = ScriptEngine::new();

    // Регистрируем компоненты — ПОСЛЕ world.register_component::<T>()
    engine.register_component::<Position>(&world);
    engine.register_component::<Velocity>(&world);
    engine.register_component::<Health>(&world);

    // Загружаем встроенный скрипт
    engine.load_script_str("game", GAME_SCRIPT)
        .expect("ошибка компиляции скрипта");

    println!("Скрипты загружены: {:?}", engine.script_names().collect::<Vec<_>>());
    println!("Активный скрипт: '{}'", engine.active_script());

    // ── Game loop (5 тиков) ───────────────────────────────────────

    for tick in 1..=5 {
        println!("\n--- Тик {} ---", tick);

        // В реальной игре: engine.poll_hot_reload();
        engine.run(0.016, &mut world);
        world.tick();

        println!("Entity после тика: {}", world.entity_count());
    }

    // ── Вариант B: хот-релоад из директории ──────────────────────
    //
    // В реальной игре:
    //
    //   let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));
    //   engine.register_component::<Position>(&world);
    //   engine.load_scripts().expect("ошибка загрузки скриптов");
    //
    //   loop {
    //       engine.poll_hot_reload();          // проверяет изменения файлов
    //       engine.run(dt, &mut world);        // выполняет fn run()
    //       world.tick();
    //   }
    //
    // При сохранении .rhai файла в редакторе — скрипт перезагружается
    // автоматически без перезапуска игры.

    println!("\n=== Завершено ===");
    println!("Итоговое количество entity: {}", world.entity_count());
}