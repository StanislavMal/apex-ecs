//! apex-examples: Lua scripting integration
//!
//! Demonstrates:
//! - `#[derive(Scriptable)]` — registering components for scripts
//! - `ScriptEngine::register_component::<T>()` — hooking into the engine
//! - `ScriptEngine::load_script_str()` — inline scripts
//! - `ScriptEngine::with_dir()` — loading from files + hot reload
//! - Game loop with `poll_hot_reload()` + `run()`
//! - Vec, HashMap, C-like enum in components
//!
//! Run:
//!   cargo run -p apex-examples --example scripting

use std::collections::HashMap;
use apex_core::prelude::*;
use apex_scripting::{ScriptEngine, Scriptable, WorldScriptingExt};

// ── Components ──────────────────────────────────────────────────────────────
//
// #[derive(Scriptable)] generates a ScriptableRegistrar:
//   - Position.new(x, y) constructor in Lua
//   - query({"Read:Position"}) recognizes the component
//   - entity.position.x is read/written through a Lua table
//   - Vec<T> → Lua table, HashMap<String, V> → Lua table
//   - C-like enum → namespace table (TileKind.Floor = 0)

#[derive(Component, Clone, Copy, Debug, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, Scriptable)]
struct Velocity { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, Scriptable)]
struct Health { current: f32, max: f32 }

// Component with Vec<String> — converted to a Lua table
#[derive(Component, Clone, Debug, Scriptable)]
struct Tags {
    list: Vec<String>,
}

// Component with HashMap<String, f32> — converted to a Lua table
#[derive(Component, Clone, Debug, Scriptable)]
struct Stats {
    values: HashMap<String, f32>,
}

// C-like enum — converted to a Lua namespace table
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Scriptable)]
enum TileKind { Floor, Wall, Water }

// ── Script ──────────────────────────────────────────────────────────────────

const GAME_SCRIPT: &str = r#"
function run()
    local dt = delta_time()

    -- Movement: Position += Velocity * dt
    for entity in query({"Read:Velocity", "Write:Position"}) do
        entity.position.x = entity.position.x + entity.velocity.x * dt
        entity.position.y = entity.position.y + entity.velocity.y * dt
        commit(entity)
    end

    -- Damage over HP
    for entity in query({"Write:Health"}) do
        entity.health.current = entity.health.current - 0.5 * dt
        commit(entity)
    end

    -- Reading Vec<String> from the Tags component
    for entity in query({"Read:Tags"}) do
        print("Entity " .. entity.entity .. " tags: " .. tostring(entity.tags.list))
    end

    -- Reading HashMap<String, f32> from the Stats component
    for entity in query({"Read:Stats"}) do
        if entity.stats.values["hp"] > 50.0 then
            print("Entity " .. entity.entity .. " has high HP")
        end
    end

    -- Comparing a C-like enum (TileKind = namespace table)
    for entity in query({"Read:TileKind"}) do
        if entity.tilekind == TileKind.Wall then
            print("Entity " .. entity.entity .. " is a wall")
        end
    end

    -- Spawn if there are few entities
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

    -- Despawn the dead
    for entity in query({"Read:Health"}) do
        if entity.health.current <= 0.0 then
            despawn(entity.entity)
        end
    end
end
"#;

fn main() {
    // Simple stderr logger for clarity
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    println!("=== Apex ECS — Lua Scripting ===\n");

    // ── World + ScriptEngine ────────────────────────────────────

    let mut world = World::new();
    let mut engine = ScriptEngine::new();

    // Single registration: both in the ECS and in the ScriptEngine
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable::<Velocity>(&mut engine);
    world.register_scriptable::<Health>(&mut engine);
    world.register_scriptable::<Tags>(&mut engine);
    world.register_scriptable::<Stats>(&mut engine);
    world.register_scriptable::<TileKind>(&mut engine);

    // Create a few entities manually
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

    println!("Initial entity count: {}", world.entity_count());

    // Load the inline script
    engine.set_auto_commit(true);
    engine.load_script_str("game", GAME_SCRIPT)
        .expect("script compilation error");

    println!("Scripts loaded: {:?}", engine.script_names().collect::<Vec<_>>());
    println!("Active script: '{}'", engine.active_script());

    // ── Game loop (5 ticks) ───────────────────────────────────────

    for tick in 1..=5 {
        println!("\n--- Tick {} ---", tick);

        // In a real game: engine.poll_hot_reload();
        engine.run(0.016, &mut world);
        world.tick();

        println!("Entities after tick: {}", world.entity_count());
    }

    // ── Variant B: hot reload from a directory ───────────────────
    //
    // In a real game:
    //
    //   let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));
    //   engine.register_component::<Position>(&world);
    //   engine.load_scripts().expect("script loading error");
    //
    //   loop {
    //       engine.poll_hot_reload();          // checks for file changes
    //       engine.run(dt, &mut world);        // runs function run()
    //       world.tick();
    //   }
    //
    // When a .lua file is saved in the editor, the script is reloaded
    // automatically without restarting the game.

    println!("\n=== Done ===");
    println!("Final entity count: {}", world.entity_count());
}
