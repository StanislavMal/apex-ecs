//! `ScriptContext` — the context between Rust and Lua within a single `run()`.
//!
//! # Lifetime and safety
//!
//! `ScriptContext` lives exactly as long as `ScriptEngine::run()`.
//! `world_ptr` is set before the script call and reset (`null`) immediately after.
//! Thus:
//! - The ptr is valid for the entire script execution
//! - Storing `ScriptContext` in a static and using it after `run()` is impossible
//!   without `unsafe`, which clearly signals an error
//!
//! # Deferred changes
//!
//! The Lua iterator holds a shared borrow on World through the ptr, so
//! structural changes (spawn/despawn) cannot be applied during iteration.
//! They accumulate in `deferred: Commands` and are applied after the script finishes.

use std::{
    collections::HashMap,
    ptr::NonNull,
};

use apex_core::{
    commands::Commands,
    component::ComponentId,
    world::World,
};

use crate::iterators::QueryDesc;
use crate::registrar::{ResourceBinding, EventBinding};

// ── ScriptSystemDecl ───────────────────────────────────────────

/// A phase-B script system declared from Lua via `system{ name, query, fn }`.
///
/// Holds the parsed query descriptors and a registry key for the Lua function.
/// The engine translates `descs` into a scheduler access declaration and builds
/// a NonSend runner that calls `fn` each frame.
pub(crate) struct ScriptSystemDecl {
    pub name:   String,
    pub descs:  Vec<QueryDesc>,
    pub fn_key: mlua::RegistryKey,
}

// ── DeclaredAccess ─────────────────────────────────────────────

/// The component ids a phase-B script system declared. Installed on the
/// `ScriptContext` for the duration of the system's run so `query`/`commit`
/// resolve ONLY declared components — an undeclared access is a scheduler-blind
/// live access and is refused loudly (§0.2a). `None` ⇒ the monolithic `run()`
/// fallback (one whole-world exclusive system), which is unrestricted.
#[derive(Default)]
pub(crate) struct DeclaredAccess {
    pub reads:  Vec<ComponentId>,
    pub writes: Vec<ComponentId>,
}

// ── ComponentBinding ───────────────────────────────────────────

/// Information about a component registered for scripting.
///
/// Holds component ↔ Lua conversion functions without being tied to a concrete type T.
#[derive(Clone)]
pub struct ComponentBinding {
    /// Component type name (matches the key in the query table)
    pub name: &'static str,
    /// ComponentId for lookup in archetypes
    pub id:   ComponentId,
    /// `TypeId` of the component. Needed to build a scheduler [`AccessDescriptor`]
    /// for a phase-B script system: conflict detection compares `TypeId` vectors
    /// (not `ComponentId`s), so a script's `Write:Position` must carry
    /// `TypeId::of::<Position>()` to conflict with a Rust `write<Position>`.
    pub type_id: std::any::TypeId,
    /// Read a component from Column[row] → mlua::Value
    pub read:  unsafe fn(*const u8, &mlua::Lua) -> mlua::Result<mlua::Value>,
    /// Write a component into Column[row] from mlua::Value; returns false if the type is wrong
    pub write: unsafe fn(*mut u8, &mlua::Value) -> bool,
}

// ── SpawnRequest ───────────────────────────────────────────────

/// A request to create an entity, formed from a script.
///
/// Holds a list of (name, RegistryKey) pairs — components for the new entity.
/// The RegistryKey allows deferring extraction of the mlua::Value until apply time.
pub struct SpawnRequest {
    /// List of components: (type name, RegistryKey with the Lua value)
    pub components: Vec<(String, mlua::RegistryKey)>,
}

// ── ScriptContext ──────────────────────────────────────────────

/// The bridge between a Lua script and the ECS world.
///
/// Stored in `Arc<RefCell<ScriptContext>>` inside `ScriptEngine`.
/// Accessed via `lua.set_app_data()` / `lua.app_data_ref()`.
pub struct ScriptContext {
    /// Current frame delta time — set before `run()`
    pub delta_time: f32,

    /// Raw pointer to the world. Lives exactly within `run()`.
    /// `None` means we are outside `run()` — any access through a script
    /// returns an error instead of UB.
    world_ptr: Option<NonNull<World>>,

    /// Buffer of deferred spawn/despawn commands.
    /// Applied after the script finishes via `apply_deferred()`.
    pub(crate) deferred: Commands,

    /// Buffer of spawn requests from scripts.
    pub(crate) deferred_spawns: Vec<SpawnRequest>,

    /// Registry of components accessible from scripts: name → binding
    pub(crate) bindings: HashMap<&'static str, ComponentBinding>,

