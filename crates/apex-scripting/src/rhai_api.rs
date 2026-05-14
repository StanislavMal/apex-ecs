//! Регистрация глобальных Rhai-функций: `delta_time`, `entity_count`,
//! `query`, `spawn`, `despawn`, `read_resource`, `write_resource`, `emit_event`.
//!
//! Все функции захватывают `Arc<Mutex<ScriptContext>>` и работают
//! с миром через него в пределах вызова `ScriptEngine::run()`.
//!
//! # Безопасность и многопоточность
//!
//! Используется `Arc<Mutex<>>` для потокобезопасного доступа к контексту.
//! Это позволяет использовать скрипты в многопоточном окружении.
//!
//! # Паттерн регистрации
//!
//! ```ignore
//! let ctx_clone = Arc::clone(&ctx);
//! engine.register_fn("delta_time", move || -> f64 {
//!     ctx_clone.lock().unwrap().delta_time() as f64
//! });
//! ```

use std::sync::{Arc, Mutex};

use rhai::{Dynamic, Engine};

use crate::{
    context::ScriptContext,
    iterators::{parse_query_descs, RhaiQueryIter},
};

/// Зарегистрировать все глобальные API-функции в Rhai Engine.
///
/// Вызывается один раз при инициализации `ScriptEngine`.
pub fn register_globals(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    register_delta_time(engine, Arc::clone(&ctx));
    register_entity_count(engine, Arc::clone(&ctx));
    register_query(engine, Arc::clone(&ctx));
    register_spawn(engine, Arc::clone(&ctx));
    register_despawn(engine, Arc::clone(&ctx));
    register_resource_api(engine, Arc::clone(&ctx));
    register_event_api(engine, Arc::clone(&ctx));

    // Регистрируем итератор чтобы Rhai знал как итерировать RhaiQueryIter
    // Примечание: register_iterator требует Clone + 'static для типа итератора.
    engine.register_iterator::<RhaiQueryIter>();
}

// ── delta_time() ───────────────────────────────────────────────

fn register_delta_time(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("delta_time", move || -> rhai::FLOAT {
        ctx.lock().unwrap().delta_time() as rhai::FLOAT
    });
}

// ── entity_count() ─────────────────────────────────────────────

fn register_entity_count(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("entity_count", move || -> rhai::INT {
        ctx.lock().unwrap().entity_count() as rhai::INT
    });
}

// ── query(descs) ───────────────────────────────────────────────
//
// Принимает массив строк: query(["Read:Position", "Write:Velocity"])
// Возвращает RhaiQueryIter — итератор по entity с запрошенными компонентами.

fn register_query(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    // Используем register_fn с возвратом Dynamic, оборачивая RhaiQueryIter
    // в Rc<RefCell<>> для удовлетворения требований RhaiNativeFunc.
    engine.register_fn("query", move |descs: rhai::Array| -> Dynamic {
        let parsed = parse_query_descs(&descs);
        let iter = RhaiQueryIter::new(Arc::clone(&ctx), parsed);
        Dynamic::from(iter)
    });
}

// ── spawn(map) ─────────────────────────────────────────────────
//
// Принимает Dynamic Map: spawn(#{ position: Position(0.0, 0.0), ... })
// Ставит в очередь SpawnRequest, применяется после скрипта.

fn register_spawn(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    // Версия с Map компонентов: spawn_entity(#{ position: Position(0.0, 0.0) })
    let ctx_map = Arc::clone(&ctx);
    engine.register_fn("spawn_entity", move |components: rhai::Map| -> Dynamic {
        let request = crate::context::SpawnRequest {
            components: components
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        };
        ctx_map.lock().unwrap().queue_spawn(request);
        Dynamic::UNIT
    });

    // Версия без аргументов: spawn() — создаёт пустую entity
    let ctx_empty = Arc::clone(&ctx);
    engine.register_fn("spawn_empty", move || -> Dynamic {
        let request = crate::context::SpawnRequest { components: Vec::new() };
        ctx_empty.lock().unwrap().queue_spawn(request);
        Dynamic::UNIT
    });
}

// ── despawn(entity_index) ──────────────────────────────────────
//
// Принимает entity index (i64) из Map полученного через query().
// Ставит в очередь despawn, применяется после скрипта.

fn register_despawn(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("despawn", move |entity_idx: rhai::INT| -> Dynamic {
        // Ищем живую entity по index
        let ctx_ref = ctx.lock().unwrap();
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

// ── read_resource(name) / write_resource(name, value) ──────────

/// Зарегистрировать `read_resource(type_name)` и `write_resource(type_name, value)`.
fn register_resource_api(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    let ctx_read = Arc::clone(&ctx);
    engine.register_fn("read_resource", move |type_name: rhai::ImmutableString| -> Dynamic {
        let ctx = ctx_read.lock().unwrap();
        match ctx.read_resource(type_name.as_str()) {
            Some(val) => val,
            None => {
                log::warn!("read_resource: ресурс '{}' не найден", type_name);
                Dynamic::UNIT
            }
        }
    });

    let ctx_write = Arc::clone(&ctx);
    engine.register_fn("write_resource", move |type_name: rhai::ImmutableString, value: Dynamic| {
        let ctx = ctx_write.lock().unwrap();
        ctx.write_resource(type_name.as_str(), &value);
    });
}

// ── emit_event(name, value) ────────────────────────────────────

/// Зарегистрировать `emit_event(type_name, value)`.
fn register_event_api(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("emit_event", move |type_name: rhai::ImmutableString, value: Dynamic| {
        let ctx = ctx.lock().unwrap();
        ctx.emit_event(type_name.as_str(), &value);
    });
}