//! Registration of the global Lua functions: `delta_time`, `entity_count`,
//! `query`, `commit`, `spawn_entity`, `despawn`, `read_resource`,
//! `write_resource`, `emit_event`, `get_component`, `set_component`, `log`.
//!
//! All functions obtain the context via `lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()`
//! and work with the world through it within the scope of a `ScriptEngine::run()` call.

use std::cell::RefCell;
use std::rc::Rc;

use crate::{
    context::{ScriptContext, ScriptSystemDecl, SpawnRequest},
    iterators,
};

/// Register all global API functions in Lua.
pub fn register_globals(lua: &mlua::Lua) -> mlua::Result<()> {
    register_delta_time(lua)?;
    register_entity_count(lua)?;
    register_query(lua)?;
    register_commit(lua)?;
    register_system_decl(lua)?;
    register_spawn(lua)?;
    register_despawn(lua)?;
    register_resource_api(lua)?;
    register_event_api(lua)?;
    register_component_serde_api(lua)?;
    register_log(lua)?;
    register_log_levels(lua)?;
    register_inspect(lua)?;

    lua.load("math = require('math'); string = require('string'); table = require('table')")
        .exec()?;

    Ok(())
}

// ── delta_time() ───────────────────────────────────────────────

fn register_delta_time(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("delta_time", lua.create_function(|lua, ()| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        let val = ctx.borrow().delta_time as f64;
        Ok(val)
    })?)
}

// ── entity_count() ─────────────────────────────────────────────

fn register_entity_count(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("entity_count", lua.create_function(|lua, ()| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        let val = ctx.borrow().entity_count() as i32;
        Ok(val)
    })?)
}

// ── query(descs) ───────────────────────────────────────────────

fn register_query(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("query", lua.create_function(|lua, descs: mlua::Table| {
        let parsed = iterators::parse_query_descs(&descs)?;

        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;

        // Snapshot the matching entities up front through a core `DynQuery`
        // (no private archetype scan / cache — the world is structurally frozen
        // for the run, so one snapshot is authoritative).
        let entities = {
            let ctx_ref = ctx.borrow();
            let world = ctx_ref.world_ref();
            iterators::collect_matching_entities(world, &ctx_ref, &parsed)
        };

        iterators::create_query_iter_fn(lua, entities, parsed)
    })?)
}

// ── commit(entity_table) ───────────────────────────────────────

fn register_commit(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("commit", lua.create_function(|lua, entity_table: mlua::Table| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        let ctx_ref = ctx.borrow();
        iterators::commit_entity_table(&ctx_ref, &entity_table)
    })?)
}

// ── system{ name, query, fn } ──────────────────────────────────

/// Declare a phase-B script system: `system{ name = "...", query = {...},
/// fn = function(it) ... end }`. Records the declaration on the `ScriptContext`;
/// the engine later translates `query` into a scheduler access declaration and
/// registers a NonSend runner that invokes `fn` each frame. Unlike the
/// monolithic `run()`, a declared system participates in conflict detection and
/// runs concurrently with disjoint Rust systems.
fn register_system_decl(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("system", lua.create_function(|lua, spec: mlua::Table| {
        let name: String = spec.get("name")
            .map_err(|_| mlua::Error::runtime("system{}: missing string field 'name'"))?;
        let query_tbl: mlua::Table = spec.get("query")
            .map_err(|_| mlua::Error::runtime("system{}: missing table field 'query'"))?;
        let descs = iterators::parse_query_descs(&query_tbl)?;
        let func: mlua::Function = spec.get("fn")
            .map_err(|_| mlua::Error::runtime("system{}: missing function field 'fn'"))?;
        let fn_key = lua.create_registry_value(func)?;

        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        ctx.borrow_mut().script_systems.push(ScriptSystemDecl { name, descs, fn_key });
        Ok(())
    })?)
}

// ── spawn_entity(components) ───────────────────────────────────

