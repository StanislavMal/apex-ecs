//! apex-examples: Lua scripting integration
//!
//! Демонстрирует:
//! - `#[derive(Scriptable)]` — регистрация компонентов для скриптов
//! - `ScriptEngine::register_component::<T>()` — подключение к движку
//! - `ScriptEngine::load_script_str()` — встроенные скрипты
//! - `ScriptEngine::with_dir()` — загрузка из файлов + хот-релоад
//! - Game loop с `poll_hot_reload()` + `run()`
//! - Vec, HashMap, C-like enum в компонентах
//!
//! Запуск:
//!   cargo run -p apex-examples --example scripting

use std::collections::HashMap;
use apex_core::prelude::*;
use apex_scripting::{ScriptEngine, Scriptable, WorldScriptingExt};

// ── Компоненты ─────────────────────────────────────────────────────────────
//
// #[derive(Scriptable)] генерирует ScriptableRegistrar:
//   - Position.new(x, y) конструктор в Lua
//   - query({"Read:Position"}) распознаёт компонент
//   - entity.position.x читается/пишется через Lua таблицу
//   - Vec<T> → Lua таблица, HashMap<String, V> → Lua таблица
//   - C-like enum → таблица-неймспейс (TileKind.Floor = 0)

#[derive(Component, Clone, Copy, Debug, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, Scriptable)]
struct Velocity { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, Scriptable)]
struct Health { current: f32, max: f32 }

// Компонент с Vec<String> — конвертируется в Lua таблицу
#[derive(Component, Clone, Debug, Scriptable)]
struct Tags {
    list: Vec<String>,
}

// Компонент с HashMap<String, f32> — конвертируется в Lua таблицу
#[derive(Component, Clone, Debug, Scriptable)]
struct Stats {
    values: HashMap<String, f32>,
}

// C-like enum — конвертируется в Lua таблицу-неймспейс
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Scriptable)]
enum TileKind { Floor, Wall, Water }

// ── Скрипт ────────────────────────────────────────────────────────────────

const GAME_SCRIPT: &str = r#"
function run()
    local dt = delta_time()

    -- Движение: Position += Velocity * dt
    for entity in query({"Read:Velocity", "Write:Position"}) do
        entity.position.x = entity.position.x + entity.velocity.x * dt
        entity.position.y = entity.position.y + entity.velocity.y * dt
        commit(entity)
    end

    -- Урон по HP
    for entity in query({"Write:Health"}) do
        entity.health.current = entity.health.current - 0.5 * dt
        commit(entity)
    end

    -- Чтение Vec<String> из компонента Tags
    for entity in query({"Read:Tags"}) do
        print("Entity " .. entity.entity .. " tags: " .. tostring(entity.tags.list))
    end

    -- Чтение HashMap<String, f32> из компонента Stats
    for entity in query({"Read:Stats"}) do
        if entity.stats.values["hp"] > 50.0 then
            print("Entity " .. entity.entity .. " has high HP")
        end
    end

    -- Сравнение C-like enum (TileKind = таблица-неймспейс)
    for entity in query({"Read:TileKind"}) do
        if entity.tilekind == TileKind.Wall then
            print("Entity " .. entity.entity .. " is a wall")
        end
    end

    -- Спавн если мало entity
    if entity_count() < 5 then
        spawn_entity({
            position = Position.new(0.0, 0.0),
            velocity = Velocity.new(1.0, 0.5),
            health   = Health.new(100.0, 100.0),
            tags     = Tags.new({"enemy", "boss"}),
            stats    = Stats.new({ hp = 100.0, mp = 50.0 }),
            tilekind = TileKind.Floor,
        })
        print("Spawned entity, total: " .. entity_count())
    end

    -- Деспавн мёртвых
    for entity in query({"Read:Health"}) do
        if entity.health.current <= 0.0 then
            despawn(entity.entity)
        end
    end
end
"#;

fn main() {
    // Простой stderr-логгер для наглядности
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    println!("=== Apex ECS — Lua Scripting ===\n");

    // ── Мир + ScriptEngine ──────────────────────────────────────

    let mut world = World::new();
    let mut engine = ScriptEngine::new();

    // Единая регистрация: и в ECS, и в ScriptEngine
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable::<Velocity>(&mut engine);
    world.register_scriptable::<Health>(&mut engine);
    world.register_scriptable::<Tags>(&mut engine);
    world.register_scriptable::<Stats>(&mut engine);
    world.register_scriptable::<TileKind>(&mut engine);

    // Создаём несколько entity вручную
    world.spawn((
        Position { x: 0.0,  y: 0.0 },
        Velocity { x: 1.0,  y: 0.5 },
        Health   { current: 100.0, max: 100.0 },
    ));
    world.spawn((
        Position { x: 5.0,  y: 2.0 },
        Velocity { x: -0.5, y: 1.0 },
        Health   { current: 50.0,  max: 50.0  },
    ));

    println!("Начальное количество entity: {}", world.entity_count());

    // Загружаем встроенный скрипт
    engine.set_auto_commit(true);
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
    //       engine.run(dt, &mut world);        // выполняет function run()
    //       world.tick();
    //   }
    //
    // При сохранении .lua файла в редакторе — скрипт перезагружается
    // автоматически без перезапуска игры.

    println!("\n=== Завершено ===");
    println!("Итоговое количество entity: {}", world.entity_count());
}
