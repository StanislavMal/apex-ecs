//! End-to-end tests for phase-B script SYSTEMS: a Lua `system{ name, query, fn }`
//! declaration registered as a first-class scheduler system (NonSend, main
//! thread), driven by `Scheduler::run`, mutating a real `World`.
//!
//! These complement `engine_e2e.rs` (the monolithic `run()` path). Here the Lua
//! function is invoked BY THE SCHEDULER through the runner built in
//! `ScriptEngine::register_systems`, so they exercise the declared-access path
//! (the runner installs the system's declared component ids; `query`/`commit`
//! resolve only those) and the `ScriptVm` serialization token.

use apex_core::prelude::*;
use apex_scheduler::Scheduler;
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

#[derive(Clone, Debug, PartialEq, Scriptable)]
struct Score {
    value: i32,
}

#[derive(Clone, Debug, Scriptable)]
struct Damage {
    amount: i32,
}

/// Flagship: a `system{}` declaration runs as a scheduler system and its
/// committed write persists in the world.
#[test]
fn script_system_write_persists_through_scheduler() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable::<Velocity>(&mut engine);

    let e = world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 2.0, y: 3.0 }));

    engine
        .load_script_str(
            "integrate",
            r#"
            system{
                name = "integrate",
                query = {"Read:Velocity", "Write:Position"},
                fn = function()
                    for e in query({"Read:Velocity", "Write:Position"}) do
                        e.position.x = e.position.x + e.velocity.x
                        e.position.y = e.position.y + e.velocity.y
                        commit(e)
                    end
                end,
            }
            "#,
        )
        .expect("script must compile");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);

    engine.set_delta_time(1.0);
    sched.run(&mut world);

    assert_eq!(
        world.get::<Position>(e),
        Some(&Position { x: 2.0, y: 3.0 }),
        "the script system's committed write persisted through the scheduler"
    );

    // Run a second frame: the runner is reused (persistent VM + fn), the write
    // applies again.
    sched.run(&mut world);
    assert_eq!(
        world.get::<Position>(e),
        Some(&Position { x: 4.0, y: 6.0 }),
        "the runner persists across frames and re-applies each frame"
    );
}

/// Two Lua systems both declare a write on the `ScriptVm` token, so the scheduler
/// serializes them (one VM must never run two systems at once). With disjoint
/// component access both writes still land — the token serializes without
/// dropping either system.
#[test]
fn two_script_systems_both_apply() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable::<Velocity>(&mut engine);

    let e = world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 0.0, y: 0.0 }));

    engine
        .load_script_str(
            "two_systems",
            r#"
            system{
                name = "move_pos",
                query = {"Write:Position"},
                fn = function()
                    for e in query({"Write:Position"}) do
                        e.position.x = e.position.x + 1.0
                        commit(e)
                    end
                end,
            }
            system{
                name = "move_vel",
                query = {"Write:Velocity"},
                fn = function()
                    for e in query({"Write:Velocity"}) do
                        e.velocity.y = e.velocity.y + 5.0
                        commit(e)
                    end
                end,
            }
            "#,
        )
        .expect("script must compile");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);
    sched.run(&mut world);

    assert_eq!(world.get::<Position>(e), Some(&Position { x: 1.0, y: 0.0 }), "move_pos ran");
    assert_eq!(world.get::<Velocity>(e), Some(&Velocity { x: 0.0, y: 5.0 }), "move_vel ran");
}

/// Enforcement: a script system that ACCESSES a component it did not DECLARE gets
/// an empty query for it (§0.2a) — the undeclared write never lands, while the
/// declared one does.
#[test]
fn undeclared_component_access_is_refused() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable::<Velocity>(&mut engine);

    let e = world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 0.0, y: 0.0 }));

    // Declares only Write:Position, but the body ALSO tries to query+write
    // Velocity (undeclared). The Velocity query must yield nothing; Position must
    // still be written.
    engine
        .load_script_str(
            "leaky",
            r#"
            system{
                name = "leaky",
                query = {"Write:Position"},
                fn = function()
                    for e in query({"Write:Position"}) do
                        e.position.x = e.position.x + 1.0
                        commit(e)
                    end
                    for e in query({"Write:Velocity"}) do
                        e.velocity.y = e.velocity.y + 99.0
                        commit(e)
                    end
                end,
            }
            "#,
        )
        .expect("script must compile");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);
    sched.run(&mut world);

    assert_eq!(
        world.get::<Position>(e),
        Some(&Position { x: 1.0, y: 0.0 }),
        "declared Write:Position applied"
    );
    assert_eq!(
        world.get::<Velocity>(e),
        Some(&Velocity { x: 0.0, y: 0.0 }),
        "undeclared Velocity access yielded nothing — no write landed"
    );
}

