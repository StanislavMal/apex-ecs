//! End-to-end tests for `ScriptEngine`: load a Lua script, run it against a
//! real `World`, and assert the WORLD CHANGED as the script intended.
//!
//! The inline unit tests pin narrow invariants (no per-call leaks, forged-meta
//! rejection, id generation round-trip). None drives the full pipeline —
//! register component/resource/event -> compile a script -> run -> observe the
//! world. These do, exercising the Lua API surface a real game script uses
//! (query/commit, spawn, despawn, resources, events) and the loud failure
//! modes (syntax error, runtime error).

use apex_core::prelude::*;
use apex_core::query::Read;
use apex_scripting::{ScriptEngine, Scriptable, WorldScriptingExt};

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Velocity {
    x: f32,
    y: f32,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Scriptable)]
struct Health {
    current: f32,
    max: f32,
}

#[derive(Clone, Debug, PartialEq, Scriptable)]
struct Score {
    value: i32,
}

#[derive(Clone, Debug, Scriptable)]
struct Damage {
    amount: i32,
}

// ── query + write + commit mutates the world (flagship) ───────────

#[test]
fn script_query_write_commit_persists_to_world() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable::<Velocity>(&mut engine);

    let e = world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 2.0, y: 3.0 }));

    engine
        .load_script_str(
            "integrate",
            r#"
            function run()
                for entity in query({"Read:Velocity", "Write:Position"}) do
                    entity.position.x = entity.position.x + entity.velocity.x
                    entity.position.y = entity.position.y + entity.velocity.y
                    commit(entity)
                end
            end
        "#,
        )
        .expect("script must compile");

    engine.run(1.0, &mut world);

    assert_eq!(
        world.get::<Position>(e),
        Some(&Position { x: 2.0, y: 3.0 }),
        "the script's committed write must persist in the world"
    );
}

// ── Changed: reactive filter is actually applied ──────────────────

#[test]
fn script_changed_filter_restricts_the_query() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);

    world.spawn((Position { x: 0.0, y: 0.0 },));
    world.spawn((Position { x: 1.0, y: 1.0 },));
    // Advance the change tick so the two spawns are OLD relative to the query's
    // baseline (world.last_run_tick()): nothing has changed "since the last run".
    world.advance_change_tick();
    world.advance_change_tick();

    // The script spawns a marker per MATCHED entity. With `Changed:Position` and
    // no recent mutation, zero rows match, so no markers are spawned. If the
    // filter were silently dropped, all 2 rows would match and the count would
    // grow — this pins that `Changed:` reaches the core filter.
    engine
        .load_script_str(
            "reactive",
            r#"
            function run()
                for _ in query({"Read:Position", "Changed:Position"}) do
                    spawn_entity({ position = Position.new(9.0, 9.0) })
                end
            end
        "#,
        )
        .expect("script must compile");

    engine.run(0.016, &mut world);

    assert_eq!(
        world.entity_count(),
        2,
        "Changed:Position matched nothing (no recent change) — the filter is applied, not ignored"
    );
}

// ── spawn_entity from Lua ─────────────────────────────────────────

#[test]
fn script_spawn_entity_creates_entity_with_components() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);

    engine
        .load_script_str(
            "spawner",
            r#"
            function run()
                if entity_count() == 0 then
                    spawn_entity({ position = Position.new(3.0, 4.0) })
                end
            end
        "#,
        )
        .unwrap();

    engine.run(0.016, &mut world);

    assert_eq!(world.entity_count(), 1, "the script spawned exactly one entity");
    let mut found = None;
    world.query::<Read<Position>>().for_each(|_e, p| found = Some(*p));
    assert_eq!(
        found,
        Some(Position { x: 3.0, y: 4.0 }),
        "the spawned entity carries the component values the script set"
    );
}

// ── despawn from Lua ──────────────────────────────────────────────

