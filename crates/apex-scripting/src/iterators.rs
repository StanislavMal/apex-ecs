//! Lua-совместимые итераторы для `query()`.
//!
//! # Дизайн query-итератора
//!
//! Каждый элемент итератора — Lua таблица со структурой:
//! ```text
//! {
//!     entity = 42,
//!     position = { x = 1.0, y = 2.0 },   -- Read/Write компоненты (lowercase ключи)
//!     velocity = { x = 0.5, y = 0.0 },
//!     _meta = { arch = 0, row = 3, writes = { position = 1, velocity = 2 } }
//! }
//! ```
//!
//! `commit(entity)` читает `_meta` чтобы найти колонки архетипа
//! и записывает изменённые значения обратно в ECS.
//!
//! # Формат дескрипторов
//!
//! `query({"Read:Position", "Write:Velocity"})` — таблица строк.
//! Парсится в `QueryDesc` в `parse_query_descs()`.

use std::cell::RefCell;
use std::rc::Rc;

use apex_core::{
    component::ComponentId,
    entity::Entity,
    world::World,
};

use crate::context::ScriptContext;

// ── QueryDesc ──────────────────────────────────────────────────

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct QueryDesc {
    pub type_name: String,
    pub write:     bool,
}

pub fn parse_query_descs(table: &mlua::Table) -> mlua::Result<Vec<QueryDesc>> {
    let mut descs = Vec::new();
    let len = table.raw_len();
    for i in 1..=len {
        let val: String = table.get(i)?;
        descs.push(parse_one_desc(&val));
    }
    Ok(descs)
}

fn parse_one_desc(s: &str) -> QueryDesc {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("Write:").or_else(|| s.strip_prefix("write:")) {
        return QueryDesc { type_name: rest.trim().to_string(), write: true };
    }
    if let Some(rest) = s.strip_prefix("Read:").or_else(|| s.strip_prefix("read:")) {
        return QueryDesc { type_name: rest.trim().to_string(), write: false };
    }
    QueryDesc { type_name: s.to_string(), write: false }
}

// ── ArchState ──────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ArchState {
    pub arch_idx: usize,
    pub len: usize,
    pub components: Vec<ComponentState>,
}

#[derive(Clone)]
pub(crate) struct ComponentState {
    pub col_idx:   usize,
    pub type_name: String,
    pub write:     bool,
    #[allow(dead_code)]
    pub comp_id:   ComponentId,
}

// ── QueryIteratorState ─────────────────────────────────────────

struct IterState {
    arch_states: Vec<ArchState>,
    arch_cursor: usize,
    row_cursor:  usize,
}

// ── Построение arch states ─────────────────────────────────────

pub(crate) fn build_arch_states(
    world: &World,
    ctx: &ScriptContext,
    descs: &[QueryDesc],
) -> Vec<ArchState> {
    let resolved: Vec<Option<(ComponentId, &QueryDesc)>> = descs.iter()
        .map(|d| ctx.binding(&d.type_name).map(|b| (b.id, d)))
        .collect();

    if resolved.iter().any(|r| r.is_none()) {
        return Vec::new();
    }
    let resolved: Vec<(ComponentId, &QueryDesc)> = resolved.into_iter()
        .map(|r| r.unwrap())
        .collect();

    world.archetypes()
        .iter()
        .enumerate()
        .filter_map(|(arch_idx, arch)| {
            if arch.is_empty() { return None; }

            let components: Vec<ComponentState> = resolved.iter()
                .filter_map(|(cid, desc)| {
                    let col_idx = arch.column_index(*cid)?;
                    Some(ComponentState {
                        col_idx,
                        type_name: desc.type_name.clone(),
                        write:     desc.write,
                        comp_id:   *cid,
                    })
                })
                .collect();

            if components.len() != resolved.len() {
                return None;
            }

            Some(ArchState {
                arch_idx,
                len: arch.len(),
                components,
            })
        })
        .collect()
}

// ── Построение entity таблицы ──────────────────────────────────

