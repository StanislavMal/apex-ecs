//! apex-scripting — интеграция Lua-скриптинга с Apex ECS.
//!
//! # Архитектура
//!
//! ```text
//! ScriptEngine
//!   ├── mlua::Lua             — Lua 5.4 VM
//!   ├── ScriptContext         — мост World ↔ Lua (Rc<RefCell<>>)
//!   ├── HashMap<name, RegistryKey> — скомпилированные скрипты
//!   └── FileWatcher           — хот-релоад .lua файлов
//!
//! ScriptContext
//!   ├── delta_time: f32
//!   ├── world_ptr:  NonNull<World>   — живёт ≤ ScriptEngine::run()
//!   └── deferred:   Commands         — буфер spawn/despawn
//! ```
//!
//! # Использование
//!
//! ```ignore
//! use std::path::Path;
//! use apex_scripting::{ScriptEngine, Scriptable};
//!
//! // Регистрация компонентов как Scriptable
//! #[derive(Clone, Copy, Scriptable)]
//! struct Position { x: f32, y: f32 }
//!
//! #[derive(Clone, Copy, Scriptable)]
//! struct Velocity { x: f32, y: f32 }
//!
//! // Создание движка с директорией скриптов
//! let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));
//!
//! // Регистрация компонентов для доступа из скриптов
//! engine.register_component::<Position>(&world);
//! engine.register_component::<Velocity>(&world);
//!
//! // Загрузка .lua файлов
//! engine.load_scripts().expect("ошибка загрузки скриптов");
//!
//! // Game loop
//! loop {
//!     engine.poll_hot_reload();
//!     engine.run(dt, &mut world);
//!     world.tick();
//! }
//! ```
//!
//! # Пример скрипта (scripts/game.lua)
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
pub use registrar::ScriptableRegistrar;
pub use script_engine::ScriptEngine;

// Re-export макроса из apex-macros чтобы пользователи писали
// `use apex_scripting::Scriptable` а не импортировали отдельно
pub use apex_macros::Scriptable;

use apex_core::world::World;

/// Extension trait: регистрирует типы одновременно в World и ScriptEngine.
///
/// Устраняет необходимость двойной регистрации:
/// ```ignore
/// // Было:
/// world.register_component::<Position>();
/// engine.register_component::<Position>(&world);
///
/// // Стало:
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
