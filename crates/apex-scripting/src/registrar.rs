//! `ScriptableRegistrar` — a trait implemented via `#[derive(Scriptable)]`.
//!
//! Provides two-way conversion of a component to/from `mlua::Value`
//! and registration of its constructor in Lua.
//!
//! Also contains `ResourceBinding` — for accessing global resources from scripts.
//!
//! # Manual implementation
//!
//! ```ignore
//! struct Health { current: f32, max: f32 }
//!
//! impl ScriptableRegistrar for Health {
//!     fn type_name_str() -> &'static str { "Health" }
//!
//!     fn field_names() -> &'static [&'static str] { &["current", "max"] }
//!
//!     fn to_lua<'lua>(&self, lua: &'lua mlua::Lua) -> mlua::Result<mlua::Value<'lua>> {
//!         let t = lua.create_table()?;
//!         t.set("current", self.current)?;
//!         t.set("max", self.max)?;
//!         Ok(mlua::Value::Table(t))
//!     }
//!
//!     fn from_lua(val: &mlua::Value) -> Option<Self> {
//!         let t = val.as_table()?;
//!         let current = t.get::<f32>("current").ok()?;
//!         let max     = t.get::<f32>("max").ok()?;
//!         Some(Self { current, max })
//!     }
//!
//!     fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
//!         let t = lua.create_table()?;
//!         t.set("new", lua.create_function(|lua, (current, max): (f32, f32)| {
//!             let t = lua.create_table()?;
//!             t.set("current", current)?;
//!             t.set("max", max)?;
//!             Ok(t)
//!         })?)?;
//!         lua.globals().set("Health", t)
//!     }
//! }
//! ```

/// Trait for components accessible from Lua scripts.
///
/// Generated automatically via `#[derive(Scriptable)]`.
/// Can be implemented manually for non-standard types.
pub trait ScriptableRegistrar: Sized + 'static {
    /// String type name — used as the key in the table inside the query iterator.
    fn type_name_str() -> &'static str;

    /// Names of the struct fields — for documentation and debugging.
    fn field_names() -> &'static [&'static str];

    /// Convert the component value into an mlua::Value (usually a Table).
    fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value>;

    /// Reconstruct the component from an mlua::Value.
    ///
    /// Returns `None` if the Value is not a Table or the fields are missing/have
    /// the wrong type. This is a normal situation when working with scripts.
    fn from_lua(val: &mlua::Value) -> Option<Self>;

    /// Register the type's constructor in the Lua globals.
    ///
    /// For example: `Position.new(x, y)`, `TileKind.Floor = 0`
    ///
    /// Called once during `ScriptEngine::register_component::<T>()`.
    fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()>;
}

// ── ResourceBinding ─────────────────────────────────────────────

/// Information about a resource registered for access from Lua scripts.
///
/// Analogous to `ComponentBinding`, but for global resources (`World.resources`).
#[derive(Clone)]
pub struct ResourceBinding {
    /// String type name of the resource.
    pub name: &'static str,
    /// Read the resource from `&World` → mlua::Value.
    /// Takes Lua so it can create tables.
    pub read:   fn(&mlua::Lua, &apex_core::World) -> mlua::Result<mlua::Value>,
    /// Write the resource into `&mut World` from an mlua::Value.
    /// Returns `false` if the type is wrong.
    pub write:  fn(&mlua::Value, &mut apex_core::World) -> bool,
}

// ── EventBinding ────────────────────────────────────────────────

/// Information about an event registered for emission from Lua scripts.
#[derive(Clone)]
pub struct EventBinding {
    /// String type name of the event.
    pub name: &'static str,
    /// Emit the event into `&mut World` (takes mlua::Value, converts to T).
    /// Returns `false` if the event is not registered or the type is wrong.
    pub emit: fn(&mlua::Value, &mut apex_core::World) -> bool,
}

// ── ScriptVm token + access translation (phase B) ───────────────

use std::any::TypeId;
use std::collections::HashMap;

use apex_core::access::AccessDescriptor;
use apex_core::component::ComponentId;

use crate::context::ComponentBinding;
use crate::iterators::{QueryDesc, QueryMode};

