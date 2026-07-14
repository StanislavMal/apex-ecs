//! Serde-tree component access from Lua (RT-2): `get_component` /
//! `set_component` globals.
//!
//! Thin scripting shims over the CORE dynamic serde-tree path
//! (`World::component_json` / `apply_component_json` — the same mechanism the
//! editor inspector's `edit_setComponent` uses): a component registered with
//! `register_component_serde_json` is readable/writable from Lua WITHOUT a
//! per-type `Scriptable` binding. Writes follow partial-merge semantics (the
//! Lua table carries ONLY the fields to change, [`apex_core::json_merge`]).
//!
//! Reads are immediate (`&World` is valid for the whole script run); writes
//! are DEFERRED like every other script mutation (drained into the system's
//! `Commands` buffer, applied at the sync point).

use apex_core::world::World;
use apex_core::Entity;

/// Read one component of one entity as a JSON tree (see
/// [`World::component_json`]). `Ok(None)` — the entity lacks the component.
pub(crate) fn component_json(
    world: &World,
    entity: Entity,
    name: &str,
) -> Result<Option<serde_json::Value>, String> {
    world.component_json(entity, name)
}

/// Apply a PARTIAL JSON value to one component of one entity (the deferred
/// half of `set_component`). Failures warn loudly (§0.2a) — a script write
/// must never crash the app.
pub(crate) fn apply_component_json(
    world: &mut World,
    entity: Entity,
    name: &str,
    partial: &serde_json::Value,
) {
    if let Err(e) = world.apply_component_json(entity, name, partial) {
        log::warn!("set_component('{name}'): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::component::Component;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    struct Health {
        current: u32,
        max: u32,
    }
    impl Component for Health {}

    fn world_with_health() -> (World, Entity) {
        let mut world = World::new();
        world.register_component_serde_json::<Health>();
        let e = world.spawn((Health { current: 10, max: 100 },));
        (world, e)
    }

    #[test]
    fn read_component_as_json_by_short_name() {
        let (world, e) = world_with_health();
        let value = component_json(&world, e, "Health").expect("resolved").expect("present");
        assert_eq!(value.pointer("/current").and_then(|v| v.as_u64()), Some(10));
        assert_eq!(value.pointer("/max").and_then(|v| v.as_u64()), Some(100));
    }

    #[test]
    fn partial_write_merges_into_component() {
        let (mut world, e) = world_with_health();
        apply_component_json(&mut world, e, "Health", &serde_json::json!({ "current": 42 }));
        assert_eq!(
            world.get::<Health>(e),
            Some(&Health { current: 42, max: 100 }),
            "untouched fields survive the partial write"
        );
    }

    #[test]
    fn write_to_absent_component_requires_complete_value() {
        #[derive(Serialize, Deserialize, Debug)]
        struct Tag {
            label: String,
        }
        impl Component for Tag {}

        let (mut world, e) = world_with_health();
        world.register_component_serde_json::<Tag>();
        // Complete value → insert.
        apply_component_json(&mut world, e, "Tag", &serde_json::json!({ "label": "boss" }));
        assert_eq!(world.get::<Tag>(e).map(|t| t.label.as_str()), Some("boss"));
    }

    #[test]
    fn unknown_and_unregistered_names_error() {
        struct Plain(#[allow(dead_code)] u32);
        impl Component for Plain {}

        let (mut world, e) = world_with_health();
        world.register_component::<Plain>();
        assert!(component_json(&world, e, "NoSuch").is_err(), "unknown name");
        assert!(component_json(&world, e, "Plain").is_err(), "no serde registration");
    }

    #[test]
    fn write_stamps_change_tick() {
        let (mut world, e) = world_with_health();
        world.advance_change_tick();
        let last_run = world.last_run_tick();
        assert!(!world
            .component_tick_of::<Health>(e)
            .expect("present")
            .is_newer_than(last_run));
        apply_component_json(&mut world, e, "Health", &serde_json::json!({ "current": 1 }));
        assert!(
            world
                .component_tick_of::<Health>(e)
                .expect("present")
                .is_newer_than(last_run),
            "a serde-path write is a change"
        );
    }
}