fn register_spawn(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("spawn_entity", lua.create_function(|lua, components: mlua::Table| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;

        let mut comps: Vec<(String, mlua::RegistryKey)> = Vec::new();
        for pair in components.clone().pairs::<String, mlua::Value>() {
            let (key, val): (String, mlua::Value) = pair?;
            let reg_key = lua.create_registry_value(val)?;
            comps.push((key, reg_key));
        }

        let request = SpawnRequest { components: comps };
        ctx.borrow_mut().queue_spawn(request);
        Ok(())
    })?)
}

// ── despawn(entity_id) ─────────────────────────────────────────

fn register_despawn(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("despawn", lua.create_function(|lua, entity_id: mlua::Value| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;

        // Entity ids are handed to Lua as `"index:generation"` strings so the full
        // 64-bit generational id survives (a bare index would drop the generation,
        // and packing both u32 into one f64 would lose precision above 2^53). We
        // reconstruct the FULL entity — including generation — before despawning.
        // A stale id (slot reused by a newer generation) yields a generation
        // mismatch, so `World::despawn` no-ops instead of killing the new tenant (E10).
        let entity = match iterators::parse_entity_id(&entity_id) {
            Some(e) => e,
            None => {
                log::warn!("despawn: invalid entity id {:?}", entity_id);
                return Ok(());
            }
        };
        ctx.borrow_mut().queue_despawn(entity);
        Ok(())
    })?)
}

// ── log(msg) ───────────────────────────────────────────────────

fn register_log(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("log", lua.create_function(|_, msg: String| {
        log::info!("[script] {}", msg);
        Ok(())
    })?)?;

    lua.globals().set("print", lua.create_function(|_, msg: String| {
        log::info!("[script] {}", msg);
        Ok(())
    })?)
}

// ── read_resource(name) / write_resource(name, value) ──────────

fn register_resource_api(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("read_resource", lua.create_function(|lua, type_name: String| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        let ctx_ref = ctx.borrow();
        match ctx_ref.read_resource(lua, &type_name) {
            Some(val) => Ok(val),
            None => {
                log::warn!("read_resource: resource '{}' not found", type_name);
                Ok(mlua::Value::Nil)
            }
        }
    })?)?;

    lua.globals().set("write_resource", lua.create_function(|lua, (type_name, value): (String, mlua::Value)| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        // No `Box::leak`: the deferred buffer owns the name as a `String` (E3).
        let result = ctx.borrow_mut().write_resource(lua, &type_name, value);
        result
    })?)
}

// ── get_component / set_component (serde path, RT-2) ──────────

/// `get_component(entity_id, name)` — read one component as a Lua table via
/// the serde-tree path (the editor inspector's mechanism); `set_component(
/// entity_id, name, partial_table)` — deferred PARTIAL write (only the fields
/// to change, deep-merged; the editor `edit_setComponent` semantics). `name`
/// is the full type name or its unambiguous last segment. Requires the
/// component to be registered with `register_component_serde_json`; no
/// per-type `Scriptable` binding is needed.
fn register_component_serde_api(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("get_component", lua.create_function(
        |lua, (entity_id, name): (mlua::Value, String)| {
            let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
                .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
            let Some(entity) = iterators::parse_entity_id(&entity_id) else {
                log::warn!("get_component: invalid entity id {:?}", entity_id);
                return Ok(mlua::Value::Nil);
            };
            let ctx_ref = ctx.borrow();
            let world = ctx_ref.world_ref();
            // Phase-B declared access applies to the serde path too — an
            // undeclared read is scheduler-blind (§0.2a: refuse loudly).
            if let Ok(info) = world.registry().find_by_name(&name) {
                if !ctx_ref.declares_read(info.id) {
                    log::warn!("get_component: '{}' is not in the system's declared access", name);
                    return Ok(mlua::Value::Nil);
                }
            }
            match crate::reflect::component_json(world, entity, &name) {
                Ok(Some(value)) => {
                    use mlua::LuaSerdeExt;
                    Ok(lua.to_value(&value)?)
                }
                Ok(None) => Ok(mlua::Value::Nil),
                Err(e) => {
                    log::warn!("get_component('{}'): {}", name, e);
                    Ok(mlua::Value::Nil)
                }
            }
        },
    )?)?;

    lua.globals().set("set_component", lua.create_function(
        |lua, (entity_id, name, value): (mlua::Value, String, mlua::Value)| {
            let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
                .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
            let Some(entity) = iterators::parse_entity_id(&entity_id) else {
                log::warn!("set_component: invalid entity id {:?}", entity_id);
                return Ok(());
            };
            let partial: serde_json::Value = {
                use mlua::LuaSerdeExt;
                lua.from_value(value)?
            };
            let mut ctx_mut = ctx.borrow_mut();
            {
                let world = ctx_mut.world_ref();
                if let Ok(info) = world.registry().find_by_name(&name) {
                    if !ctx_mut.declares_write(info.id) {
                        log::warn!(
                            "set_component: '{}' is not in the system's declared WRITE access",
                            name
                        );
                        return Ok(());
                    }
                }
            }
            ctx_mut.queue_component_serde_write(entity, name, partial);
            Ok(())
        },
    )?)
}