    /// Registry of resources accessible from scripts: name → binding
    pub(crate) resource_bindings: HashMap<&'static str, ResourceBinding>,

    /// Registry of events accessible from scripts: name → binding
    pub(crate) event_bindings: HashMap<&'static str, EventBinding>,

    /// Buffer of deferred resource writes: (type_name, RegistryKey)
    /// Applied after the script finishes.
    ///
    /// The key is stored as an owned `String`, not `&'static str`. Previously
    /// `write_resource` did `Box::leak(name)` on EVERY call, which caused a
    /// linear memory leak for a script writing a resource every frame (E3).
    /// The binding lookup is by string anyway, so an owned key is correct and does not leak.
    pub(crate) deferred_resource_writes: Vec<(String, mlua::RegistryKey)>,

    /// Buffer of deferred events: (type_name, RegistryKey)
    /// Applied after the script finishes. Owned `String` key — see E3
    /// in the comment on `deferred_resource_writes`.
    pub(crate) deferred_events: Vec<(String, mlua::RegistryKey)>,

    /// Entity count — cached to avoid calling world through the ptr every time
    entity_count_cache: usize,

    /// Automatically call commit(entity) when moving to the next entity in the query iterator
    pub auto_commit: bool,

    /// Phase-B script systems declared this compile via `system{}` (drained by
    /// the engine when registering them with the scheduler).
    pub(crate) script_systems: Vec<ScriptSystemDecl>,

    /// The declared component access of the phase-B script system currently
    /// running (`None` for the monolithic `run()` fallback). `query`/`commit`
    /// refuse any component not in this set.
    pub(crate) declared: Option<DeclaredAccess>,
}

impl ScriptContext {
    pub fn new() -> Self {
        Self {
            delta_time:              0.0,
            world_ptr:               None,
            deferred:                Commands::new(),
            deferred_spawns:         Vec::new(),
            bindings:                HashMap::new(),
            resource_bindings:       HashMap::new(),
            event_bindings:          HashMap::new(),
            deferred_resource_writes: Vec::new(),
            deferred_events:         Vec::new(),
            entity_count_cache:      0,
            auto_commit:             false,
            script_systems:          Vec::new(),
            declared:                None,
        }
    }

    // ── Lifetime management ────────────────────────────────────

    /// Set the world pointer before executing the script.
    pub(crate) unsafe fn set_world_ptr(&mut self, world: &mut World) {
        self.world_ptr         = Some(NonNull::new_unchecked(world as *mut World));
        self.entity_count_cache = world.entity_count();
        self.deferred.clear();
        self.deferred_resource_writes.clear();
        self.deferred_events.clear();
    }

    /// Set the world pointer from a SHARED `&World` — the phase-B script-system
    /// runner path.
    ///
    /// Unlike [`set_world_ptr`](Self::set_world_ptr) this stores a pointer derived
    /// from `&World`, so it may ONLY be read back as `&World` (via
    /// [`world_ref`](Self::world_ref) — used by the `query`/`commit` declared-cell
    /// path). It must NEVER reach [`world_mut`](Self::world_mut): a `&mut World`
    /// derived from a `&World` provenance is UB, and the stage that vends this
    /// `&World` holds only shared access. Structural changes from a script system
    /// go through the scheduler's per-system `Commands` slot instead (phase B4).
    pub(crate) unsafe fn set_world_ptr_shared(&mut self, world: &World) {
        self.world_ptr = Some(NonNull::new_unchecked(world as *const World as *mut World));
        self.entity_count_cache = world.entity_count();
        self.deferred.clear();
        self.deferred_resource_writes.clear();
        self.deferred_events.clear();
    }

    /// Reset the world pointer after the script finishes.
    pub(crate) fn clear_world_ptr(&mut self) {
        self.world_ptr = None;
    }

    // ── Phase-B declared access ────────────────────────────────

    /// Install the running script system's declared component access. While set,
    /// `query`/`commit` refuse any component not declared (§0.2a).
    pub(crate) fn set_declared_access(&mut self, reads: Vec<ComponentId>, writes: Vec<ComponentId>) {
        self.declared = Some(DeclaredAccess { reads, writes });
    }

    /// Clear the declared access (back to the unrestricted monolith default).
    pub(crate) fn clear_declared_access(&mut self) {
        self.declared = None;
    }

    /// Whether component `id` may be READ by the running script system. `true`
    /// when no declaration is installed (the monolithic `run()` fallback).
    pub(crate) fn declares_read(&self, id: ComponentId) -> bool {
        match &self.declared {
            None => true,
            Some(d) => d.reads.contains(&id) || d.writes.contains(&id),
        }
    }