/// A script SYSTEM spawns entities through its per-system Commands slot; the
/// spawn persists (with its component) after the scheduler applies the slot.
#[test]
fn script_system_spawn_persists_through_commands() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);

    engine
        .load_script_str(
            "spawner",
            r#"
            system{
                name = "spawner",
                query = {"Write:Position"},
                fn = function()
                    spawn_entity({ position = Position.new(5.0, 6.0) })
                end,
            }
            "#,
        )
        .expect("script must compile");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);
    sched.run(&mut world);

    let mut found = Vec::new();
    Query::<Read<Position>>::new(&world).for_each(|_, p| found.push(*p));
    assert_eq!(
        found,
        vec![Position { x: 5.0, y: 6.0 }],
        "the script system's deferred spawn was applied via the per-system Commands slot"
    );
}

/// A script SYSTEM despawns a matched entity through its per-system Commands
/// slot; the despawn applies after the stage.
#[test]
fn script_system_despawn_applies_through_commands() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);

    world.spawn((Position { x: 1.0, y: 0.0 },));
    world.spawn((Position { x: 2.0, y: 0.0 },));

    engine
        .load_script_str(
            "reaper",
            r#"
            system{
                name = "reaper",
                query = {"Read:Position"},
                fn = function()
                    for e in query({"Read:Position"}) do
                        if e.position.x > 1.5 then
                            despawn(e.entity)
                        end
                    end
                end,
            }
            "#,
        )
        .expect("script must compile");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);
    sched.run(&mut world);

    let mut remaining = Vec::new();
    Query::<Read<Position>>::new(&world).for_each(|_, p| remaining.push(p.x));
    assert_eq!(remaining, vec![1.0], "the x>1.5 entity was despawned via the per-system Commands slot");
}

/// Determinism gate: a script system spawning through its per-system Commands
/// slot assigns identical entity ids run-to-run under `set_deterministic_spawn`
/// (D8b seeds a rank-deterministic id block because the script access declares
/// `uses_commands`).
#[test]
fn script_system_spawns_are_deterministic() {
    fn run_once() -> Vec<Entity> {
        let mut world = World::new();
        let mut engine = ScriptEngine::new();
        world.register_scriptable::<Position>(&mut engine);
        // Seed some existing entities so the spawned ids are non-trivial.
        world.spawn((Position { x: 0.0, y: 0.0 },));
        world.spawn((Position { x: 1.0, y: 1.0 },));

        engine
            .load_script_str(
                "burst",
                r#"
                system{
                    name = "burst",
                    query = {"Write:Position"},
                    fn = function()
                        for i = 1, 5 do
                            spawn_entity({ position = Position.new(9.0, 9.0) })
                        end
                    end,
                }
                "#,
            )
            .expect("script must compile");

        let mut sched = Scheduler::new();
        sched.set_deterministic_spawn(true);
        engine.register_systems(&mut sched);
        // Two frames of bursts, to exercise block reuse across frames.
        sched.run(&mut world);
        sched.run(&mut world);

        // Collect the entities matching the spawn marker (9.0, 9.0), sorted for a
        // stable comparison independent of iteration order.
        let mut ids = Vec::new();
        Query::<Read<Position>>::new(&world).for_each(|e, p| {
            if *p == (Position { x: 9.0, y: 9.0 }) {
                ids.push(e);
            }
        });
        ids.sort_by_key(|e| (e.index(), e.generation()));
        ids
    }

    let a = run_once();
    let b = run_once();
    assert_eq!(a.len(), 10, "5 spawns × 2 frames");
    assert_eq!(a, b, "script-system spawn ids are deterministic run-to-run (D8b)");
}

