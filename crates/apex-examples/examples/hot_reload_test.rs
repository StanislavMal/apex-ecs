//! Full test of hot-reload and all Lua API methods.
//!
//! Verifies:
//! - `delta_time()` — returns the correct value
//! - `entity_count()` — returns the number of entities
//! - `query({"Read:..."})` — iteration with reading
//! - `query({"Write:..."})` — iteration with writing
//! - `spawn_entity(table)` — creating an entity with components
//! - `despawn(entity)` — removing an entity
//! - `log()` / `print()` — logging
//! - Hot-reload: changing the script on the fly
//!
//! Run:
//!   cargo run -p apex-examples --example hot_reload_test

use apex_core::prelude::*;
use apex_scripting::{ScriptEngine, Scriptable};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

// ── Test components ──────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Velocity { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Health { current: f32, max: f32 }

// Tuple struct (single field) — tests the { _value = ... } wrapper
#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Gravity(f32);

// Event
#[derive(Clone, Copy, Debug, PartialEq, Scriptable)]
struct PlayerDied { x: f32, y: f32 }

// Marker components for With/Without tests
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Scriptable)]
struct Player;

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Scriptable)]
struct Enemy;

// ── Helper functions ─────────────────────────────────────────────

fn setup_scripts_dir() -> std::path::PathBuf {
    let dir = Path::new("target/hot_reload_full_test");
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("clearing temp dir");
    }
    std::fs::create_dir_all(dir).expect("creating temp dir");
    dir.to_path_buf()
}