    /// Whether component `id` may be WRITTEN by the running script system. `true`
    /// when no declaration is installed.
    pub(crate) fn declares_write(&self, id: ComponentId) -> bool {
        match &self.declared {
            None => true,
            Some(d) => d.writes.contains(&id),
        }
    }

    /// Drain the script systems declared this compile (for scheduler registration).
    pub(crate) fn take_script_systems(&mut self) -> Vec<ScriptSystemDecl> {
        std::mem::take(&mut self.script_systems)
    }

    /// Discard any deferred structural / resource / event operations WITHOUT
    /// applying them (the phase-B script-system path, where applying them would
    /// need a `&mut World` the concurrent stage does not grant — that is phase
    /// B4). Frees the associated Lua registry slots so they do not leak. Returns
    /// `true` if anything was discarded, so the caller can warn loudly (§0.2a).
    pub(crate) fn discard_deferred_structural(&mut self, lua: &mlua::Lua) -> bool {
        let mut had = false;
        for (_name, key) in std::mem::take(&mut self.deferred_resource_writes) {
            had = true;
            let _ = lua.remove_registry_value(key);
        }
        for (_name, key) in std::mem::take(&mut self.deferred_events) {
            had = true;
            let _ = lua.remove_registry_value(key);
        }
        for req in std::mem::take(&mut self.deferred_spawns) {
            had = true;
            for (_name, key) in req.components {
                let _ = lua.remove_registry_value(key);
            }
        }
        // Despawns (and any other structural commands) buffered in `deferred`.
        if !self.deferred.is_empty() {
            had = true;
            self.deferred.clear();
        }
        had
    }

    /// Get `&World` — read-only (query iterators).
    pub(crate) fn world_ref(&self) -> &World {
        unsafe {
            self.world_ptr
                .expect("ScriptContext::world_ref called outside run()")
                .as_ref()
        }
    }

    /// Get `&mut World` — for applying deferred commands.
    pub(crate) unsafe fn world_mut(&mut self) -> &mut World {
        self.world_ptr
            .expect("ScriptContext::world_mut called outside run()")
            .as_mut()
    }

    // ── API for Lua functions ─────────────────────────────────

    pub fn delta_time(&self) -> f32 {
        self.delta_time
    }

    pub fn entity_count(&self) -> usize {
        self.entity_count_cache
    }

    pub fn queue_spawn(&mut self, request: SpawnRequest) {
        self.deferred_spawns.push(request);
    }

    pub fn queue_despawn(&mut self, entity: apex_core::Entity) {
        self.deferred.despawn(entity);
    }

    pub(crate) fn apply_deferred(&mut self) {
        let mut deferred = std::mem::take(&mut self.deferred);
        let world = unsafe { self.world_mut() };
        deferred.apply(world);
        self.deferred = deferred;
    }

