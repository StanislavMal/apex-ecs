//! Lua-compatible iterators for `query()`.
//!
//! # Query-iterator design
//!
//! `query()` snapshots the MATCHING entities up front (a `Vec<Entity>` from a
//! core [`DynQuery`](apex_core::query::DynQuery)); the Lua iterator walks that
//! snapshot. Each element is a Lua table with the structure:
//! ```text
//! {
//!     entity = "42:0",                   -- id "index:generation" (E10), not a bare index
//!     position = { x = 1.0, y = 2.0 },   -- Read/Write components (lowercase keys)
//!     velocity = { x = 0.5, y = 0.0 },
//!     _meta = { entity = "42:0", writes = { position = 1 } }
//! }
//! ```
//!
//! Component access — reads AND the `commit` write-back — goes through the
//! core dynamic-query surface ([`DynItem::get_ptr`](apex_core::query::DynItem)
//! / [`DynItemMut::get_mut_ptr`](apex_core::query::DynItemMut)), so scripting
//! shares the one archetype-access path the ECS validates (no private unsafe
//! column walk). `commit(entity)` reads `_meta.entity` to re-resolve the row
//! against the LIVE world and writes the declared components back.
//!
//! # Descriptor format
//!
//! `query({"Read:Position", "Write:Velocity"})` — a table of strings.
//! Parsed into `QueryDesc` by `parse_query_descs()`. Besides `Read:`/`Write:`/
//! `With:`/`Without:`, the reactive filters `Changed:X` / `Added:X` restrict the
//! snapshot to rows whose `X` changed / was added since the previous run (the
//! value is exposed only if `X` is also requested via `Read:`/`Write:`).

use std::cell::RefCell;
use std::rc::Rc;

use apex_core::{
    component::ComponentId,
    entity::Entity,
    world::World,
    Severity,
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

/// Component access mode in a query descriptor.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum QueryMode {
    Read,
    Write,
    With,
    Without,
    /// Reactive filter: the component changed since the previous run.
    Changed,
    /// Reactive filter: the component was added since the previous run.
    Added,
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
    // Prefixes are case-insensitive on the first letter (`Write:`/`write:`).
    let table: &[(&str, &str, QueryMode)] = &[
        ("Write:",   "write:",   QueryMode::Write),
        ("Read:",    "read:",    QueryMode::Read),
        ("With:",    "with:",    QueryMode::With),
        ("Without:", "without:", QueryMode::Without),
        ("Changed:", "changed:", QueryMode::Changed),
        ("Added:",   "added:",   QueryMode::Added),
    ];
    for (upper, lower, mode) in table {
        if let Some(rest) = s.strip_prefix(*upper).or_else(|| s.strip_prefix(*lower)) {
            return QueryDesc { type_name: rest.trim().to_string(), mode: *mode };
        }
    }
    QueryDesc { type_name: s.to_string(), mode: QueryMode::Read }
}

// ── Snapshot of matching entities ──────────────────────────────

/// Read function pointer for a scriptable component (`ptr → Lua value`).
type ReadFn = unsafe fn(*const u8, &mlua::Lua) -> mlua::Result<mlua::Value>;
/// Write function pointer for a scriptable component (`Lua value → ptr`).
type WriteFn = unsafe fn(*mut u8, &mlua::Value) -> bool;

