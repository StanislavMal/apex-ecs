//! Регистрация глобальных Rhai-функций: `delta_time`, `entity_count`,
//! `query`, `spawn`, `despawn`.
//!
//! Все функции захватывают `Rc<RefCell<ScriptContext>>` и работают
//! с миром через него в пределах вызова `ScriptEngine::run()`.
//!
//! # Безопасность и однопоточность
//!
//! Используется `Rc<RefCell<>>` (не `Arc<Mutex<>>`), потому что Rhai без фичи
//! `"sync"` — однопоточный. Это исключает случайное использование из другого потока.
//!
//! # Паттерн регистрации
//!
//! ```ignore
//! let ctx_clone = Rc::clone(&ctx);
//! engine.register_fn("delta_time", move || -> f64 {
//!     ctx_clone.borrow().delta_time() as f64
//! });
//! ```

use std::{cell::RefCell, rc::Rc};

use rhai::{Dynamic, Engine, EvalAltResult};

use crate::{
    context::ScriptContext,
    iterators::{parse_query_descs, RhaiQueryIter},
};

/// Зарегистрировать все глобальные API-функции в Rhai Engine.
///
/// Вызывается один раз при инициализации `ScriptEngine`.
pub fn register_globals(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    register_delta_time(engine, Rc::clone(&ctx));
    register_entity_count(engine, Rc::clone(&ctx));
    register_query(engine, Rc::clone(&ctx));
    register_spawn(engine, Rc::clone(&ctx));
    register_despawn(engine, Rc::clone(&ctx));

    // Регистрируем итератор чтобы Rhai знал как итерировать RhaiQueryIter
    engine.register_iterator::<RhaiQueryIter>();
}

// ── delta_time() ───────────────────────────────────────────────

fn register_delta_time(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("delta_time", move || -> rhai::FLOAT {
        ctx.borrow().delta_time() as rhai::FLOAT
    });
}

// ── entity_count() ─────────────────────────────────────────────

fn register_entity_count(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("entity_count", move || -> rhai::INT {
        ctx.borrow().entity_count() as rhai::INT
    });
}

// ── query(descs) ───────────────────────────────────────────────
//
// Принимает массив строк: query(["Read:Position", "Write:Velocity"])
// Возвращает RhaiQueryIter — итератор по entity с запрошенными компонентами.

fn register_query(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("query", move |descs: rhai::Array| -> RhaiQueryIter {
        let parsed = parse_query_descs(&descs);
        RhaiQueryIter::new(Rc::clone(&ctx), parsed)
    });
}

// ── spawn(map) ─────────────────────────────────────────────────
//
// Принимает Dynamic Map: spawn(#{ position: Position(0.0, 0.0), ... })
// Ставит в очередь SpawnRequest, применяется после скрипта.

fn register_spawn(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    // Версия с Map компонентов: spawn(#{ position: Position(0.0, 0.0) })
    let ctx_map = Rc::clone(&ctx);
    engine.register_fn("spawn", move |components: rhai::Map| -> Dynamic {
        let request = crate::context::SpawnRequest {
            components: components
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        };
        ctx_map.borrow().queue_spawn(request);
        Dynamic::UNIT
    });

    // Версия без аргументов: spawn() — создаёт пустую entity
    let ctx_empty = Rc::clone(&ctx);
    engine.register_fn("spawn_empty", move || -> Dynamic {
        let request = crate::context::SpawnRequest { components: Vec::new() };
        ctx_empty.borrow().queue_spawn(request);
        Dynamic::UNIT
    });
}

// ── despawn(entity_index) ──────────────────────────────────────
//
// Принимает entity index (i64) из Map полученного через query().
// Ставит в очередь despawn, применяется после скрипта.

fn register_despawn(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("despawn", move |entity_idx: rhai::INT| -> Dynamic {
        // Ищем живую entity по index
        let ctx_ref = ctx.borrow();
        let world   = ctx_ref.world_ref();
        if let Some(entity) = world.entity_allocator().get_by_index(entity_idx as u32) {
            ctx_ref.queue_despawn(entity);
        } else {
            log::warn!("despawn: entity index {} не найден или уже мёртв", entity_idx);
        }
        Dynamic::UNIT
    });
}

// ── log() ──────────────────────────────────────────────────────

/// Зарегистрировать `log(message)` — вывод в лог движка.
pub fn register_log(engine: &mut Engine) {
    engine.register_fn("log", |msg: rhai::ImmutableString| {
        log::info!("[script] {}", msg);
    });
    engine.on_print(|msg| log::info!("[script] {}", msg));
    engine.on_debug(|msg, src, pos| {
        log::debug!("[script] {}:{} — {}", src.unwrap_or("?"), pos, msg);
    });
}