    /// Apply deferred resource writes and event emissions.
    pub(crate) fn apply_deferred_resources_and_events(
        &mut self,
        lua: &mlua::Lua,
    ) {
        let writes = std::mem::take(&mut self.deferred_resource_writes);
        let events = std::mem::take(&mut self.deferred_events);

        if writes.is_empty() && events.is_empty() {
            return;
        }

        // Collect the bindings before borrowing world
        type ApplyFn = fn(&mlua::Value, &mut World) -> bool;

        let write_infos: Vec<(&'static str, ApplyFn)> = writes.iter()
            .filter_map(|(name, _)| {
                self.resource_bindings.get(name.as_str())
                    .map(|b| (b.name, b.write))
            })
            .collect();

        let emit_infos: Vec<(&'static str, ApplyFn)> = events.iter()
            .filter_map(|(name, _)| {
                self.event_bindings.get(name.as_str())
                    .map(|b| (b.name, b.emit))
            })
            .collect();

        let world = unsafe { self.world_mut() };

        for (type_name, key) in writes {
            let val: mlua::Value = match lua.registry_value(&key) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for (name, write_fn) in &write_infos {
                if *name == type_name.as_str() {
                    write_fn(&val, world);
                }
            }
            let _ = lua.remove_registry_value(key);
        }

        for (type_name, key) in events {
            let val: mlua::Value = match lua.registry_value(&key) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for (name, emit_fn) in &emit_infos {
                if *name == type_name.as_str() {
                    emit_fn(&val, world);
                }
            }
            let _ = lua.remove_registry_value(key);
        }
    }

    // ── Registration ──────────────────────────────────────────

    pub(crate) fn add_binding(&mut self, binding: ComponentBinding) {
        self.bindings.insert(binding.name, binding);
    }

    pub(crate) fn binding(&self, name: &str) -> Option<&ComponentBinding> {
        self.bindings.get(name)
    }

    pub(crate) fn add_resource_binding(&mut self, binding: ResourceBinding) {
        self.resource_bindings.insert(binding.name, binding);
    }

    #[allow(dead_code)]
    pub(crate) fn resource_binding(&self, name: &str) -> Option<&ResourceBinding> {
        self.resource_bindings.get(name)
    }

    pub(crate) fn add_event_binding(&mut self, binding: EventBinding) {
        self.event_bindings.insert(binding.name, binding);
    }

    #[allow(dead_code)]
    pub(crate) fn event_binding(&self, name: &str) -> Option<&EventBinding> {
        self.event_bindings.get(name)
    }

    // ── Resource access from Lua ──────────────────────────────

    pub fn read_resource(
        &self,
        lua: &mlua::Lua,
        type_name: &str,
    ) -> Option<mlua::Value> {
        let binding = self.resource_bindings.get(type_name)?;
        let world = self.world_ref();
        (binding.read)(lua, world).ok()
    }

    /// Queue a deferred resource write.
    ///
    /// Takes `&str` (not `&'static str`) — the name is copied into an owned
    /// `String` key. No `Box::leak`, so repeated calls with the same name
    /// do not accumulate a leak (E3).
    pub fn write_resource(
        &mut self,
        lua: &mlua::Lua,
        type_name: &str,
        value: mlua::Value,
    ) -> mlua::Result<()> {
        if !self.resource_bindings.contains_key(type_name) {
            log::warn!("write_resource: resource '{}' is not registered", type_name);
            return Ok(());
        }
        let key = lua.create_registry_value(value)?;
        self.deferred_resource_writes.push((type_name.to_owned(), key));
        Ok(())
    }

    /// Queue a deferred event. `&str` → owned `String` key,
    /// without `Box::leak` (E3).
    pub fn emit_event(
        &mut self,
        lua: &mlua::Lua,
        type_name: &str,
        value: mlua::Value,
    ) -> mlua::Result<()> {
        if !self.event_bindings.contains_key(type_name) {
            log::warn!("emit_event: event '{}' is not registered", type_name);
            return Ok(());
        }
        let key = lua.create_registry_value(value)?;
        self.deferred_events.push((type_name.to_owned(), key));
        Ok(())
    }
}

impl Default for ScriptContext {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registrar::{ResourceBinding, EventBinding};

    fn dummy_resource_write(_v: &mlua::Value, _w: &mut World) -> bool { true }
    fn dummy_resource_read(_lua: &mlua::Lua, _w: &World) -> mlua::Result<mlua::Value> {
        Ok(mlua::Value::Nil)
    }
    fn dummy_event_emit(_v: &mlua::Value, _w: &mut World) -> bool { true }

    /// E3: `write_resource` must NOT leak memory per call. Previously each call
    /// did `Box::leak(name)`, so a script writing the same resource every frame
    /// leaked linearly. The deferred buffer now owns the name as a `String`, so
    /// repeated calls with the SAME name allocate no permanent per-call storage.
    ///
    /// We assert the observable contract: N calls with one name produce exactly N
    /// queued writes, each carrying the correct owned name (round-trip), and the
    /// set of DISTINCT queued names stays at 1 (no name explosion / interning bug).
    #[test]
    fn write_resource_does_not_leak_per_call() {
        let lua = mlua::Lua::new();
        let mut ctx = ScriptContext::new();
        ctx.add_resource_binding(ResourceBinding {
            name:  "Score",
            read:  dummy_resource_read,
            write: dummy_resource_write,
        });

        const N: usize = 100;
        for _ in 0..N {
            ctx.write_resource(&lua, "Score", mlua::Value::Integer(1))
                .expect("queued");
        }

        // One queued write per call — round-trip of the name is intact.
        assert_eq!(ctx.deferred_resource_writes.len(), N);
        assert!(ctx.deferred_resource_writes.iter().all(|(n, _)| n == "Score"));

        // Only ONE distinct name across all calls: repeated same-name writes do
        // not grow a per-call leaked/interned name table.
        let distinct: std::collections::HashSet<&str> = ctx
            .deferred_resource_writes
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(distinct.len(), 1);
    }

    /// E3 (events): same no-leak contract for `emit_event`.
    #[test]
    fn emit_event_does_not_leak_per_call() {
        let lua = mlua::Lua::new();
        let mut ctx = ScriptContext::new();
        ctx.add_event_binding(EventBinding {
            name: "Boom",
            emit: dummy_event_emit,
        });

        const N: usize = 100;
        for _ in 0..N {
            ctx.emit_event(&lua, "Boom", mlua::Value::Integer(1))
                .expect("queued");
        }

        assert_eq!(ctx.deferred_events.len(), N);
        let distinct: std::collections::HashSet<&str> = ctx
            .deferred_events
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(distinct.len(), 1);
    }
}
