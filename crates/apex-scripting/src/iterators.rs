//! Lua-совместимые итераторы для `query()`.
//!
//! # Дизайн query-итератора
//!
//! Каждый элемент итератора — Lua таблица со структурой:
//! ```text
//! {
//!     entity = "42:0",                   -- id "index:generation" (E10), не голый индекс
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

// ── Entity id encoding (E10) ───────────────────────────────────
//
// An `Entity` is a generational index: 2×u32 = 64 bits. Lua numbers are f64,
// exact only up to 2^53, so a packed 64-bit id could silently lose its high
// bits. To keep the FULL generation across frames we hand entity ids to Lua as
// `"index:generation"` strings and decode them back into a real `Entity`.
//
// This matters because slots are reused: an index saved from a past frame can,
// after that entity died and a new one took its slot, refer to a DIFFERENT
// generation. Carrying the generation lets `World::despawn`/commit reject the
// stale id (generation mismatch → no-op) instead of hitting the new tenant.

/// Encode an entity as the `"index:generation"` string handed to Lua.
pub(crate) fn encode_entity_id(entity: Entity) -> String {
    format!("{}:{}", entity.index(), entity.generation())
}

/// Decode an entity id previously produced by [`encode_entity_id`].
///
/// Accepts a Lua string `"index:generation"`. For backward tolerance a bare
/// integer/string index (no `:generation`) is rejected as invalid rather than
/// silently reconstructed with a guessed generation — a caller must round-trip
/// the exact id it was given.
pub(crate) fn parse_entity_id(value: &mlua::Value) -> Option<Entity> {
    let s = match value {
        mlua::Value::String(s) => s.to_str().ok()?.to_owned(),
        _ => return None,
    };
    parse_entity_id_str(&s)
}

/// Decode an `"index:generation"` string into an [`Entity`].
pub(crate) fn parse_entity_id_str(s: &str) -> Option<Entity> {
    let (idx, gen) = s.split_once(':')?;
    let index: u32 = idx.trim().parse().ok()?;
    let generation: u32 = gen.trim().parse().ok()?;
    Some(Entity::from_raw_parts(index, generation))
}

// ── QueryDesc ──────────────────────────────────────────────────

/// Режим доступа к компоненту в query-дескрипторе.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum QueryMode {
    Read,
    Write,
    With,
    Without,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct QueryDesc {
    pub type_name: String,
    pub mode:      QueryMode,
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
        return QueryDesc { type_name: rest.trim().to_string(), mode: QueryMode::Write };
    }
    if let Some(rest) = s.strip_prefix("Read:").or_else(|| s.strip_prefix("read:")) {
        return QueryDesc { type_name: rest.trim().to_string(), mode: QueryMode::Read };
    }
    if let Some(rest) = s.strip_prefix("With:").or_else(|| s.strip_prefix("with:")) {
        return QueryDesc { type_name: rest.trim().to_string(), mode: QueryMode::With };
    }
    if let Some(rest) = s.strip_prefix("Without:").or_else(|| s.strip_prefix("without:")) {
        return QueryDesc { type_name: rest.trim().to_string(), mode: QueryMode::Without };
    }
    QueryDesc { type_name: s.to_string(), mode: QueryMode::Read }
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
    pub mode:      QueryMode,
    #[allow(dead_code)]
    pub comp_id:   ComponentId,
}

// ── QueryIteratorState ─────────────────────────────────────────

struct IterState {
    arch_states: Vec<ArchState>,
    arch_cursor: usize,
    row_cursor:  usize,
    /// (arch_idx, row, entity_table) — для авто-commit на следующей итерации
    pending:     Option<(usize, usize, mlua::Table)>,
}

// ── Построение arch states ─────────────────────────────────────

