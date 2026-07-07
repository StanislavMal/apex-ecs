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