/// Resolve the descriptors against the LIVE world and return the matching
/// entities as an up-front snapshot.
///
/// The snapshot is taken through a core read [`DynQuery`], so scripting shares
/// the archetype-matching + reactive-filter (`Changed`/`Added`, S8) logic the
/// ECS validates instead of scanning archetypes itself. Structural changes are
/// deferred until the script finishes, so the snapshot stays valid for the whole
/// iteration.
///
/// §0.2a: an unregistered data / `With` / `Changed` / `Added` component empties
/// the query (nothing can match a never-registered component); an unregistered
/// `Without` silently disables that filter (over-match). Both are surfaced
/// through the world's `ErrorHandler` (`anomaly!`) — the query never fails
/// silently.
pub(crate) fn collect_matching_entities(
    world: &World,
    ctx: &ScriptContext,
    descs: &[QueryDesc],
) -> Vec<Entity> {
    let mut qb = world.query_builder();
    for desc in descs {
        let id = match ctx.binding(&desc.type_name) {
            Some(b) => b.id,
            None => {
                // A missing binding empties every mode except `Without` (which
                // only over-matches when its filter is dropped).
                match desc.mode {
                    QueryMode::Without => {
                        apex_core::anomaly!(
                            world, Severity::Warn, "script query", None, None,
                            "Without filter component '{}' is not registered — filter ignored, query may over-match",
                            desc.type_name
                        );
                        continue;
                    }
                    _ => {
                        apex_core::anomaly!(
                            world, Severity::Warn, "script query", None, None,
                            "component '{}' is not registered — query yields no results",
                            desc.type_name
                        );
                        return Vec::new();
                    }
                }
            }
        };
        qb = match desc.mode {
            // Read AND Write both need read access here (the snapshot only reads;
            // the actual mutation happens in `commit` via `DynQueryMut`).
            QueryMode::Read | QueryMode::Write => qb.read_id(id),
            QueryMode::With     => qb.with_id(id),
            QueryMode::Without  => qb.exclude_id(id),
            QueryMode::Changed  => qb.changed_id(id),
            QueryMode::Added    => qb.added_id(id),
        };
    }

    // Build never errors here: we resolve names ourselves (no `read_name`, so no
    // UnknownComponent) and never request writes on the read builder (so no
    // WriteNotSupported). Treat any surprise as an empty result.
    match qb.build() {
        Ok(q) => q.iter().map(|item| item.entity()).collect(),
        Err(_) => Vec::new(),
    }
}

// ── QueryIteratorState ─────────────────────────────────────────

struct IterState {
    /// Matching entities, snapshotted at `query()` time (in iteration order).
    entities: Vec<Entity>,
    /// The parsed descriptors — needed to rebuild per-entity read access and the
    /// `_meta.writes` set on each `next()`.
    descs: Vec<QueryDesc>,
    cursor: usize,
    /// The previously yielded table — auto-committed on the next `next()`.
    pending: Option<mlua::Table>,
}

// ── Building the entity table ──────────────────────────────────