pub(crate) fn build_arch_states(
    world: &World,
    ctx: &ScriptContext,
    descs: &[QueryDesc],
) -> Vec<ArchState> {
    // Разделяем на data-компоненты (Read/Write) и фильтры (With/Without)
    let data_descs: Vec<&QueryDesc> = descs.iter()
        .filter(|d| matches!(d.mode, QueryMode::Read | QueryMode::Write))
        .collect();
    let with_descs: Vec<&QueryDesc> = descs.iter()
        .filter(|d| matches!(d.mode, QueryMode::With))
        .collect();
    let without_descs: Vec<&QueryDesc> = descs.iter()
        .filter(|d| matches!(d.mode, QueryMode::Without))
        .collect();

    // Разрешаем имена data-компонентов → ComponentId
    let resolved_data: Vec<(ComponentId, &QueryDesc)> = data_descs.iter()
        .filter_map(|d| ctx.binding(&d.type_name).map(|b| (b.id, *d)))
        .collect();
    if resolved_data.len() != data_descs.len() {
        // §0.2a (E8): an unregistered data component makes the whole query
        // silently yield nothing — the script sees an empty result and no
        // reason why. Name the first missing type (throttled).
        if let Some(d) = data_descs.iter().find(|d| ctx.binding(&d.type_name).is_none()) {
            apex_core::warn_once!(
                "script query: data component '{}' is not registered — query yields no results",
                d.type_name,
            );
        }
        return Vec::new();
    }

    // Разрешаем With/Without в ComponentId
    let with_ids: Vec<ComponentId> = with_descs.iter()
        .filter_map(|d| ctx.binding(&d.type_name).map(|b| b.id))
        .collect();
    if with_ids.len() != with_descs.len() {
        // §0.2a (E8): an unregistered With filter also empties the query.
        if let Some(d) = with_descs.iter().find(|d| ctx.binding(&d.type_name).is_none()) {
            apex_core::warn_once!(
                "script query: With filter component '{}' is not registered — query yields no results",
                d.type_name,
            );
        }
        return Vec::new();
    }
    let without_ids: Vec<ComponentId> = without_descs.iter()
        .filter_map(|d| ctx.binding(&d.type_name).map(|b| b.id))
        .collect();
    // §0.2a (E8): an unregistered Without component silently disables that
    // filter (the query then over-matches). Surface it (throttled).
    if without_ids.len() != without_descs.len() {
        if let Some(d) = without_descs.iter().find(|d| ctx.binding(&d.type_name).is_none()) {
            apex_core::warn_once!(
                "script query: Without filter component '{}' is not registered — filter ignored, query may over-match",
                d.type_name,
            );
        }
    }

    world.archetypes()
        .iter()
        .enumerate()
        .filter_map(|(arch_idx, arch)| {
            if arch.is_empty() { return None; }

            // Data-компоненты: все должны присутствовать
            let components: Vec<ComponentState> = resolved_data.iter()
                .filter_map(|(cid, desc)| {
                    let col_idx = arch.column_index(*cid)?;
                    Some(ComponentState {
                        col_idx,
                        type_name: desc.type_name.clone(),
                        mode:      desc.mode,
                        comp_id:   *cid,
                    })
                })
                .collect();
            if components.len() != resolved_data.len() {
                return None;
            }

            // With-фильтры: все должны присутствовать
            for cid in &with_ids {
                arch.column_index(*cid)?;
            }

            // Without-фильтры: ни один не должен присутствовать
            for cid in &without_ids {
                if arch.column_index(*cid).is_some() {
                    return None;
                }
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

    let entity_id = encode_entity_id(entity);

    let t = lua.create_table()?;
    // Full "index:generation" id (E10), not a bare index — see encode_entity_id.
    t.set("entity", entity_id.as_str())?;

    let meta = lua.create_table()?;
    meta.set("arch", arch_idx as i32)?;
    meta.set("row", row as i32)?;
    // Mirror the id into `_meta` so commit can validate that the row still holds
    // THIS entity (full generation), rejecting stale/forged tables (E1 + E10).
    meta.set("entity", entity_id.as_str())?;

    let writes = lua.create_table()?;
    for comp in components {
        if matches!(comp.mode, QueryMode::Write) {
            writes.set(comp.type_name.as_str(), comp.col_idx as i32)?;
        }
    }
    meta.set("writes", writes)?;
    t.set("_meta", meta)?;

    for comp in components {
        let binding = match ctx.binding(&comp.type_name) {
            Some(b) => b,
            // §0.2a (E8): the binding was present when the archetype state was
            // built but is gone now — the component silently vanishes from the
            // table the script sees. Should not happen; surface it (throttled).
            None => {
                apex_core::warn_once!(
                    "script query: binding for '{}' disappeared mid-iteration — component omitted from entity table",
                    comp.type_name,
                );
                continue;
            }
        };

        let val = unsafe {
            let col = &arch.columns_raw()[comp.col_idx];
            let ptr = col.get_raw_ptr(row);
            (binding.read)(ptr, lua)?
        };

        let key = comp.type_name.to_lowercase();
        t.set(key.clone(), val)?;

        // Для Read-компонентов: ставим __newindex для предупреждения о модификации
        if matches!(comp.mode, QueryMode::Read) {
            if let Ok(sub_table) = t.get::<mlua::Table>(key.as_str()) {
                let mt = lua.create_table()?;
                let type_name = comp.type_name.clone();
                mt.set("__newindex", lua.create_function(move |_, (_t, field, _val): (mlua::Table, String, mlua::Value)| {
                    log::warn!(
                        "[script] попытка изменить Read-компонент '{}' (поле '{}') — используй Write:{}",
                        type_name, field, type_name
                    );
                    Ok(())
                })?)?;
                sub_table.set_metatable(Some(mt));
            }
        }
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
            log::warn!("commit: таблица не из query (нет _meta), пропущена");
            return Ok(());
        }
    };

    let arch_idx: i32 = meta.get("arch")?;
    let row: i32 = meta.get("row")?;
    let meta_entity_raw: mlua::Value = meta.get("entity")?;
    let writes: mlua::Table = meta.get("writes")?;

    // Decode the FULL entity id (index + generation) stored in `_meta` (E10).
    // A missing/garbage id can't be trusted → reject cleanly.
    let meta_entity = match parse_entity_id(&meta_entity_raw) {
        Some(e) => e,
        None => {
            log::warn!("commit: _meta.entity is not a valid entity id, skipped");
            return Ok(());
        }
    };

    // Никогда НЕ доверяем `_meta` из Lua: скрипт может подделать arch/row/col
    // или сохранить таблицу до следующего кадра, где строку уже swap-removed'нули.
    // Валидируем всё против ЖИВОГО мира до любого unsafe-доступа (E1).
    if arch_idx < 0 || row < 0 {
        log::warn!("commit: negative _meta arch/row, skipped");
        return Ok(());
    }
    let arch_idx = arch_idx as usize;
    let row = row as usize;
    let arch = match world.archetypes().get(arch_idx) {
        Some(a) => a,
        None => {
            log::warn!("commit: _meta.arch {arch_idx} out of range, skipped");
            return Ok(());
        }
    };
    if row >= arch.len() {
        log::warn!("commit: _meta.row {row} out of range for archetype {arch_idx}, skipped");
        return Ok(());
    }
    // Строка обязана всё ещё нести ту entity, для которой построена таблица —
    // иначе отложенное структурное изменение переселило её, и мы бы записали
    // в ЧУЖУЮ entity (порча данных). Сравниваем ПОЛНУЮ entity (index +
    // generation): переиспользованный слот с новым поколением ловится как
    // несовпадение и отвергается (E1 + E10).
    if arch.entities()[row] != meta_entity {
        log::warn!(
            "commit: entity at archetype {arch_idx} row {row} no longer matches table entity, skipped"
        );
        return Ok(());
    }

    for pair in writes.clone().pairs::<String, i32>() {
        let (type_name, _lua_col): (String, i32) = pair?;

        let binding = match ctx.binding(&type_name) {
            Some(b) => b,
            None => {
                log::warn!("commit: no binding for '{}'", type_name);
                continue;
            }
        };

        // РЕ-резолвим колонку из архетипа по ComponentId биндинга — Lua-значению
        // col_idx не верим (подделка = OOB или type confusion в write).
        let col_idx = match arch.column_index(binding.id) {
            Some(i) => i,
            None => {
                log::warn!("commit: component '{type_name}' not present in archetype, skipped");
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

        // SAFETY: arch_idx/row/col_idx проверены выше против живого мира, row в
        // границах архетипа, а `binding.write` соответствует `binding.id` = типу
        // колонки (col_idx резолвнут именно по нему) — type confusion исключена.
        unsafe {
            let col = &arch.columns_raw()[col_idx];
            let ptr = col.get_raw_ptr(row) as *mut u8;
            (binding.write)(ptr, &val);
            arch.set_change_tick(row, binding.id, world.current_tick());
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
        pending:     None,
    }));

    lua.create_function(move |lua, ()| {
        let mut st = state.borrow_mut();

        // Авто-commit предыдущей entity (если включён)
        if let Some((_arch_idx, _row, ref table)) = st.pending.take() {
            let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
                .ok_or_else(|| mlua::Error::runtime("no ScriptContext in Lua app data"))?;
            let ctx_ref = ctx.borrow();
            if ctx_ref.auto_commit {
                let world = ctx_ref.world_ref();
                commit_entity_table(lua, world, &ctx_ref, table)?;
            }
        }

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

            // Сохраняем для авто-commit на следующем вызове
            st.pending = Some((arch_idx, row, table.clone()));

            return Ok(mlua::Value::Table(table));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::component::Component;

    struct Marker;
    impl Component for Marker {}

    /// Build a forged `_meta` table. `entity` is the `"index:generation"` string
    /// exactly as the real query path stores it (E10).
    fn forged_meta(lua: &mlua::Lua, arch: i32, row: i32, entity: &str) -> mlua::Table {
        let t = lua.create_table().unwrap();
        let meta = lua.create_table().unwrap();
        meta.set("arch", arch).unwrap();
        meta.set("row", row).unwrap();
        meta.set("entity", entity).unwrap();
        meta.set("writes", lua.create_table().unwrap()).unwrap();
        t.set("_meta", meta).unwrap();
        t
    }

    /// E1: a forged `_meta` (out-of-range archetype/row, or a row that no longer
    /// holds the table's entity) must be rejected cleanly — never dereferenced
    /// into an OOB / wrong-entity write.
    #[test]
    fn commit_rejects_forged_meta() {
        let lua = mlua::Lua::new();
        let ctx = ScriptContext::new();
        let mut world = World::new();
        let e = world.spawn((Marker,)); // archetype 0, one row
        let real_id = encode_entity_id(e);

        // Out-of-range archetype.
        let t = forged_meta(&lua, 9999, 0, &real_id);
        commit_entity_table(&lua, &world, &ctx, &t).expect("skips cleanly");

        // Valid archetype, out-of-range row.
        let t = forged_meta(&lua, 0, 9999, &real_id);
        commit_entity_table(&lua, &world, &ctx, &t).expect("skips cleanly");

        // Valid archetype/row, but the table claims a different entity than the
        // one currently occupying that row.
        let t = forged_meta(&lua, 0, 0, "424242:0");
        commit_entity_table(&lua, &world, &ctx, &t).expect("skips cleanly");

        // E10: valid archetype/row and the CORRECT index, but a STALE generation
        // (as if the slot were reused). Full-entity comparison must reject it.
        let stale_gen = format!("{}:{}", e.index(), e.generation().wrapping_add(1));
        let t = forged_meta(&lua, 0, 0, &stale_gen);
        commit_entity_table(&lua, &world, &ctx, &t).expect("skips cleanly");
    }

    /// E10: entity ids round-trip through the `"index:generation"` string form,
    /// preserving the full generation (not just the low index bits).
    #[test]
    fn entity_id_roundtrip_preserves_generation() {
        let e = Entity::from_raw_parts(7, 42);
        let s = encode_entity_id(e);
        assert_eq!(s, "7:42");
        assert_eq!(parse_entity_id_str(&s), Some(e));

        // High generation bits survive (would be lost by a bare index).
        let e2 = Entity::from_raw_parts(1, u32::MAX);
        assert_eq!(parse_entity_id_str(&encode_entity_id(e2)), Some(e2));

        // Garbage / bare index is rejected, not silently accepted.
        assert_eq!(parse_entity_id_str("5"), None);
        assert_eq!(parse_entity_id_str("abc:def"), None);
    }
}
