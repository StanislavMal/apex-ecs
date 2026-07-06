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