/// ZST marker-resource token for phase-B script systems.
///
/// Every script system declares a WRITE on it, so the scheduler serializes
/// script systems against each other — one Lua VM must never run two systems
/// concurrently — while still parallelizing them against Rust systems with
/// disjoint access. The exclusivity lives in the access model, not a `Mutex` or
/// a `Send` fiction (wave 3 §5). Only its `TypeId` participates in conflict
/// detection; it is never stored as a real resource.
pub struct ScriptVm;

/// The translated access of a script system: the scheduler [`AccessDescriptor`]
/// (keyed by `TypeId`) plus the declared component ids (keyed by `ComponentId`,
/// for the runtime `query`/`commit` enforcement), plus any unresolved names.
pub(crate) struct ScriptAccess {
    /// Scheduler-facing access (`TypeId` vectors + the `ScriptVm` write token).
    pub descriptor: AccessDescriptor,
    /// Declared read component ids (Read + Changed/Added) — for the runner's
    /// per-run enforcement (`ScriptContext::declares_read`).
    pub read_ids:  Vec<ComponentId>,
    /// Declared write component ids — for `ScriptContext::declares_write`.
    pub write_ids: Vec<ComponentId>,
    /// Data/reactive descriptor components with no binding (an under-declaration
    /// the caller MUST surface loudly, §0.2a).
    pub unresolved: Vec<String>,
}

/// Translate a script system's parsed query descriptors into its [`ScriptAccess`].
///
/// Component access is keyed by `TypeId` for the scheduler (conflict detection
/// compares `TypeId` vectors, not `ComponentId`s) and by `ComponentId` for the
/// runtime enforcement, resolved through the component bindings. A mandatory
/// `write` on [`ScriptVm`] serializes Lua↔Lua. `With`/`Without` are pure
/// archetype-structure filters and declare no data access (matching a typed
/// `With<T>`/`Without<T>`); `Read`/`Write`/`Changed`/`Added` touch component data
/// or change ticks, so they declare read/write access.
///
/// A missing write/read binding is reported in `unresolved`: the caller must
/// refuse to register an under-declared system, else the scheduler could run it
/// concurrently with a conflicting one (a data race).
pub(crate) fn descs_to_access(
    bindings: &HashMap<&'static str, ComponentBinding>,
    descs: &[QueryDesc],
) -> ScriptAccess {
    let mut descriptor = AccessDescriptor::new();
    // Lua↔Lua serialization token: one VM, never two script systems at once.
    descriptor.writes.push(TypeId::of::<ScriptVm>());
    // A script system may spawn/despawn through its per-system Commands slot
    // (structural ops are deferred there — B4). Declaring `uses_commands` makes the
    // scheduler seed a rank-deterministic id block for the slot under
    // `set_deterministic_spawn`, so script spawns get run-to-run identical ids
    // (D8b). It does not affect conflict detection (only reads/writes do) and the
    // system is already single-task (stateful), so it changes nothing else.
    descriptor.uses_commands = true;

    let mut read_ids = Vec::new();
    let mut write_ids = Vec::new();
    let mut unresolved = Vec::new();

    for desc in descs {
        // With/Without touch no component data — no access declaration.
        if matches!(desc.mode, QueryMode::With | QueryMode::Without) {
            continue;
        }
        let binding = match bindings.get(desc.type_name.as_str()) {
            Some(b) => b,
            None => {
                unresolved.push(desc.type_name.clone());
                continue;
            }
        };
        match desc.mode {
            QueryMode::Write => {
                if !descriptor.writes.contains(&binding.type_id) {
                    descriptor.writes.push(binding.type_id);
                }
                if !write_ids.contains(&binding.id) {
                    write_ids.push(binding.id);
                }
            }
            // Read + the reactive filters read the component's data / change ticks.
            QueryMode::Read | QueryMode::Changed | QueryMode::Added => {
                if !descriptor.reads.contains(&binding.type_id) {
                    descriptor.reads.push(binding.type_id);
                }
                if !read_ids.contains(&binding.id) {
                    read_ids.push(binding.id);
                }
            }
            QueryMode::With | QueryMode::Without => unreachable!("filtered above"),
        }
    }

    // A component both read and written is a write (avoid listing it in both,
    // which would be redundant for conflict detection).
    descriptor.reads.retain(|t| !descriptor.writes.contains(t));
    read_ids.retain(|id| !write_ids.contains(id));

    ScriptAccess { descriptor, read_ids, write_ids, unresolved }
}

#[cfg(test)]
mod access_tests {
    use super::*;
    use apex_core::component::ComponentId;
    use crate::iterators::parse_query_descs;