/// Build the Lua table for one entity, reading its Read/Write components through
/// a core read [`DynQuery`]. Returns `None` if the entity no longer matches
/// (it was snapshotted, so this only happens defensively).
pub(crate) fn build_entity_table(
    lua: &mlua::Lua,
    world: &World,
    ctx: &ScriptContext,
    entity: Entity,
    descs: &[QueryDesc],
) -> mlua::Result<Option<mlua::Table>> {
    // Resolve the data components (Read/Write modes) and build a read query over
    // them. `With`/`Without`/`Changed`/`Added` are pure filters — already applied
    // at snapshot time — so they are not part of the per-entity access.
    let mut qb = world.query_builder();
    let mut data: Vec<(&QueryDesc, ComponentId, ReadFn)> = Vec::new();
    for desc in descs {
        if !matches!(desc.mode, QueryMode::Read | QueryMode::Write) {
            continue;
        }
        match ctx.binding(&desc.type_name) {
            Some(b) => {
                qb = qb.read_id(b.id);
                data.push((desc, b.id, b.read));
            }
            None => {
                // §0.2a (E8): the binding was present when the snapshot was built
                // but is gone now — the component silently vanishes from the table
                // the script sees. Should not happen; surface it (throttled).
                apex_core::anomaly!(
                    world, Severity::Warn, "script query", Some(entity), None,
                    "binding for '{}' disappeared mid-iteration — component omitted from entity table",
                    desc.type_name
                );
            }
        }
    }

    let query = match qb.build() {
        Ok(q) => q,
        Err(_) => return Ok(None),
    };
    let item = match query.get(entity) {
        Some(i) => i,
        None => return Ok(None),
    };

    let entity_id = encode_entity_id(entity);
    let t = lua.create_table()?;
    // Full "index:generation" id (E10), not a bare index — see encode_entity_id.
    t.set("entity", entity_id.as_str())?;

    let meta = lua.create_table()?;
    // Mirror the id into `_meta` so commit re-resolves THIS entity (full
    // generation) against the live world, rejecting stale/forged tables (E1+E10).
    meta.set("entity", entity_id.as_str())?;
    let writes = lua.create_table()?;
    for (desc, _id, _read) in &data {
        if matches!(desc.mode, QueryMode::Write) {
            // Marker set: the value is irrelevant, commit re-resolves the column
            // by the binding's ComponentId (a forged col index cannot mislead it).
            writes.set(desc.type_name.as_str(), 1)?;
        }
    }
    meta.set("writes", writes)?;
    t.set("_meta", meta)?;

    for (desc, id, read) in &data {
        let ptr = match item.get_ptr(*id) {
            Some(p) => p,
            None => continue, // archetype lacks it — should not happen post-match.
        };
        // SAFETY: `ptr` points at this entity's component (id resolved against the
        // matched archetype); `read` is the binding paired with `id`, so it
        // interprets the bytes as the right type. The `&World` borrow keeps the
        // storage alive and structurally frozen for the read.
        let val = unsafe { (read)(ptr, lua)? };

        let key = desc.type_name.to_lowercase();
        t.set(key.as_str(), val)?;

        // For Read components: warn if the script tries to modify the sub-table
        // (a Read term grants no write-back).
        if matches!(desc.mode, QueryMode::Read) {
            if let Ok(sub_table) = t.get::<mlua::Table>(key.as_str()) {
                let mt = lua.create_table()?;
                let type_name = desc.type_name.clone();
                mt.set("__newindex", lua.create_function(move |_, (_t, field, _val): (mlua::Table, String, mlua::Value)| {
                    log::warn!(
                        "[script] attempt to modify Read component '{}' (field '{}') — use Write:{}",
                        type_name, field, type_name
                    );
                    Ok(())
                })?)?;
                sub_table.set_metatable(Some(mt));
            }
        }
    }

    Ok(Some(t))
}

// ── commit(entity_table) ───────────────────────────────────────

pub(crate) fn commit_entity_table(
    ctx: &ScriptContext,
    entity_table: &mlua::Table,
) -> mlua::Result<()> {
    let meta: mlua::Table = match entity_table.get("_meta") {
        Ok(m) => m,
        Err(_) => {
            log::warn!("commit: table is not from a query (no _meta), skipped");
            return Ok(());
        }
    };

    // Decode the FULL entity id (index + generation) stored in `_meta` (E10).
    let meta_entity_raw: mlua::Value = meta.get("entity")?;
    let entity = match parse_entity_id(&meta_entity_raw) {
        Some(e) => e,
        None => {
            log::warn!("commit: _meta.entity is not a valid entity id, skipped");
            return Ok(());
        }
    };
    let writes: mlua::Table = meta.get("writes")?;

    // Gather the write plan — (ComponentId, write fn, value) — while we still
    // hold only the shared bindings + the Lua table. Collecting owned values here
    // means the ctx/Lua reads finish BEFORE we materialize `&mut World`.
    let mut plan: Vec<(ComponentId, WriteFn, mlua::Value)> = Vec::new();
    for pair in writes.pairs::<String, mlua::Value>() {
        let (type_name, _marker) = pair?;
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
        plan.push((binding.id, binding.write, val));
    }
    if plan.is_empty() {
        return Ok(());
    }

    // SAFETY: the commit runs between Lua iterations, so no other borrow of the
    // world is live, and the gather phase above already released its ctx/Lua
    // reads (see `ScriptContext::world_ptr_mut`).
    let world = unsafe { &mut *ctx.world_ptr_mut() };

    // Build a dynamic WRITE query declaring exactly the components we intend to
    // write. `get_mut(entity)` re-resolves the entity against the LIVE allocator:
    // a stale generation or a slot reused by a new tenant yields `None` (E10), so
    // a forged `_meta` can never steer a write into the wrong entity (E1). Column
    // access goes through `DynItemMut::get_mut_ptr`, which enforces the write
    // declaration (S7) and stamps the change tick.
    //
    // Trust boundary: the write-set comes from `_meta.writes` (script-authored),
    // so the S7 gate is tautological in this phase-A path — its scheduler-visible
    // enforcement arrives with phase-B script systems that declare access up
    // front. Memory safety does not depend on it: id-resolution plus the
    // allocator lookup rule out OOB and type confusion whatever `_meta` claims.
    let mut qb = world.query_builder_mut();
    for (id, _, _) in &plan {
        qb = qb.write_id(*id);
    }
    let mut query = match qb.build() {
        Ok(q) => q,
        // The only reachable build error is a duplicate write id (forged
        // `_meta.writes`) — reject the whole commit cleanly.
        Err(_) => return Ok(()),
    };
    let mut item = match query.get_mut(entity) {
        Some(i) => i,
        // Entity dead / reused (E10) or no longer in a matching archetype — skip
        // cleanly rather than write into a wrong row.
        None => return Ok(()),
    };
    for (id, write_fn, val) in &plan {
        // SAFETY: `get_mut_ptr` returns a pointer valid for this row (bounds +
        // the query's exclusive world access); `write_fn` is paired with `id`, so
        // it stores the right type. Scriptable components are non-ZST structs, so
        // the ZST dangling-sentinel case does not arise.
        if let Some(ptr) = item.get_mut_ptr(*id) {
            unsafe { (write_fn)(ptr, val); }
        }
    }

    Ok(())
}