// ── emit_event(name, value) ────────────────────────────────────

fn register_event_api(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("emit_event", lua.create_function(|lua, (type_name, value): (String, mlua::Value)| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        // No `Box::leak`: the deferred buffer owns the name as a `String` (E3).
        let result = ctx.borrow_mut().emit_event(lua, &type_name, value);
        result
    })?)
}

// ── log_debug / log_warn / log_error ──────────────────────────

fn register_log_levels(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("log_debug", lua.create_function(|_, msg: String| {
        log::debug!("[script] {}", msg);
        Ok(())
    })?)?;
    lua.globals().set("log_warn", lua.create_function(|_, msg: String| {
        log::warn!("[script] {}", msg);
        Ok(())
    })?)?;
    lua.globals().set("log_error", lua.create_function(|_, msg: String| {
        log::error!("[script] {}", msg);
        Ok(())
    })?)
}

// ── inspect(table) ────────────────────────────────────────────

fn register_inspect(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("inspect", lua.create_function(|_, val: mlua::Value| {
        Ok(inspect_value(&val, 0))
    })?)
}

fn inspect_value(val: &mlua::Value, depth: usize) -> String {
    if depth > 4 {
        return "{...}".to_string();
    }
    match val {
        mlua::Value::Nil => "nil".to_string(),
        mlua::Value::Boolean(b) => b.to_string(),
        mlua::Value::Integer(i) => i.to_string(),
        mlua::Value::Number(n) => format!("{:.4}", n).trim_end_matches('0')
            .trim_end_matches('.').to_string(),
        mlua::Value::String(s) => {
            let s = s.to_string_lossy();
            format!("\"{}\"", s)
        }
        mlua::Value::Table(t) => {
            let mut parts = Vec::new();
            let indent = "  ".repeat(depth + 1);

            // Array part (indices 1..N)
            let len = t.raw_len();
            for i in 1..=len {
                if let Ok(v) = t.get::<mlua::Value>(i) {
                    parts.push(inspect_value(&v, depth + 1));
                }
            }
            // Hash part (string keys)
            for (k, v) in t.clone().pairs::<String, mlua::Value>().flatten() {
                if k == "_meta" { continue; }
                let val_str = inspect_value(&v, depth + 1);
                parts.push(format!("{} = {}", k, val_str));
            }

            if parts.is_empty() {
                "{}".to_string()
            } else if parts.len() <= 4 && parts.iter().all(|p| !p.contains('=')) {
                // Compact array
                format!("{{ {} }}", parts.join(", "))
            } else {
                let inner = parts.iter()
                    .map(|p| format!("{indent}{p}"))
                    .collect::<Vec<_>>()
                    .join(",\n");
                format!("{{\n{}\n{}}}", inner, "  ".repeat(depth))
            }
        }
        _ => "<userdata>".to_string(),
    }
}