    fn binding(name: &'static str, id: ComponentId, tid: TypeId) -> ComponentBinding {
        ComponentBinding {
            name,
            id,
            type_id: tid,
            read: |_p, _l| Ok(mlua::Value::Nil),
            write: |_p, _v| true,
        }
    }

    // Distinct marker types to hand out stable, distinct TypeIds.
    struct Position;
    struct Velocity;
    struct Health;
    struct Player;

    fn bindings() -> HashMap<&'static str, ComponentBinding> {
        let mut m = HashMap::new();
        m.insert("Position", binding("Position", ComponentId(0), TypeId::of::<Position>()));
        m.insert("Velocity", binding("Velocity", ComponentId(1), TypeId::of::<Velocity>()));
        m.insert("Health", binding("Health", ComponentId(2), TypeId::of::<Health>()));
        m.insert("Player", binding("Player", ComponentId(3), TypeId::of::<Player>()));
        m
    }

    fn descs(items: &[&str]) -> Vec<QueryDesc> {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        for (i, s) in items.iter().enumerate() {
            t.set(i + 1, *s).unwrap();
        }
        parse_query_descs(&t).unwrap()
    }

    /// Read/Write map to reads/writes by TypeId; the ScriptVm token is always a
    /// write; `With` declares no data access; a component both written and
    /// filtered-changed lands only in writes.
    #[test]
    fn translates_modes_to_typed_access() {
        let b = bindings();
        let sa = descs_to_access(
            &b,
            &descs(&["Write:Position", "Read:Velocity", "With:Player", "Changed:Position"]),
        );

        assert!(sa.unresolved.is_empty(), "all components are registered");
        assert!(sa.descriptor.writes.contains(&TypeId::of::<ScriptVm>()), "VM token declared");
        assert!(sa.descriptor.writes.contains(&TypeId::of::<Position>()), "Write:Position");
        assert!(sa.descriptor.reads.contains(&TypeId::of::<Velocity>()), "Read:Velocity");
        // Position is written AND Changed-filtered → only a write, never a read.
        assert!(!sa.descriptor.reads.contains(&TypeId::of::<Position>()), "Position is a write, not a read");
        // With:Player declares no data access.
        assert!(!sa.descriptor.reads.contains(&TypeId::of::<Player>()));
        assert!(!sa.descriptor.writes.contains(&TypeId::of::<Player>()));
        // ComponentId sets mirror the read/write split (Position id=0 is a write,
        // Velocity id=1 a read; Player id=3, a With filter, is in neither).
        assert_eq!(sa.write_ids, vec![ComponentId(0)]);
        assert_eq!(sa.read_ids, vec![ComponentId(1)]);
    }

    /// Two script systems ALWAYS conflict (both write the ScriptVm token) — the
    /// scheduler serializes Lua↔Lua even when their component access is disjoint.
    #[test]
    fn two_script_systems_conflict_on_vm_token() {
        let b = bindings();
        let a = descs_to_access(&b, &descs(&["Write:Position"]));
        let c = descs_to_access(&b, &descs(&["Write:Velocity"]));
        assert!(
            a.descriptor.conflicts_with(&c.descriptor),
            "disjoint component access, but the shared ScriptVm write serializes them"
        );
    }

    /// A script writing Position conflicts with a Rust system writing Position
    /// (same TypeId), and does NOT conflict with one writing only Velocity.
    #[test]
    fn script_access_conflicts_with_rust_by_typeid() {
        let b = bindings();
        let script = descs_to_access(&b, &descs(&["Write:Position"]));

        let rust_pos = apex_core::access_desc!(write<Position>);
        let rust_vel = apex_core::access_desc!(write<Velocity>);
        assert!(script.descriptor.conflicts_with(&rust_pos), "shared Position write conflicts");
        assert!(!script.descriptor.conflicts_with(&rust_vel), "disjoint component → no conflict");
    }

    /// A data/reactive descriptor with no binding is reported as unresolved
    /// (§0.2a) — the caller must refuse to register an under-declared system.
    #[test]
    fn missing_binding_is_reported_unresolved() {
        let b = bindings();
        let sa = descs_to_access(&b, &descs(&["Write:Position", "Read:Ghost"]));
        assert_eq!(sa.unresolved, vec!["Ghost".to_string()]);
    }
}
