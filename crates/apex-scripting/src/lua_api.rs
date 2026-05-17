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
            .map(|d| format!("{}:{}", if d.write { "w" } else { "r" }, d.type_name))
            .collect();

        let arch_states = {
            let ctx_ref = ctx.borrow();
            if let Some(cached) = ctx_ref.query_cache.get(&cache_key) {
                cached.clone()
            } else {
                let world = ctx_ref.world_ref();
                let states = iterators::build_arch_states(world, &ctx_ref, &parsed);
                states
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

// ── despawn(entity_index) ──────────────────────────────────────

fn register_despawn(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("despawn", lua.create_function(|lua, entity_idx: i32| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        let entity = {
            let ctx_ref = ctx.borrow();
            let world = ctx_ref.world_ref();
            world.entity_allocator().get_by_index(entity_idx as u32)
        };
        if let Some(entity) = entity {
            ctx.borrow_mut().queue_despawn(entity);
        } else {
            log::warn!("despawn: entity index {} не найден или уже мёртв", entity_idx);
        }
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
        let static_name: &'static str = Box::leak(type_name.into_boxed_str());
        let result = ctx.borrow_mut().write_resource(lua, static_name, value);
        result
    })?)
}

// ── emit_event(name, value) ────────────────────────────────────

fn register_event_api(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.globals().set("emit_event", lua.create_function(|lua, (type_name, value): (String, mlua::Value)| {
        let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
            .ok_or_else(|| mlua::Error::runtime("no ScriptContext"))?;
        let static_name: &'static str = Box::leak(type_name.into_boxed_str());
        let result = ctx.borrow_mut().emit_event(lua, static_name, value);
        result
    })?)
}