/// A script SYSTEM writes a resource and emits an event; both are applied through
/// the per-system Commands slot (`commands.add`) after the stage — no `&mut World`
/// needed during the concurrent run. Closes the resource/event tail on the
/// golden path (parity with the monolithic `run()`).
#[test]
fn script_system_resource_and_event_apply_through_commands() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);
    world.register_scriptable_resource::<Score>(&mut engine);
    world.register_scriptable_event::<Damage>(&mut engine);
    world.insert_resource(Score { value: 1 });

    engine
        .load_script_str(
            "sfx",
            r#"
            system{
                name = "sfx",
                query = {"Read:Position"},
                fn = function()
                    local s = read_resource("Score")
                    s.value = s.value + 41
                    write_resource("Score", s)
                    emit_event("Damage", Damage.new(7))
                end,
            }
            "#,
        )
        .expect("script must compile");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);
    sched.run(&mut world);

    assert_eq!(
        world.try_resource::<Score>(),
        Some(&Score { value: 42 }),
        "the script system's resource write was applied via the per-system Commands slot"
    );
    assert_eq!(
        world.events::<Damage>().len(),
        1,
        "the script system's emitted event reached the world's Damage buffer"
    );
}

/// Hot-reload: re-registering after a script changes REPLACES the old script
/// systems (they are removed from the scheduler), so the new behavior runs and
/// the old does not. If removal failed, both would run and the value would be
/// wrong.
#[test]
fn reregister_replaces_old_script_systems() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    world.register_scriptable::<Position>(&mut engine);
    let e = world.spawn((Position { x: 0.0, y: 0.0 },));

    // v1: bump x by 1.
    engine
        .load_script_str(
            "gameplay",
            r#"
            system{
                name = "bump",
                query = {"Write:Position"},
                fn = function()
                    for e in query({"Write:Position"}) do
                        e.position.x = e.position.x + 1.0
                        commit(e)
                    end
                end,
            }
            "#,
        )
        .expect("v1 compiles");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);
    sched.run(&mut world);
    assert_eq!(world.get::<Position>(e).map(|p| p.x), Some(1.0), "v1 (+1) ran");

    // Reload the SAME script name with new behavior: bump x by 10.
    engine
        .load_script_str(
            "gameplay",
            r#"
            system{
                name = "bump",
                query = {"Write:Position"},
                fn = function()
                    for e in query({"Write:Position"}) do
                        e.position.x = e.position.x + 10.0
                        commit(e)
                    end
                end,
            }
            "#,
        )
        .expect("v2 compiles");

    // Re-register: the old "bump" system must be removed and v2 registered.
    engine.register_systems(&mut sched);
    sched.run(&mut world);

    assert_eq!(
        world.get::<Position>(e).map(|p| p.x),
        Some(11.0),
        "only v2 (+10) ran after re-registration (1 + 10); the old v1 was removed \
         (both running would give 12)"
    );
}

/// A `system{}` referencing an UNREGISTERED component is refused registration
/// (§0.2a: registering it with an under-declared access would be a data race), so
/// it never runs and the world is untouched.
#[test]
fn unregistered_component_refuses_registration() {
    let mut world = World::new();
    let mut engine = ScriptEngine::new();
    // Position is registered; "Ghost" is NOT.
    world.register_scriptable::<Position>(&mut engine);

    let e = world.spawn((Position { x: 7.0, y: 7.0 },));

    engine
        .load_script_str(
            "ghost",
            r#"
            system{
                name = "ghost",
                query = {"Write:Position", "Read:Ghost"},
                fn = function()
                    for e in query({"Write:Position"}) do
                        e.position.x = 0.0
                        commit(e)
                    end
                end,
            }
            "#,
        )
        .expect("script must compile");

    let mut sched = Scheduler::new();
    engine.register_systems(&mut sched);
    sched.run(&mut world);

    assert_eq!(
        world.get::<Position>(e),
        Some(&Position { x: 7.0, y: 7.0 }),
        "the system was refused (unregistered Ghost) — it never ran, world untouched"
    );
}
