//! apex-scripting — Lua scripting integration with Apex ECS.
//!
//! # Architecture
//!
//! ```text
//! ScriptEngine
//!   ├── mlua::Lua             — Lua 5.4 VM
//!   ├── ScriptContext         — World ↔ Lua bridge (Rc<RefCell<>>)
//!   ├── HashMap<name, RegistryKey> — compiled scripts
//!   └── FileWatcher           — hot-reload of .lua files
//!
//! ScriptContext
//!   ├── delta_time: f32
//!   ├── world_ptr:  NonNull<World>   — lives ≤ ScriptEngine::run() / a system run
//!   └── deferred_{despawns,spawns}   — drained into a Commands buffer afterwards
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use std::path::Path;
//! use apex_scripting::{ScriptEngine, Scriptable};
//!
//! // Register components as Scriptable
//! #[derive(Clone, Copy, Scriptable)]
//! struct Position { x: f32, y: f32 }
//!
//! #[derive(Clone, Copy, Scriptable)]
//! struct Velocity { x: f32, y: f32 }
//!
//! // Create the engine with a scripts directory
//! let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));
//!
//! // Register components for access from scripts
//! engine.register_component::<Position>(&world);
//! engine.register_component::<Velocity>(&world);
//!
//! // Load .lua files
//! engine.load_scripts().expect("failed to load scripts");
//!
//! // Game loop
//! loop {
//!     engine.poll_hot_reload();
//!     engine.run(dt, &mut world);
//!     world.tick();
//! }
//! ```
//!
//! # Script example (scripts/game.lua)
//!
//! ```lua
//! function run()
//!     local dt = delta_time()
//!
//!     for entity in query({"Read:Position", "Write:Velocity"}) do
//!         entity.velocity.x = entity.velocity.x * 0.99
//!         entity.velocity.y = entity.velocity.y * 0.99
//!         entity.position.x = entity.position.x + entity.velocity.x * dt
//!         entity.position.y = entity.position.y + entity.velocity.y * dt
//!         commit(entity)
//!     end
//!
//!     if entity_count() < 10 then
//!         spawn_entity({
//!             position = Position.new(0.0, 0.0),
//!             velocity = Velocity.new(1.0, 0.5),
//!         })
//!     end
//! end
//! ```

pub mod context;
pub mod error;
pub mod iterators;
pub mod lua_api;
pub mod registrar;
pub mod script_engine;

pub use context::ScriptContext;
pub use error::ScriptError;
pub use registrar::{ScriptVm, ScriptableRegistrar};
pub use script_engine::ScriptEngine;

// Re-export the macro from apex-macros so users write
// `use apex_scripting::Scriptable` instead of importing it separately
pub use apex_macros::Scriptable;

use apex_core::world::World;

/// Extension trait: registers types in both World and ScriptEngine at once.
///
/// Removes the need for double registration:
/// ```ignore
/// // Before:
/// world.register_component::<Position>();
/// engine.register_component::<Position>(&world);
///
/// // After:
/// world.register_scriptable::<Position>(&mut engine);
/// ```
pub trait WorldScriptingExt {
    fn register_scriptable<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + apex_core::component::Component;

    fn register_scriptable_resource<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static;

    fn register_scriptable_event<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static;
}

impl WorldScriptingExt for World {
    fn register_scriptable<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + apex_core::component::Component,
    {
        self.register_component::<T>();
        engine.register_component::<T>(self);
    }

    fn register_scriptable_resource<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static,
    {
        engine.register_resource::<T>();
    }

    fn register_scriptable_event<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static,
    {
        self.add_event::<T>();
        engine.register_event::<T>();
    }
}