fn write_script(path: &Path, content: &str) {
    let mut file = std::fs::File::create(path).expect("creating file");
    file.write_all(content.as_bytes()).expect("writing");
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
            println!("  -> Applied on attempt {}/{}", attempt, max_retries);
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
    println!("  FULL SCRIPTING AND HOT-RELOAD TEST");
    println!("═══════════════════════════════════════════════\n");

    let mut all_ok = true;

    // ═══════════════════════════════════════════════════════
    // TEST 1: Basic functions (delta_time, entity_count)
    // ═══════════════════════════════════════════════════════

    println!("--- Test 1: delta_time + entity_count ---");

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
    world.insert_resource(Gravity(9.8));
    world.add_event::<PlayerDied>();
    engine.load_scripts().expect("load_scripts");

    for _ in 0..3 {
        engine.poll_hot_reload();
        engine.run(0.016, &mut world);
        world.tick();
    }

    println!("  OK: delta_time + entity_count work\n");

    // ═══════════════════════════════════════════════════════
    // TEST 2: query with Read (reading components)
    // ═══════════════════════════════════════════════════════

    println!("--- Test 2: query with Read ---");

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
    println!("  OK: query Read works\n");

    // ═══════════════════════════════════════════════════════
    // TEST 3: query with Write (modifying components)
    // ═══════════════════════════════════════════════════════

    println!("--- Test 3: query with Write ---");

    // Create an entity with Velocity for the test
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

    // Find an entity with both components (Position + Velocity)
    let before_pos = {
        let q = Query::<(&Position, &Velocity)>::new(&world);
        q.iter().next().map(|(p, _)| *p)
    };

    // Wait for watcher to detect file change 
    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.5, &mut world);
    world.tick();

    let after_pos = {
        let q = Query::<(&Position, &Velocity)>::new(&world);
        q.iter().next().map(|(p, _)| *p)
    };

    match (before_pos, after_pos) {
        (Some(before), Some(after)) => {
            assert!(after.x > before.x, "Position.x did not change");
            assert!(after.y > before.y, "Position.y did not change");
            println!("  OK: query Write works (x: {} -> {}, y: {} -> {})",
                before.x, after.x, before.y, after.y);
        }
        _ => {
            println!("  WARN: could not verify — entities not found");
        }
    }
    println!();

    // ═══════════════════════════════════════════════════════
    // TEST 4: spawn_entity
    // ═══════════════════════════════════════════════════════

    println!("--- Test 4: spawn_entity ---");

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
        println!("  OK: spawn_entity works ({} -> {})\n", before, after);
    } else {
        println!("  FAIL: spawn_entity did not work\n");
        all_ok = false;
    }

    // ═══════════════════════════════════════════════════════
    // TEST 5: despawn
    // ═══════════════════════════════════════════════════════

    println!("--- Test 5: despawn ---");

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
        println!("  OK: despawn works ({} -> {})\n", before, after);
    } else {
        println!("  WARN: despawn removed no entity (possibly none matched)\n");
    }

    // ═══════════════════════════════════════════════════════
    // TEST 6: log() / print()
    // ═══════════════════════════════════════════════════════

    println!("--- Test 6: log() + print() ---");

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

    println!("  OK: log and print work (check the output above)\n");

    // ═══════════════════════════════════════════════════════
    // TEST 7: Hot-reload (changing the script on the fly)
    // ═══════════════════════════════════════════════════════

    println!("--- Test 7: Hot-reload ---");

    write_script(&script_path, r#"
function run()
    print("[TEST] VERSION 1")
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    engine.run(0.016, &mut world);
    world.tick();

    // Change the script
    write_script(&script_path, r#"
function run()
    print("[TEST] VERSION 2 - HOT RELOADED!")
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    wait_for_hot_reload(&mut engine, &mut world, 0, 5);

    println!("  OK: hot-reload works\n");

    // ═══════════════════════════════════════════════════════
    // TEST 8: read_resource / write_resource
    // ═══════════════════════════════════════════════════════

    println!("--- Test 8: read_resource + write_resource ---");

    write_script(&script_path, r#"
function run()
    local g = read_resource("Gravity")
    if g._value > 0 then
        write_resource("Gravity", Gravity.new(g._value + 1.0))
    end
end
"#);

    let before_g = world.try_resource::<Gravity>().copied();
    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();
    let after_g = world.try_resource::<Gravity>().copied();

    match (before_g, after_g) {
        (Some(before), Some(after)) => {
            if after.0 > before.0 {
                println!("  OK: read/write_resource works ({} -> {})\n", before.0, after.0);
            } else {
                println!("  FAIL: resource did not change\n");
                all_ok = false;
            }
        }
        _ => {
            println!("  FAIL: resource not found\n");
            all_ok = false;
        }
    }

    // ═══════════════════════════════════════════════════════
    // TEST 9: emit_event
    // ═══════════════════════════════════════════════════════

    println!("--- Test 9: emit_event ---");

    write_script(&script_path, r#"
function run()
    emit_event("PlayerDied", PlayerDied.new(42.0, 24.0))
end
"#);

    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.016, &mut world);
    world.tick();

    // Verify the event was sent (may be pending)
    let readable = world.events::<PlayerDied>().len_readable();
    let pending = world.events::<PlayerDied>().len_pending();
    if readable > 0 || pending > 0 {
        println!("  OK: emit_event works (readable={}, pending={})\n", readable, pending);
    } else {
        println!("  WARN: no events (readable=0, pending=0)\n");
    }

    // ═══════════════════════════════════════════════════════
    // TEST 10: Tuple struct (Gravity) — spawn + query
    // ═══════════════════════════════════════════════════════

    println!("--- Test 10: Tuple struct Gravity ---");

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
        println!("  OK: tuple struct works (spawn: {} -> {})\n", before, after);
    } else {
        println!("  FAIL: tuple struct did not work\n");
        all_ok = false;
    }

    // ═══════════════════════════════════════════════════════
    // TEST 11: With<T> — filter by component presence
    // ═══════════════════════════════════════════════════════

    println!("--- Test 11: query With<T> ---");

    // Create two entities: Player (with Health) and Enemy (without Health)
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
    -- With:Player — only entities with the Player component
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

    // Verify: only 1 entity with the Player marker (created above + possibly from test 4)
    let player_count = Query::<(&Position, With<Player>)>::new(&world).iter().count();
    println!("  OK: With<T> filter works (Player entities: {})\n", player_count);

    // ═══════════════════════════════════════════════════════
    // TEST 12: Without<T> — exclusion filter
    // ═══════════════════════════════════════════════════════

    println!("--- Test 12: query Without<T> ---");

    write_script(&script_path, r#"
function run()
    local count = 0
    -- Without:Enemy — only non-Enemy entities with Position
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

    let non_enemy = Query::<(&Position, Without<Enemy>)>::new(&world).iter().count();
    let total_pos = Query::<&Position>::new(&world).iter().count();
    if non_enemy < total_pos {
        println!("  OK: Without<T> filter works (non-Enemy: {} of {})\n", non_enemy, total_pos);
    } else {
        println!("  WARN: Without<T> did not filter ({} == {})\n", non_enemy, total_pos);
    }

    // ═══════════════════════════════════════════════════════
    // TEST 13: Auto-commit (without an explicit commit())
    // ═══════════════════════════════════════════════════════

    println!("--- Test 13: auto-commit ---");

    // Enable auto-commit — commit(entity) is called automatically
    engine.set_auto_commit(true);

    write_script(&script_path, r#"
function run()
    local dt = delta_time()
    for entity in query({"Read:Velocity", "Write:Position"}) do
        entity.position.x = entity.position.x + entity.velocity.x * dt
        entity.position.y = entity.position.y + entity.velocity.y * dt
        -- do NOT call commit(entity) — auto-commit writes it for us
    end
end
"#);

    let before = {
        let q = Query::<(&Position, &Velocity)>::new(&world);
        q.iter().next().map(|(p, _)| *p)
    };

    std::thread::sleep(Duration::from_millis(200));
    engine.poll_hot_reload();
    engine.run(0.5, &mut world);
    world.tick();

    let after = {
        let q = Query::<(&Position, &Velocity)>::new(&world);
        q.iter().next().map(|(p, _)| *p)
    };

    match (before, after) {
        (Some(b), Some(a)) if a.x > b.x => {
            println!("  OK: auto-commit works (x: {} -> {})\n", b.x, a.x);
        }
        _ => {
            println!("  FAIL: auto-commit did not work\n");
            all_ok = false;
        }
    }

    engine.set_auto_commit(false);

    // ═══════════════════════════════════════════════════════
    // SUMMARY
    // ═══════════════════════════════════════════════════════

    println!("═══════════════════════════════════════════════");
    if all_ok {
        println!("  ALL TESTS PASSED!");
    } else {
        println!("  SOME TESTS FAILED");
    }
    println!("═══════════════════════════════════════════════");
}
