//! Регистрация глобальных Lua-функций: `delta_time`, `entity_count`,
//! `query`, `commit`, `spawn_entity`, `despawn`, `read_resource`,
//! `write_resource`, `emit_event`, `log`.
//!
//! Все функции получают контекст через `lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()`
//! и работают с миром через него в пределах вызова `ScriptEngine::run()`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::{
    context::{ScriptContext, SpawnRequest},
    iterators,
};

/// Зарегистрировать все глобальные API-функции в Lua.
pub fn register_globals(lua: &mlua::Lua) -> mlua::Result<()> {
    register_delta_time(lua)?;
    register_entity_count(lua)?;
    register_query(lua)?;
    register_commit(lua)?;
    register_spawn(lua)?;
    register_despawn(lua)?;
    register_resource_api(lua)?;
    register_event_api(lua)?;
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

        let cache_key: Vec<String> = parsed.iter()
            .map(|d| format!("{:?}:{}", d.mode, d.type_name))
            .collect();

        let arch_states = {
            let ctx_ref = ctx.borrow();
            if let Some(cached) = ctx_ref.query_cache.get(&cache_key) {
                cached.clone()
            } else {
                let world = ctx_ref.world_ref();
                
                iterators::build_arch_states(world, &ctx_ref, &parsed)
            }
        };

        if !arch_states.is_empty() {
            let need_insert = !ctx.borrow().query_cache.contains_key(&cache_key);
            if need_insert {
                ctx.borrow_mut().query_cache.insert(cache_key, arch_states.clone());
            }
        }

        iterators::create_query_iter_fn(lua, arch_states)
    })?)
}

// ── commit(entity_table) ───────────────────────────────────────

fn register_commit(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("commit", lua.create_function(|lua, entity_table: mlua::Table| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        let ctx_ref = ctx.borrow();
        let world = ctx_ref.world_ref();
        iterators::commit_entity_table(lua, world, &ctx_ref, &entity_table)
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
                log::warn!("read_resource: ресурс '{}' не найден", type_name);
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

            // Array part (индексы 1..N)
            let len = t.raw_len();
            for i in 1..=len {
                if let Ok(v) = t.get::<mlua::Value>(i) {
                    parts.push(inspect_value(&v, depth + 1));
                }
            }
            // Hash part (строковые ключи)
            for (k, v) in t.clone().pairs::<String, mlua::Value>().flatten() {
                if k == "_meta" { continue; }
                let val_str = inspect_value(&v, depth + 1);
                parts.push(format!("{} = {}", k, val_str));
            }

            if parts.is_empty() {
                "{}".to_string()
            } else if parts.len() <= 4 && parts.iter().all(|p| !p.contains('=')) {
                // Компактный массив
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
