//! Serde-tree component access from Lua (RT-2): `get_component` /
//! `set_component` globals.
//!
//! This is the SAME reflective path the editor inspector uses
//! (`ComponentSerdeFns` → JSON value tree → partial deep-merge →
//! `insert_dyn`), lifted to scripting — a component registered with
//! `register_component_serde_json` is readable/writable from Lua WITHOUT a
//! per-type `Scriptable` binding. Writes follow the partial-merge semantics of
//! the editor's `edit_setComponent`: the Lua table carries ONLY the fields to
//! change ([`apex_core::json_merge`]).
//!
//! Reads are immediate (`&World` is valid for the whole script run); writes
//! are DEFERRED like every other script mutation (drained into the system's
//! `Commands` buffer, applied at the sync point).

use apex_core::component::{ComponentId, ComponentInfo};
use apex_core::world::World;
use apex_core::{json_merge, Entity, NoContext};

/// Resolve a component by name: the full `type_name`, or its unambiguous last
/// `::`-segment (`"UiText"` for `"apex_ui::text::UiText"`). Errors are strings
/// for direct Lua warn/propagation (§0.2a — never silently no-op).
pub(crate) fn resolve_component<'w>(
    world: &'w World,
    name: &str,
) -> Result<&'w ComponentInfo, String> {
    let registry = world.registry();
    if let Some(info) = registry.iter().find(|i| i.name == name) {
        return Ok(info);
    }
    let mut found: Option<&ComponentInfo> = None;
    for info in registry.iter() {
        if info.name.rsplit("::").next() == Some(name) {
            if let Some(prev) = found {
                return Err(format!(
                    "component name '{}' is ambiguous ('{}' vs '{}') — use the full type name",
                    name, prev.name, info.name
                ));
            }
            found = Some(info);
        }
    }
    found.ok_or_else(|| format!("component '{}' is not registered", name))
}

/// Read one component of one entity as a JSON tree via its serde fns.
/// `Ok(None)` — the entity lacks the component (not an error: scripts probe).
pub(crate) fn component_json(
    world: &World,
    entity: Entity,
    name: &str,
) -> Result<Option<serde_json::Value>, String> {
    let info = resolve_component(world, name)?;
    let serde_fns = info.serde.as_ref().ok_or_else(|| {
        format!(
            "component '{}' has no serde registration — register_component_serde_json it",
            info.name
        )
    })?;
    if !world.is_alive(entity) {
        return Ok(None);
    }
    let query = world
        .query_builder()
        .build()
        .map_err(|e| format!("point query failed: {e:?}"))?;
    let Some(item) = query.get(entity) else {
        return Ok(None);
    };
    let Some(ptr) = item.get_ptr(info.id) else {
        return Ok(None);
    };
    // SAFETY: `ptr` is the live column slot of `info.id` on this entity's row;
    // the serialize fn reads it as the registered `T`.
    let bytes = unsafe { (serde_fns.serialize_fn)(ptr, &mut NoContext) }
        .map_err(|e| format!("serialize of '{}' failed: {e:?}", info.name))?;
    let value = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "component '{}' is registered with the '{}' serde format, not JSON \
             (register_component_serde_json for script access): {e}",
            info.name, serde_fns.format
        )
    })?;
    Ok(Some(value))
}

/// Apply a PARTIAL JSON value to one component of one entity (the deferred
/// half of `set_component`): current tree ⊕ partial → whole write via
/// `insert_dyn`. When the component is absent the partial must be a complete
/// value (it becomes the insert). Failures warn loudly (§0.2a) — a script
/// write must never crash the app.
pub(crate) fn apply_component_json(
    world: &mut World,
    entity: Entity,
    name: &str,
    partial: &serde_json::Value,
) {
    let (component_id, merged) = match prepare_component_write(world, entity, name, partial) {
        Ok(prepared) => prepared,
        Err(e) => {
            log::warn!("set_component('{name}'): {e}");
            return;
        }
    };
    let tick = world.current_tick();
    world.insert_dyn(entity, component_id, merged, tick);
}

/// Resolve + merge + deserialize to raw column bytes (everything fallible
/// before the world mutation).
fn prepare_component_write(
    world: &World,
    entity: Entity,
    name: &str,
    partial: &serde_json::Value,
) -> Result<(ComponentId, Vec<u8>), String> {
    if !world.is_alive(entity) {
        return Err(format!("entity {entity:?} is not alive"));
    }
    let current = component_json(world, entity, name)?;
    let info = resolve_component(world, name)?;
    let serde_fns = info
        .serde
        .as_ref()
        .expect("checked by component_json above");
    let merged = match current {
        Some(mut base) => {
            json_merge(&mut base, partial);
            base
        }
        // Absent component: the partial must be the complete value.
        None => partial.clone(),
    };
    let bytes = serde_json::to_vec(&merged).map_err(|e| e.to_string())?;
    let raw = (serde_fns.deserialize_fn)(&bytes, &mut NoContext).map_err(|e| {
        format!(
            "deserialize into '{}' failed (partial write on an absent component \
             must carry ALL fields): {e:?}",
            info.name
        )
    })?;
    Ok((info.id, raw))
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
