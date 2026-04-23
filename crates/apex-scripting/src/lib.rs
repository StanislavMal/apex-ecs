//! apex-scripting — интеграция Rhai-скриптинга с Apex ECS.
//!
//! # Архитектура
//!
//! ```text
//! ScriptEngine
//!   ├── rhai::Engine         — компилятор/исполнитель скриптов
//!   ├── ScriptContext        — мост World ↔ Rhai (Rc<RefCell<>>)
//!   ├── HashMap<name, AST>   — скомпилированные скрипты
//!   └── FileWatcher          — хот-релоад .rhai файлов
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
//! // Регистрация компонентов как Scriptable
//! #[derive(Clone, Copy, Scriptable)]
//! struct Position { x: f32, y: f32 }
//!
//! // Создание движка
//! let mut engine = ScriptEngine::new(Path::new("scripts/"));
//!
//! // Регистрация API: компоненты + глобальные функции
//! engine.register_component::<Position>(&mut world);
//! engine.build_api(&world);
//!
//! // Game loop
//! loop {
//!     engine.poll_hot_reload();
//!     engine.run(dt, &mut world);
//!     world.tick();
//! }
//! ```
//!
//! # Пример скрипта (scripts/game.rhai)
//!
//! ```rhai
//! fn run(ctx) {
//!     let dt = delta_time();
//!
//!     for entity in query(["Read:Position", "Write:Velocity"]) {
//!         entity.velocity.x *= 0.99;
//!         entity.velocity.y *= 0.99;
//!         entity.position.x += entity.velocity.x * dt;
//!         entity.position.y += entity.velocity.y * dt;
//!     }
//!
//!     if entity_count() < 10 {
//!         spawn(#{ position: Position(0.0, 0.0), velocity: Velocity(1.0, 0.5) });
//!     }
//! }
//! ```

pub mod context;
pub mod error;
pub mod field;
pub mod iterators;
pub mod registrar;
pub mod rhai_api;
pub mod script_engine;

pub use context::ScriptContext;
pub use error::ScriptError;
pub use field::ScriptableField;
pub use registrar::ScriptableRegistrar;
pub use script_engine::ScriptEngine;

// Re-export макроса из apex-macros чтобы пользователи писали
// `use apex_scripting::Scriptable` а не импортировали отдельно
pub use apex_macros::Scriptable;