// ── Creating the Lua iterator factory ──────────────────────────

pub(crate) fn create_query_iter_fn(
    lua: &mlua::Lua,
    entities: Vec<Entity>,
    descs: Vec<QueryDesc>,
) -> mlua::Result<mlua::Function> {
    let state = Rc::new(RefCell::new(IterState {
        entities,
        descs,
        cursor:  0,
        pending: None,
    }));

    lua.create_function(move |lua, ()| {
        let mut st = state.borrow_mut();

        // Auto-commit the previous entity (if enabled).
        if let Some(table) = st.pending.take() {
            let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
                .ok_or_else(|| mlua::Error::runtime("no ScriptContext in Lua app data"))?;
            let ctx_ref = ctx.borrow();
            if ctx_ref.auto_commit {
                commit_entity_table(&ctx_ref, &table)?;
            }
        }

        loop {
            let entity = match st.entities.get(st.cursor).copied() {
                Some(e) => e,
                None    => return Ok(mlua::Value::Nil),
            };
            st.cursor += 1;

            let ctx = lua.app_data_ref::<Rc<RefCell<ScriptContext>>>()
                .ok_or_else(|| mlua::Error::runtime("no ScriptContext in Lua app data"))?;
            let ctx_ref = ctx.borrow();
            let world = ctx_ref.world_ref();

            match build_entity_table(lua, world, &ctx_ref, entity, &st.descs)? {
                Some(table) => {
                    // Store for auto-commit on the next call.
                    st.pending = Some(table.clone());
                    return Ok(mlua::Value::Table(table));
                }
                None => continue, // entity no longer matches — skip it.
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::component::Component;
    use crate::context::ComponentBinding;

    #[derive(Clone, Copy)]
    struct TestComp {
        v: i32,
    }
    impl Component for TestComp {}

    fn test_comp_binding(id: ComponentId) -> ComponentBinding {
        ComponentBinding {
            name: "TestComp",
            id,
            read: |ptr: *const u8, _lua: &mlua::Lua| {
                let c = unsafe { &*(ptr as *const TestComp) };
                Ok(mlua::Value::Integer(c.v as i64))
            },
            write: |ptr: *mut u8, val: &mlua::Value| {
                if let mlua::Value::Integer(i) = val {
                    unsafe { (*(ptr as *mut TestComp)).v = *i as i32; }
                    true
                } else {
                    false
                }
            },
        }
    }

    /// Build a query-shaped table `{ testcomp = value, _meta = { entity, writes } }`.
    fn write_table(lua: &mlua::Lua, entity: &str, value: i64) -> mlua::Table {
        let t = lua.create_table().unwrap();
        t.set("testcomp", value).unwrap();
        let meta = lua.create_table().unwrap();
        meta.set("entity", entity).unwrap();
        let writes = lua.create_table().unwrap();
        writes.set("TestComp", 1).unwrap();
        meta.set("writes", writes).unwrap();
        t.set("_meta", meta).unwrap();
        t
    }

    /// The skip paths that need no world: a table with no `_meta`, a garbage
    /// entity id, and an empty write-set all return cleanly (no panic).
    #[test]
    fn commit_skips_non_query_tables_cleanly() {
        let lua = mlua::Lua::new();
        let ctx = ScriptContext::new();

        // No _meta at all.
        let t = lua.create_table().unwrap();
        commit_entity_table(&ctx, &t).expect("skips cleanly");

        // _meta present but garbage entity id.
        let t = lua.create_table().unwrap();
        let meta = lua.create_table().unwrap();
        meta.set("entity", "not-an-id").unwrap();
        meta.set("writes", lua.create_table().unwrap()).unwrap();
        t.set("_meta", meta).unwrap();
        commit_entity_table(&ctx, &t).expect("skips cleanly");

        // Valid id, empty writes → no-op (never touches the world, so no
        // world_ptr is required).
        let t = lua.create_table().unwrap();
        let meta = lua.create_table().unwrap();
        meta.set("entity", "0:0").unwrap();
        meta.set("writes", lua.create_table().unwrap()).unwrap();
        t.set("_meta", meta).unwrap();
        commit_entity_table(&ctx, &t).expect("skips cleanly");
    }

    /// E1 + E10: a commit whose `_meta.entity` is stale (reused generation) must
    /// NOT write; a commit for the LIVE entity must. The write goes through
    /// `DynQueryMut::get_mut`, which resolves the entity against the allocator —
    /// so forgery is rejected without any private OOB dereference.
    #[test]
    fn commit_rejects_stale_entity_and_writes_live_one() {
        let lua = mlua::Lua::new();
        let mut world = World::new();
        let e = world.spawn((TestComp { v: 10 },));
        let id = world.registry().get_id::<TestComp>().expect("registered on spawn");

        let mut ctx = ScriptContext::new();
        ctx.add_binding(test_comp_binding(id));
        // SAFETY: the ptr is used only for the commits below, all before
        // `clear_world_ptr`; `world` is not otherwise borrowed meanwhile.
        unsafe { ctx.set_world_ptr(&mut world); }

        // Stale generation → must be ignored.
        let stale = format!("{}:{}", e.index(), e.generation().wrapping_add(1));
        commit_entity_table(&ctx, &write_table(&lua, &stale, 999)).unwrap();

        // Live entity → must apply.
        let real = encode_entity_id(e);
        commit_entity_table(&ctx, &write_table(&lua, &real, 42)).unwrap();

        ctx.clear_world_ptr();
        assert_eq!(
            world.get::<TestComp>(e).map(|c| c.v),
            Some(42),
            "live-entity write applied; stale-generation write ignored"
        );
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

    /// The descriptor parser maps every access keyword — including the reactive
    /// `Changed:`/`Added:` filters — to the right `QueryMode`; a bare name
    /// defaults to `Read`.
    #[test]
    fn parse_desc_covers_every_mode() {
        let cases = [
            ("Read:Position",    QueryMode::Read),
            ("Write:Velocity",   QueryMode::Write),
            ("With:Player",      QueryMode::With),
            ("Without:Frozen",   QueryMode::Without),
            ("Changed:Position", QueryMode::Changed),
            ("added:Health",     QueryMode::Added), // lowercase prefix accepted
            ("Position",         QueryMode::Read),  // bare name = Read
        ];
        for (input, mode) in cases {
            let d = parse_one_desc(input);
            assert_eq!(d.mode, mode, "mode of {input:?}");
            assert!(!d.type_name.contains(':'), "prefix stripped from {input:?}");
        }
    }
}