#[test]
fn script_despawn_removes_matching_entities() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Health>(&mut engine);

    world.spawn((Health { current: 100.0, max: 100.0 },)); // survives
    world.spawn((Health { current: 0.0, max: 100.0 },)); // culled
    assert_eq!(world.entity_count(), 2);

    engine
        .load_script_str(
            "reaper",
            r#"
            function run()
                for entity in query({"Read:Health"}) do
                    if entity.health.current <= 0.0 then
                        despawn(entity.entity)
                    end
                end
            end
        "#,
        )
        .unwrap();

    engine.run(0.016, &mut world);

    assert_eq!(
        world.entity_count(),
        1,
        "only the zero-health entity should have been despawned"
    );
}

// ── resources: read then write back through the engine ────────────

#[test]
fn script_resource_read_write_roundtrips() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable_resource::<Score>(&mut engine);
    world.insert_resource(Score { value: 7 });

    engine
        .load_script_str(
            "scorer",
            r#"
            function run()
                local s = read_resource("Score")
                s.value = s.value + 1
                write_resource("Score", s)
            end
        "#,
        )
        .unwrap();

    engine.run(0.016, &mut world);

    assert_eq!(
        world.try_resource::<Score>(),
        Some(&Score { value: 8 }),
        "the script read the resource, mutated it, and wrote it back"
    );
}

// ── events: emit from Lua reaches the world's event buffer ────────

#[test]
fn script_emit_event_reaches_world() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable_event::<Damage>(&mut engine);

    engine
        .load_script_str(
            "attacker",
            r#"
            function run()
                emit_event("Damage", Damage.new(5))
            end
        "#,
        )
        .unwrap();

    engine.run(0.016, &mut world);

    assert_eq!(
        world.events::<Damage>().len(),
        1,
        "the emitted event must land in the world's Damage buffer"
    );
}

// ── error paths (loud, non-fatal) ─────────────────────────────────

#[test]
fn syntax_error_is_reported_on_load() {
    let mut engine = ScriptEngine::new();
    let result = engine.load_script_str("broken", "function run( this is not lua");
    assert!(
        result.is_err(),
        "a script that does not compile must return an error, not load silently"
    );
}

#[test]
fn runtime_error_is_non_fatal_and_still_flushes_prior_work() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);

    // Compiles fine; blows up at run time AFTER queuing a spawn.
    engine
        .load_script_str(
            "boom",
            r#"
            function run()
                spawn_entity({ position = Position.new(1.0, 1.0) })
                error("intentional runtime failure")
            end
        "#,
        )
        .unwrap();

    engine.run(0.016, &mut world);

    // Contract pinned here: a Lua runtime error is NON-FATAL — run() logs it and
    // returns without panicking, and it is NOT transactional: deferred work
    // requested BEFORE the error (the spawn) is still flushed at the end of the
    // frame. This catches a regression that panics on script error or that
    // silently drops the whole frame's queued work.
    assert_eq!(
        world.entity_count(),
        1,
        "runtime error must be non-fatal (no panic) and still flush work queued before it"
    );
}

// ── set_active selects which script runs ──────────────────────────

#[test]
fn set_active_selects_the_running_script() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable::<Velocity>(&mut engine);

    // Two scripts with distinct, observable effects.
    engine
        .load_script_str(
            "adds_position",
            r#"function run() spawn_entity({ position = Position.new(1.0, 1.0) }) end"#,
        )
        .unwrap();
    engine
        .load_script_str(
            "adds_velocity",
            r#"function run() spawn_entity({ velocity = Velocity.new(9.0, 9.0) }) end"#,
        )
        .unwrap();

    engine.set_active("adds_velocity").expect("script exists");
    engine.run(0.016, &mut world);

    // Only the active script ran: the spawned entity has Velocity, not Position.
    let mut pos_count = 0;
    world.query::<Read<Position>>().for_each(|_e, _p| pos_count += 1);
    let mut vel_count = 0;
    world.query::<Read<Velocity>>().for_each(|_e, _v| vel_count += 1);
    assert_eq!(pos_count, 0, "the inactive Position script must not have run");
    assert_eq!(vel_count, 1, "the active Velocity script must have run");

    // Selecting a non-existent script is a loud error, not a silent no-op.
    assert!(engine.set_active("does_not_exist").is_err());
}
