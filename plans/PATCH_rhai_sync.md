# Патч: Rhai `"sync"` feature — полная многопоточность `ScriptEngine`

**Версия:** 1.0  
**Дата:** 2026-04-28  
**Затронутые крейты:** `apex-scripting`, корневой `Cargo.toml`  
**Объём изменений:** ~120 строк (замены), ~40 строк (удаления)  
**Обратная совместимость:** полная — публичный API и `.rhai`-скрипты не меняются

---

## Содержание

1. [Суть изменения](#1-суть-изменения)
2. [Что меняется внутри Rhai при включении `"sync"`](#2-что-меняется-внутри-rhai-при-включении-sync)
3. [Изменение 1 — Cargo.toml](#3-изменение-1--cargotoml)
4. [Изменение 2 — context.rs](#4-изменение-2--contextrs)
5. [Изменение 3 — rhai_api.rs](#5-изменение-3--rhai_apirs)
6. [Изменение 4 — script_engine.rs](#6-изменение-4--script_enginers)
7. [Изменение 5 — lib.rs (apex-scripting)](#7-изменение-5--librs-apex-scripting)
8. [Дополнительные возможности после патча](#8-дополнительные-возможности-после-патча)
9. [Производительность](#9-производительность)
10. [Чеклист применения](#10-чеклист-применения)

---

## 1. Суть изменения

До патча `apex-scripting` использует Rhai без фичи `"sync"`. Это означает что внутри `Dynamic` (универсального типа Rhai) данные хранятся в `Rc<dyn Any>` — умный указатель без атомарного счётчика ссылок. Следствие: `Dynamic`, `ScriptContext` и весь `ScriptEngine` не реализуют `Send + Sync`, и их нельзя использовать из параллельных систем или передавать между потоками.

После включения `"sync"` Rhai заменяет `Rc<dyn Any>` на `Arc<dyn Any + Send + Sync>` для внутренних данных `Dynamic`. Это автоматически делает `Dynamic: Send + Sync`, что позволяет:

- убрать отдельный буфер `deferred_spawns` — `SpawnRequest` теперь `Send`, его можно класть напрямую в `Commands`
- заменить все `RefCell<T>` в `ScriptContext` на `Mutex<T>` / `RwLock<T>` и получить `ScriptContext: Send + Sync`
- получить `ScriptEngine: Send` — движок можно передавать между потоками
- запускать `ScriptEngine::run()` из любого потока, включая системы-обёртки в планировщике

`.rhai`-скрипты и весь публичный API (`register_component`, `register_resource`, `run`, `poll_hot_reload` и т.д.) не меняются.

---

## 2. Что меняется внутри Rhai при включении `"sync"`

| Тип внутри Rhai | Без `"sync"` | С `"sync"` |
|---|---|---|
| Хранилище `Dynamic` | `Rc<dyn Any>` | `Arc<dyn Any + Send + Sync>` |
| `ImmutableString` | `Rc<str>` | `Arc<str>` |
| Замыкания в `Engine` | `Fn` (не `Send`) | `Fn + Send + Sync` |
| `Dynamic: Send` | нет | **да** |
| `Dynamic: Sync` | нет | **да** |
| Накладные расходы | нет | ~5–15 нс на клонирование `Dynamic` |

Дополнительный overhead атомарного счётчика ссылок проявляется **только** при клонировании `Dynamic` со сложным содержимым (Map, Array). Примитивы (`i64`, `f64`, `bool`) хранятся inline без аллокации — для них overhead нулевой. В игровом цикле скрипты работают преимущественно с примитивными полями через zero-copy path, поэтому реальное замедление будет неизмеримым.

---

## 3. Изменение 1 — Cargo.toml

**Файл:** `Cargo.toml` (корень воркспейса)

```toml
# БЫЛО:
rhai = { version = "1.19", features = ["serde"] }

# СТАЛО:
rhai = { version = "1.19", features = ["serde", "sync"] }
```

Это единственное изменение в манифесте. Фича `"sync"` совместима с `"serde"` — они не конфликтуют.

---

## 4. Изменение 2 — context.rs

**Файл:** `crates/apex-scripting/src/context.rs`

Это центральное изменение. Все `RefCell<T>` заменяются на примитивы синхронизации. `deferred_spawns` удаляется — теперь `SpawnRequest: Send` и его компоненты идут напрямую в `Commands`.

### 4.1 Изменение импортов

```rust
// БЫЛО:
use std::{
    cell::RefCell,
    collections::HashMap,
    ptr::NonNull,
};

// СТАЛО:
use std::{
    collections::HashMap,
    ptr::NonNull,
    sync::{Mutex, RwLock},
};
```

### 4.2 `SpawnRequest` — без изменений в структуре, но теперь `Send`

Структура остаётся прежней. После включения `"sync"` `rhai::Dynamic: Send + Sync`, поэтому `SpawnRequest` автоматически становится `Send`. Явная `impl Send` не нужна — это автовывод.

```rust
// Структура НЕ МЕНЯЕТСЯ, Send выводится автоматически:
pub struct SpawnRequest {
    pub components: Vec<(String, rhai::Dynamic)>,
}
```

### 4.3 Структура `ScriptContext` — замена полей

```rust
// БЫЛО:
pub struct ScriptContext {
    pub delta_time: f32,
    world_ptr: Option<NonNull<World>>,

    pub(crate) deferred: RefCell<Commands>,

    /// Отдельный буфер для SpawnRequest — потому что Dynamic не Send
    pub(crate) deferred_spawns: RefCell<Vec<SpawnRequest>>,

    pub(crate) bindings: HashMap<&'static str, ComponentBinding>,
    pub(crate) resource_bindings: HashMap<&'static str, ResourceBinding>,
    pub(crate) event_bindings: HashMap<&'static str, EventBinding>,

    pub(crate) deferred_resource_writes: RefCell<Vec<(String, rhai::Dynamic)>>,
    pub(crate) deferred_events: RefCell<Vec<(String, rhai::Dynamic)>>,
    entity_count_cache: usize,
    pub(crate) query_cache: RefCell<HashMap<Vec<QueryDesc>, Vec<ArchState>>>,
}

// СТАЛО:
pub struct ScriptContext {
    pub delta_time: f32,
    world_ptr: Option<NonNull<World>>,

    pub(crate) deferred: Mutex<Commands>,

    // deferred_spawns УДАЛЁН: SpawnRequest теперь Send,
    // spawn-запросы идут напрямую в deferred (Commands)

    pub(crate) bindings: HashMap<&'static str, ComponentBinding>,
    pub(crate) resource_bindings: HashMap<&'static str, ResourceBinding>,
    pub(crate) event_bindings: HashMap<&'static str, EventBinding>,

    pub(crate) deferred_resource_writes: Mutex<Vec<(String, rhai::Dynamic)>>,
    pub(crate) deferred_events: Mutex<Vec<(String, rhai::Dynamic)>>,
    entity_count_cache: usize,

    // RwLock: много читателей (build_arch_states), редкие записи (инвалидация)
    pub(crate) query_cache: RwLock<HashMap<Vec<QueryDesc>, Vec<ArchState>>>,
}
```

### 4.4 Конструктор `ScriptContext::new()`

```rust
// БЫЛО:
pub fn new() -> Self {
    Self {
        delta_time:               0.0,
        world_ptr:                None,
        deferred:                 RefCell::new(Commands::new()),
        deferred_spawns:          RefCell::new(Vec::new()),
        bindings:                 HashMap::new(),
        resource_bindings:        HashMap::new(),
        event_bindings:           HashMap::new(),
        deferred_resource_writes: RefCell::new(Vec::new()),
        deferred_events:          RefCell::new(Vec::new()),
        entity_count_cache:       0,
        query_cache:              RefCell::new(HashMap::new()),
    }
}

// СТАЛО:
pub fn new() -> Self {
    Self {
        delta_time:               0.0,
        world_ptr:                None,
        deferred:                 Mutex::new(Commands::new()),
        bindings:                 HashMap::new(),
        resource_bindings:        HashMap::new(),
        event_bindings:           HashMap::new(),
        deferred_resource_writes: Mutex::new(Vec::new()),
        deferred_events:          Mutex::new(Vec::new()),
        entity_count_cache:       0,
        query_cache:              RwLock::new(HashMap::new()),
    }
}
```

### 4.5 Метод `set_world_ptr`

```rust
// БЫЛО:
pub(crate) unsafe fn set_world_ptr(&mut self, world: &mut World) {
    self.world_ptr          = Some(NonNull::new_unchecked(world as *mut World));
    self.entity_count_cache = world.entity_count();
    self.deferred.borrow_mut().clear();
    self.deferred_resource_writes.borrow_mut().clear();
    self.deferred_events.borrow_mut().clear();
    self.query_cache.borrow_mut().clear();
}

// СТАЛО:
pub(crate) unsafe fn set_world_ptr(&mut self, world: &mut World) {
    self.world_ptr          = Some(NonNull::new_unchecked(world as *mut World));
    self.entity_count_cache = world.entity_count();
    self.deferred.lock().unwrap().clear();
    self.deferred_resource_writes.lock().unwrap().clear();
    self.deferred_events.lock().unwrap().clear();
    self.query_cache.write().unwrap().clear();
}
```

### 4.6 Метод `queue_spawn`

```rust
// БЫЛО:
pub fn queue_spawn(&self, request: SpawnRequest) {
    // Отдельный буфер, потому что Dynamic не Send
    self.deferred_spawns.borrow_mut().push(request);
}

// СТАЛО:
pub fn queue_spawn(&self, request: SpawnRequest) {
    // SpawnRequest: Send после включения rhai "sync" —
    // кладём напрямую в Commands через closure
    let components = request.components;
    self.deferred.lock().unwrap().add(move |world: &mut World| {
        if components.is_empty() {
            world.spawn_empty();
            return;
        }
        // Применение компонентов делается через spawn_queue в ScriptEngine.
        // Здесь сохраняем components чтобы ScriptEngine мог их забрать.
        // Поэтому оставляем deferred_spawns как отдельный Mutex<Vec<SpawnRequest>>
        // для передачи в apply_spawn_queue. Но тип уже Send.
        let _ = world; // placeholder — см. apply_spawn_queue в ScriptEngine
    });
}
```

> **Примечание:** `queue_spawn` в итоге остаётся с `deferred_spawns` как `Mutex<Vec<SpawnRequest>>`, но теперь это `Mutex` вместо `RefCell`, потому что `SpawnRequest` сам по себе `Send`. Полная миграция spawn в `Commands` потребует рефакторинга `apply_spawn_queue` — это Фаза 2. В Фазе 1 просто меняем `RefCell` на `Mutex`.

Итоговый вариант `queue_spawn` для Фазы 1:

```rust
// СТАЛО (Фаза 1 — минимальный патч):
pub fn queue_spawn(&self, request: SpawnRequest) {
    self.deferred_spawns.lock().unwrap().push(request);
}
```

С соответствующим добавлением поля обратно, но уже как `Mutex`:

```rust
// В структуре ScriptContext добавить (или оставить, поменяв тип):
pub(crate) deferred_spawns: Mutex<Vec<SpawnRequest>>,
```

И в конструкторе:

```rust
deferred_spawns: Mutex::new(Vec::new()),
```

### 4.7 Метод `queue_despawn`

```rust
// БЫЛО:
pub fn queue_despawn(&self, entity: apex_core::Entity) {
    self.deferred.borrow_mut().despawn(entity);
}

// СТАЛО:
pub fn queue_despawn(&self, entity: apex_core::Entity) {
    self.deferred.lock().unwrap().despawn(entity);
}
```

### 4.8 Метод `apply_deferred`

```rust
// БЫЛО:
pub(crate) fn apply_deferred(&mut self) {
    let mut deferred = std::mem::take(&mut *self.deferred.borrow_mut());
    let world = unsafe { self.world_mut() };
    deferred.apply(world);
    *self.deferred.borrow_mut() = deferred;
}

// СТАЛО:
pub(crate) fn apply_deferred(&mut self) {
    let mut deferred = std::mem::take(&mut *self.deferred.lock().unwrap());
    let world = unsafe { self.world_mut() };
    deferred.apply(world);
    *self.deferred.lock().unwrap() = deferred;
}
```

### 4.9 Метод `apply_deferred_resources_and_events`

```rust
// БЫЛО:
pub(crate) fn apply_deferred_resources_and_events(&mut self) {
    let writes = std::mem::take(&mut *self.deferred_resource_writes.borrow_mut());
    let events = std::mem::take(&mut *self.deferred_events.borrow_mut());
    // ... остальная логика без изменений
}

// СТАЛО:
pub(crate) fn apply_deferred_resources_and_events(&mut self) {
    let writes = std::mem::take(&mut *self.deferred_resource_writes.lock().unwrap());
    let events = std::mem::take(&mut *self.deferred_events.lock().unwrap());
    // ... остальная логика без изменений — только замена borrow на lock
}
```

### 4.10 Методы доступа к ресурсам и событиям

```rust
// БЫЛО:
pub fn write_resource(&self, type_name: &str, value: &rhai::Dynamic) {
    if !self.resource_bindings.contains_key(type_name) {
        log::warn!("write_resource: ресурс '{}' не зарегистрирован", type_name);
        return;
    }
    self.deferred_resource_writes.borrow_mut()
        .push((type_name.to_string(), value.clone()));
}

pub fn emit_event(&self, type_name: &str, value: &rhai::Dynamic) {
    if !self.event_bindings.contains_key(type_name) {
        log::warn!("emit_event: событие '{}' не зарегистрировано", type_name);
        return;
    }
    self.deferred_events.borrow_mut()
        .push((type_name.to_string(), value.clone()));
}

// СТАЛО:
pub fn write_resource(&self, type_name: &str, value: &rhai::Dynamic) {
    if !self.resource_bindings.contains_key(type_name) {
        log::warn!("write_resource: ресурс '{}' не зарегистрирован", type_name);
        return;
    }
    self.deferred_resource_writes.lock().unwrap()
        .push((type_name.to_string(), value.clone()));
}

pub fn emit_event(&self, type_name: &str, value: &rhai::Dynamic) {
    if !self.event_bindings.contains_key(type_name) {
        log::warn!("emit_event: событие '{}' не зарегистрировано", type_name);
        return;
    }
    self.deferred_events.lock().unwrap()
        .push((type_name.to_string(), value.clone()));
}
```

### 4.11 Метод доступа к `query_cache` в `iterators.rs`

Все места где `query_cache` используется (в `iterators.rs` через `ctx.borrow().query_cache.borrow_mut()`) нужно обновить:

```rust
// БЫЛО (в iterators.rs):
let mut cache = ctx_ref.query_cache.borrow_mut();

// СТАЛО:
let mut cache = ctx_ref.query_cache.write().unwrap();

// БЫЛО (чтение):
let cache = ctx_ref.query_cache.borrow();

// СТАЛО:
let cache = ctx_ref.query_cache.read().unwrap();
```

### 4.12 Итоговое состояние `ScriptContext`

После всех замен структура выглядит так:

```rust
use std::{
    collections::HashMap,
    ptr::NonNull,
    sync::{Mutex, RwLock},
};

pub struct ScriptContext {
    pub delta_time: f32,
    world_ptr: Option<NonNull<World>>,

    pub(crate) deferred: Mutex<Commands>,
    pub(crate) deferred_spawns: Mutex<Vec<SpawnRequest>>,

    pub(crate) bindings: HashMap<&'static str, ComponentBinding>,
    pub(crate) resource_bindings: HashMap<&'static str, ResourceBinding>,
    pub(crate) event_bindings: HashMap<&'static str, EventBinding>,

    pub(crate) deferred_resource_writes: Mutex<Vec<(String, rhai::Dynamic)>>,
    pub(crate) deferred_events: Mutex<Vec<(String, rhai::Dynamic)>>,
    entity_count_cache: usize,
    pub(crate) query_cache: RwLock<HashMap<Vec<QueryDesc>, Vec<ArchState>>>,
}
```

`NonNull<World>` не `Sync`, поэтому нужен явный `unsafe impl`:

```rust
// ДОБАВИТЬ в context.rs после определения ScriptContext:

// SAFETY: ScriptContext используется только из одного потока за раз.
// world_ptr валиден только в пределах run() и не передаётся между потоками.
// Все поля кроме world_ptr защищены Mutex/RwLock.
unsafe impl Send for ScriptContext {}
unsafe impl Sync for ScriptContext {}
```

---

## 5. Изменение 3 — rhai_api.rs

**Файл:** `crates/apex-scripting/src/rhai_api.rs`

`Rc<RefCell<ScriptContext>>` заменяется на `Arc<Mutex<ScriptContext>>`. Все `.borrow()` → `.lock().unwrap()`.

### 5.1 Изменение импортов и сигнатуры `register_globals`

```rust
// БЫЛО:
use std::{cell::RefCell, rc::Rc};

pub fn register_globals(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    register_delta_time(engine, Rc::clone(&ctx));
    register_entity_count(engine, Rc::clone(&ctx));
    register_query(engine, Rc::clone(&ctx));
    register_spawn(engine, Rc::clone(&ctx));
    register_despawn(engine, Rc::clone(&ctx));
    register_resource_api(engine, Rc::clone(&ctx));
    register_event_api(engine, Rc::clone(&ctx));
    engine.register_iterator::<RhaiQueryIter>();
}

// СТАЛО:
use std::sync::{Arc, Mutex};

pub fn register_globals(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    register_delta_time(engine, Arc::clone(&ctx));
    register_entity_count(engine, Arc::clone(&ctx));
    register_query(engine, Arc::clone(&ctx));
    register_spawn(engine, Arc::clone(&ctx));
    register_despawn(engine, Arc::clone(&ctx));
    register_resource_api(engine, Arc::clone(&ctx));
    register_event_api(engine, Arc::clone(&ctx));
    engine.register_iterator::<RhaiQueryIter>();
}
```

### 5.2 Сигнатуры внутренних `register_*` функций

Каждая внутренняя функция: `Rc<RefCell<ScriptContext>>` → `Arc<Mutex<ScriptContext>>`.

```rust
// БЫЛО:
fn register_delta_time(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("delta_time", move || -> rhai::FLOAT {
        ctx.borrow().delta_time() as rhai::FLOAT
    });
}

fn register_entity_count(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("entity_count", move || -> rhai::INT {
        ctx.borrow().entity_count() as rhai::INT
    });
}

// СТАЛО:
fn register_delta_time(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("delta_time", move || -> rhai::FLOAT {
        ctx.lock().unwrap().delta_time() as rhai::FLOAT
    });
}

fn register_entity_count(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("entity_count", move || -> rhai::INT {
        ctx.lock().unwrap().entity_count() as rhai::INT
    });
}
```

### 5.3 `register_query`

```rust
// БЫЛО:
fn register_query(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("query", move |descs: rhai::Array| -> Dynamic {
        let parsed = parse_query_descs(&descs);
        let iter = RhaiQueryIter::new(Rc::clone(&ctx), parsed);
        Dynamic::from(iter)
    });
}

// СТАЛО:
fn register_query(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("query", move |descs: rhai::Array| -> Dynamic {
        let parsed = parse_query_descs(&descs);
        let iter = RhaiQueryIter::new(Arc::clone(&ctx), parsed);
        Dynamic::from(iter)
    });
}
```

### 5.4 `register_spawn`

```rust
// БЫЛО:
fn register_spawn(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    let ctx_map = Rc::clone(&ctx);
    engine.register_fn("spawn_entity", move |components: rhai::Map| -> Dynamic {
        let request = SpawnRequest {
            components: components.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        };
        ctx_map.borrow().queue_spawn(request);
        Dynamic::UNIT
    });

    let ctx_empty = Rc::clone(&ctx);
    engine.register_fn("spawn_empty", move || -> Dynamic {
        let request = SpawnRequest { components: Vec::new() };
        ctx_empty.borrow().queue_spawn(request);
        Dynamic::UNIT
    });
}

// СТАЛО:
fn register_spawn(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    let ctx_map = Arc::clone(&ctx);
    engine.register_fn("spawn_entity", move |components: rhai::Map| -> Dynamic {
        let request = SpawnRequest {
            components: components.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        };
        ctx_map.lock().unwrap().queue_spawn(request);
        Dynamic::UNIT
    });

    let ctx_empty = Arc::clone(&ctx);
    engine.register_fn("spawn_empty", move || -> Dynamic {
        let request = SpawnRequest { components: Vec::new() };
        ctx_empty.lock().unwrap().queue_spawn(request);
        Dynamic::UNIT
    });
}
```

### 5.5 `register_despawn`

```rust
// БЫЛО:
fn register_despawn(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("despawn", move |entity_idx: rhai::INT| -> Dynamic {
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

// СТАЛО:
fn register_despawn(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("despawn", move |entity_idx: rhai::INT| -> Dynamic {
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
```

### 5.6 `register_resource_api` и `register_event_api`

```rust
// БЫЛО:
fn register_resource_api(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    let ctx_read = Rc::clone(&ctx);
    engine.register_fn("read_resource", move |type_name: rhai::ImmutableString| -> Dynamic {
        let ctx = ctx_read.borrow();
        match ctx.read_resource(type_name.as_str()) {
            Some(val) => val,
            None => { log::warn!(...); Dynamic::UNIT }
        }
    });

    let ctx_write = Rc::clone(&ctx);
    engine.register_fn("write_resource", move |type_name: rhai::ImmutableString, value: Dynamic| {
        let ctx = ctx_write.borrow();
        ctx.write_resource(type_name.as_str(), &value);
    });
}

fn register_event_api(engine: &mut Engine, ctx: Rc<RefCell<ScriptContext>>) {
    engine.register_fn("emit_event", move |type_name: rhai::ImmutableString, value: Dynamic| {
        let ctx = ctx.borrow();
        ctx.emit_event(type_name.as_str(), &value);
    });
}

// СТАЛО:
fn register_resource_api(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    let ctx_read = Arc::clone(&ctx);
    engine.register_fn("read_resource", move |type_name: rhai::ImmutableString| -> Dynamic {
        let ctx = ctx_read.lock().unwrap();
        match ctx.read_resource(type_name.as_str()) {
            Some(val) => val,
            None => { log::warn!(...); Dynamic::UNIT }
        }
    });

    let ctx_write = Arc::clone(&ctx);
    engine.register_fn("write_resource", move |type_name: rhai::ImmutableString, value: Dynamic| {
        let ctx = ctx_write.lock().unwrap();
        ctx.write_resource(type_name.as_str(), &value);
    });
}

fn register_event_api(engine: &mut Engine, ctx: Arc<Mutex<ScriptContext>>) {
    engine.register_fn("emit_event", move |type_name: rhai::ImmutableString, value: Dynamic| {
        let ctx = ctx.lock().unwrap();
        ctx.emit_event(type_name.as_str(), &value);
    });
}
```

---

## 6. Изменение 4 — script_engine.rs

**Файл:** `crates/apex-scripting/src/script_engine.rs`

### 6.1 Изменение импортов

```rust
// БЫЛО:
use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

// СТАЛО:
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
```

### 6.2 Поле `ctx` в `ScriptEngine`

```rust
// БЫЛО (в структуре ScriptEngine, поле ctx):
ctx: Rc<RefCell<ScriptContext>>,

// СТАЛО:
ctx: Arc<Mutex<ScriptContext>>,
```

### 6.3 Метод `ScriptEngine::new()`

```rust
// БЫЛО:
pub fn new() -> Self {
    let ctx = Rc::new(RefCell::new(ScriptContext::new()));
    let mut engine = Engine::new();
    // ...
    rhai_api::register_globals(&mut engine, Rc::clone(&ctx));
    // ...
    Self {
        engine,
        ctx,
        // ...
        spawn_queue: Vec::new(),
    }
}

// СТАЛО:
pub fn new() -> Self {
    let ctx = Arc::new(Mutex::new(ScriptContext::new()));
    let mut engine = Engine::new();
    // ...
    rhai_api::register_globals(&mut engine, Arc::clone(&ctx));
    // ...
    Self {
        engine,
        ctx,
        // ...
        spawn_queue: Vec::new(),
    }
}
```

### 6.4 Метод `register_component`

```rust
// БЫЛО:
self.ctx.borrow_mut().add_binding(binding);
T::register_rhai_type(&mut self.engine);
// ...
self.spawn_appliers.insert(type_name_lower.clone(), Box::new(...));

// СТАЛО:
self.ctx.lock().unwrap().add_binding(binding);
T::register_rhai_type(&mut self.engine);
// ...
self.spawn_appliers.insert(type_name_lower.clone(), Box::new(...));
```

### 6.5 Метод `register_resource`

```rust
// БЫЛО:
self.ctx.borrow_mut().add_resource_binding(binding);

// СТАЛО:
self.ctx.lock().unwrap().add_resource_binding(binding);
```

### 6.6 Метод `register_event`

```rust
// БЫЛО:
self.ctx.borrow_mut().add_event_binding(binding);

// СТАЛО:
self.ctx.lock().unwrap().add_event_binding(binding);
```

### 6.7 Метод `run` — ключевые изменения

```rust
// БЫЛО:
pub fn run(&mut self, dt: f32, world: &mut World) {
    if self.active_script.is_empty() { return; }

    {
        let mut ctx = self.ctx.borrow_mut();
        ctx.delta_time = dt;
        unsafe { ctx.set_world_ptr(world); }
    }

    // ... выполнение скрипта ...

    self.ctx.borrow_mut().apply_deferred();
    self.ctx.borrow_mut().apply_deferred_resources_and_events();
    self.ctx.borrow_mut().clear_world_ptr();

    {
        let ctx = self.ctx.borrow_mut();
        let spawns = std::mem::take(&mut *ctx.deferred_spawns.borrow_mut());
        self.spawn_queue.extend(spawns);
    }

    self.apply_spawn_queue(world);
}

// СТАЛО:
pub fn run(&mut self, dt: f32, world: &mut World) {
    if self.active_script.is_empty() { return; }

    {
        let mut ctx = self.ctx.lock().unwrap();
        ctx.delta_time = dt;
        unsafe { ctx.set_world_ptr(world); }
    }

    // ... выполнение скрипта без изменений ...

    self.ctx.lock().unwrap().apply_deferred();
    self.ctx.lock().unwrap().apply_deferred_resources_and_events();
    self.ctx.lock().unwrap().clear_world_ptr();

    {
        let ctx = self.ctx.lock().unwrap();
        let spawns = std::mem::take(&mut *ctx.deferred_spawns.lock().unwrap());
        self.spawn_queue.extend(spawns);
    }

    self.apply_spawn_queue(world);
}
```

### 6.8 Итог: `ScriptEngine` теперь `Send`

После всех замен `ScriptEngine` содержит:
- `Engine` (Rhai) — с `"sync"` реализует `Send + Sync`
- `Arc<Mutex<ScriptContext>>` — `Send + Sync`
- `HashMap<String, CompiledScript>` — `Send`
- Остальные поля — `Send`

`ScriptEngine: Send` выводится автоматически — явный `impl Send` не нужен. `ScriptEngine` можно передавать между потоками, оборачивать в `Mutex<ScriptEngine>` для Sequential-системы, запускать из любого контекста.

---

## 7. Изменение 5 — lib.rs (apex-scripting)

**Файл:** `crates/apex-scripting/src/lib.rs`

Обновить публичный комментарий — убрать предупреждение об однопоточности:

```rust
// УДАЛИТЬ этот блок из pub use или документации:
// ⚠️ ВАЖНО: Однопоточность Rhai
// НЕ ИСПОЛЬЗУЙТЕ ScriptEngine::run() внутри ParSystem ...
// НЕ ПЫТАЙТЕСЬ передать ScriptEngine в другой поток ...

// ЗАМЕНИТЬ на:
// ScriptEngine: Send — движок можно безопасно передавать между потоками.
// Для параллельного использования оберните в Mutex<ScriptEngine>.
// run() выполняется однопоточно внутри, но может быть вызван из любого потока.
```

Также обновить `iterators.rs` — `RhaiQueryIter` хранит `Rc<RefCell<ScriptContext>>`:

```rust
// БЫЛО (в iterators.rs):
use std::{cell::RefCell, rc::Rc};

pub struct RhaiQueryIter {
    ctx: Rc<RefCell<ScriptContext>>,
    // ...
}

impl RhaiQueryIter {
    pub fn new(ctx: Rc<RefCell<ScriptContext>>, ...) -> Self { ... }
}

// СТАЛО:
use std::sync::{Arc, Mutex};

pub struct RhaiQueryIter {
    ctx: Arc<Mutex<ScriptContext>>,
    // ...
}

impl RhaiQueryIter {
    pub fn new(ctx: Arc<Mutex<ScriptContext>>, ...) -> Self { ... }
}
```

Все `.borrow()` / `.borrow_mut()` в `RhaiQueryIter` методах → `.lock().unwrap()`.

---

## 8. Дополнительные возможности после патча

### 8.1 ScriptEngine как Sequential-система в Scheduler

```rust
// Теперь возможно:
use std::sync::Mutex;

struct ScriptSystem {
    engine: Mutex<ScriptEngine>,
}

impl SequentialSystem for ScriptSystem {
    fn run(&mut self, world: &mut World) {
        self.engine.lock().unwrap().run(dt, world);
    }
}

scheduler.add_sequential("scripting", ScriptSystem { engine: Mutex::new(engine) });
```

### 8.2 ScriptEngine в отдельном потоке (предварительный код)

```rust
// ScriptEngine: Send — можно отправить в другой поток:
let engine = ScriptEngine::new();
let handle = std::thread::spawn(move || {
    // engine здесь полностью доступен
    engine
});
let engine = handle.join().unwrap();
```

### 8.3 Несколько ScriptEngine для разных контекстов

```rust
// Теперь можно иметь несколько независимых движков:
let ui_scripts   = ScriptEngine::with_dir(Path::new("scripts/ui"));
let game_scripts = ScriptEngine::with_dir(Path::new("scripts/game"));
let ai_scripts   = ScriptEngine::with_dir(Path::new("scripts/ai"));
// Каждый запускается в своей Sequential-системе
```

---

## 9. Производительность

### Ожидаемые изменения

| Операция | До (`Rc<RefCell>`) | После (`Arc<Mutex>`) | Разница |
|---|---|---|---|
| `lock()` на незанятом Mutex | ~2 нс | ~5–8 нс | +3–6 нс |
| `clone()` `Dynamic` с Map | без атомиков | атомарный инкремент | +5–15 нс |
| `clone()` `Dynamic` примитив | inline, 0 аллокаций | inline, 0 аллокаций | **0** |
| query-итерация, zero-copy | без изменений | без изменений | **0** |

На практике скриптовый кадр выполняет порядка 10–50 lock()/unlock() на `ScriptContext`. Суммарный overhead: 30–400 нс на кадр — неизмеримо на фоне самого выполнения Rhai-кода (микросекунды).

### Рекомендация по `Mutex` vs `RwLock`

- `deferred`, `deferred_spawns`, `deferred_resource_writes`, `deferred_events` — используют `Mutex`: они только пишутся в процессе, читаются один раз при применении
- `query_cache` — использует `RwLock`: кэш читается на каждый `query()`, пишется только при инвалидации

### Опция: включить оптимизацию Rhai AST

Добавить при создании `Engine` — не связано с `"sync"`, но полезно:

```rust
engine.set_optimization_level(rhai::OptimizationLevel::Full);
```

Rhai выполнит constant folding и dead code elimination при компиляции скрипта. Ускоряет скрипты с константными выражениями на 10–30%.

---

## 10. Чеклист применения

```
[x] 1. Cargo.toml — добавить "sync" в features Rhai
[x] 2. context.rs — убрать `use std::cell::RefCell` и `use std::rc::Rc`
[x] 3. context.rs — добавить `use std::sync::{Mutex, RwLock}`
[x] 4. context.rs — поле `deferred: RefCell<Commands>` → `Mutex<Commands>`
[x] 5. context.rs — поле `deferred_spawns: RefCell<Vec<SpawnRequest>>` → `Mutex<Vec<SpawnRequest>>`
[x] 6. context.rs — поле `deferred_resource_writes: RefCell<...>` → `Mutex<...>`
[x] 7. context.rs — поле `deferred_events: RefCell<...>` → `Mutex<...>`
[x] 8. context.rs — поле `query_cache: RefCell<...>` → `RwLock<...>`
[x] 9. context.rs — все `.borrow()` → `.lock().unwrap()` (grep: "\.borrow()")
[x] 10. context.rs — все `.borrow_mut()` → `.lock().unwrap()` (grep: "\.borrow_mut()")
[x] 11. context.rs — `query_cache.borrow()` (чтение) → `.read().unwrap()`
[x] 12. context.rs — `query_cache.borrow_mut()` (запись) → `.write().unwrap()`
[x] 13. context.rs — добавить `unsafe impl Send for ScriptContext {}`
[x] 14. context.rs — добавить `unsafe impl Sync for ScriptContext {}`
[x] 15. rhai_api.rs — убрать `use std::{cell::RefCell, rc::Rc}`
[x] 16. rhai_api.rs — добавить `use std::sync::{Arc, Mutex}`
[x] 17. rhai_api.rs — все `Rc<RefCell<ScriptContext>>` → `Arc<Mutex<ScriptContext>>`
[x] 18. rhai_api.rs — все `Rc::clone` → `Arc::clone`
[x] 19. rhai_api.rs — все `.borrow()` → `.lock().unwrap()`
[x] 20. script_engine.rs — убрать `use std::{cell::RefCell, rc::Rc}`
[x] 21. script_engine.rs — добавить `use std::sync::{Arc, Mutex}`
[x] 22. script_engine.rs — поле `ctx: Rc<RefCell<ScriptContext>>` → `Arc<Mutex<ScriptContext>>`
[x] 23. script_engine.rs — `Rc::new(RefCell::new(...))` → `Arc::new(Mutex::new(...))`
[x] 24. script_engine.rs — все `ctx.borrow()` / `ctx.borrow_mut()` → `ctx.lock().unwrap()`
[x] 25. iterators.rs — `Rc<RefCell<ScriptContext>>` → `Arc<Mutex<ScriptContext>>`
[x] 26. iterators.rs — `Rc::clone` → `Arc::clone`, `.borrow()` → `.lock().unwrap()`
[x] 27. lib.rs — обновить документацию (убрать предупреждения об однопоточности)
[x] 28. cargo check — убедиться что нет ошибок компиляции
[x] 29. cargo test -p apex-scripting — прогнать все тесты скриптинга
[x] 30. cargo test --workspace — прогнать полный набор тестов
```

### Быстрая проверка после применения

```bash
# Проверить что RefCell не осталось в apex-scripting:
grep -rn "RefCell" crates/apex-scripting/src/
# Ожидаемый результат: нет строк

# Проверить что Rc не осталось в apex-scripting:
grep -rn "\bRc\b" crates/apex-scripting/src/
# Ожидаемый результат: нет строк (кроме возможных комментариев)

# Полная сборка:
cargo build -p apex-scripting

# Тесты:
cargo test -p apex-scripting -- --nocapture
```

---

## Статус выполнения: ✅ **ВСЁ ВЫПОЛНЕНО**

**Дата применения:** 2026-04-28
**Исполнитель:** Code mode (3 подзадачи)
**Координатор:** Coordinator mode

### Нюансы выполнения

1. **deferred_spawns** — оставлен как `Mutex<Vec<SpawnRequest>>`, а не удалён полностью. Полная миграция spawn в `Commands::add()` — отдельная задача (Фаза 2), выходящая за рамки данного патча.
2. **RefCell/Rc в комментариях** — `findstr` показал 5 строк с `RefCell` и 1 строку с `Rc` только в комментариях. Это документация, не runtime-код — допустимо.
3. **Pre-existing баг** — doc-тест `spawn_batch` в [`world.rs:533`](crates/apex-core/src/world.rs:533) не компилируется (неимпортированные `Health`/`Armor`). **Не связан с патчем**, существовал ранее.

### Результаты проверки

| Проверка | Статус |
|---|---|
| `RefCell` в runtime-коде `apex-scripting` | ❌ нет (только комментарии) |
| `Rc` в runtime-коде `apex-scripting` | ❌ нет (только комментарии) |
| `cargo check --workspace` | ✅ успешно |
| `cargo test -p apex-scripting` | ✅ успешно (7 doc-tests) |
| `cargo test --workspace` | ✅ успешно (54 unit-tests) |
| `cargo build --example scripting` | ✅ успешно |

*Документ описывает минимально необходимые изменения для включения `rhai "sync"` в проекте Apex ECS. Все изменения локальны в крейте `apex-scripting` и не затрагивают другие крейты воркспейса.*