pub(crate) fn build_entity_table(
    lua: &mlua::Lua,
    world: &World,
    ctx: &ScriptContext,
    arch_idx: usize,
    row: usize,
    components: &[ComponentState],
) -> mlua::Result<mlua::Table> {
    let arch = &world.archetypes()[arch_idx];
    let entity: Entity = arch.entities()[row];

    let t = lua.create_table()?;
    t.set("entity", entity.index() as i32)?;

    let meta = lua.create_table()?;
    meta.set("arch", arch_idx as i32)?;
    meta.set("row", row as i32)?;

    let writes = lua.create_table()?;
    for comp in components {
        if comp.write {
            writes.set(comp.type_name.as_str(), comp.col_idx as i32)?;
        }
    }
    meta.set("writes", writes)?;
    t.set("_meta", meta)?;

    for comp in components {
        let binding = match ctx.binding(&comp.type_name) {
            Some(b) => b,
            None => continue,
        };

        let val = unsafe {
            let col = &arch.columns_raw()[comp.col_idx];
            let ptr = col.get_raw_ptr(row);
            (binding.read)(ptr, lua)?
        };

        let key = comp.type_name.to_lowercase();
        t.set(key, val)?;
    }

    Ok(t)
}

// ── commit(entity_table) ───────────────────────────────────────

pub(crate) fn commit_entity_table(
    _lua: &mlua::Lua,
    world: &World,
    ctx: &ScriptContext,
    entity_table: &mlua::Table,
) -> mlua::Result<()> {
    let meta: mlua::Table = match entity_table.get("_meta") {
        Ok(m) => m,
        Err(_) => {
            log::warn!("commit: no _meta in entity table");
            return Ok(());
        }
    };

    let arch_idx: i32 = meta.get("arch")?;
    let row: i32 = meta.get("row")?;
    let writes: mlua::Table = meta.get("writes")?;

    let world_ptr = world as *const World;

    for pair in writes.clone().pairs::<String, i32>() {
        let (type_name, col_idx): (String, i32) = pair?;
        log::debug!("commit: writing component '{type_name}' col={col_idx}");

        let binding = match ctx.binding(&type_name) {
            Some(b) => b,
            None => {
                log::warn!("commit: no binding for '{}'", type_name);
                continue;
            }
        };

        let key = type_name.to_lowercase();
        let val: mlua::Value = match entity_table.get::<mlua::Value>(key.as_str()) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("commit: cannot get '{key}' from entity table: {e}");
                continue;
            }
        };

        unsafe {
            let w = world_ptr.as_ref().unwrap_unchecked();
            let arch = &w.archetypes()[arch_idx as usize];
            let col = &arch.columns_raw()[col_idx as usize];
            let ptr = col.get_raw_ptr(row as usize) as *mut u8;
            (binding.write)(ptr, &val);
            arch.set_change_tick(row as usize, binding.id, w.current_tick());
        }
    }

    Ok(())
}

// ── Создание Lua iterator factory ──────────────────────────────

pub(crate) fn create_query_iter_fn(
    lua: &mlua::Lua,
    arch_states: Vec<ArchState>,
) -> mlua::Result<mlua::Function> {
    let state = Rc::new(RefCell::new(IterState {
        arch_states,
        arch_cursor: 0,
        row_cursor:  0,
    }));

    lua.create_function(move |lua, ()| {
        let mut st = state.borrow_mut();

        loop {
            let arch_state = match st.arch_states.get(st.arch_cursor) {
                Some(s) => s.clone(),
                None => return Ok(mlua::Value::Nil),
            };

            if st.row_cursor >= arch_state.len {
                st.arch_cursor += 1;
                st.row_cursor  = 0;
                continue;
            }

            let arch_idx   = arch_state.arch_idx;
            let row        = st.row_cursor;
            let components = arch_state.components;
            st.row_cursor += 1;

            let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
                .ok_or_else(|| mlua::Error::runtime("no ScriptContext in Lua app data"))?;
            let ctx_ref = ctx.borrow();
            let world = ctx_ref.world_ref();

            let table = build_entity_table(
                lua,
                world,
                &ctx_ref,
                arch_idx,
                row,
                &components,
            )?;

            return Ok(mlua::Value::Table(table));
        }
    })
}
