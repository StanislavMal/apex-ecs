# Миграция apex-scripting: Rhai → mlua (Lua 5.4)

> Обратная совместимость не требуется. Всё переписывается чисто.

---

## Оглавление

1. [Оценка масштаба работы](#1-оценка-масштаба-работы)
2. [Почему mlua лучше Rhai](#2-почему-mlua-лучше-rhai)
3. [Архитектурные решения до начала](#3-архитектурные-решения-до-начала)
4. [Карта изменений по файлам](#4-карта-изменений-по-файлам)
5. [Пошаговый план](#5-пошаговый-план)
6. [Новый синтаксис скриптов](#6-новый-синтаксис-скриптов)
7. [Подводные камни и решения](#7-подводные-камни-и-решения)
8. [Что остаётся без изменений](#8-что-остаётся-без-изменений)

---

## 1. Оценка масштаба работы

### Файлы apex-scripting — что и насколько меняется

| Файл | Строк | Сложность замены | Что делает |
|---|---|---|---|
| `field.rs` | ~350 | **Удаляется полностью** | `ScriptableField` trait + конвертации Rust→Dynamic. В Lua всё становится нативными типами |
| `registrar.rs` | ~100 | **Переписывается** | `ScriptableRegistrar` trait: `to_dynamic/from_dynamic` → `to_lua/from_lua` |
| `context.rs` | ~300 | **Переписывается** | `ScriptContext`: `Arc<Mutex<>>` + `rhai::Dynamic` → `mlua::Lua` instance |
| `rhai_api.rs` | ~200 | **Удаляется, пишется `lua_api.rs`** | `register_fn` в Rhai → `lua.globals().set()` |
| `iterators.rs` | ~250 | **Переписывается** | `RhaiQueryIter` → `LuaQueryIter` или итерация через callback |
| `script_engine.rs` | ~400 | **Переписывается** | `rhai::Engine + AST` → `mlua::Lua` + chunk loading |
| `error.rs` | ~50 | **Минимальные изменения** | Типы ошибок — заменить `rhai::EvalAltResult` на `mlua::Error` |
| `lib.rs` | ~80 | **Минимальные изменения** | Re-export, `WorldScriptingExt` — остаётся почти без изменений |

### apex-macros — `#[derive(Scriptable)]`

Это самое серьёзное изменение. Макрос генерирует код специфичный для Rhai (`rhai::Map`, `rhai::Dynamic`, `engine.register_fn`). Нужно переписать полностью.

| Функция макроса | Rhai | Lua (mlua) |
|---|---|---|
| `to_dynamic` | struct → `rhai::Map` | struct → `mlua::Table` |
| `from_dynamic` | `rhai::Map` → struct | `mlua::Table` → struct |
| `register_rhai_type` | `engine.register_fn("Position", ...)` | Не нужно — Lua создаёт таблицы нативно |

**Итог:** ~400 строк кода изменяется существенно, ~600 строк переписывается. Это **2–3 недели** аккуратной работы, не месяц.

---

## 2. Почему mlua лучше Rhai

### Производительность

| Benchmark | Rhai | Lua 5.4 (mlua) | Разница |
|---|---|---|---|
| Числовые вычисления | ~5M ops/s | ~80M ops/s | **16×** |
| Итерация по таблице | медленно | native JIT-ready | существенно |
| Вызов функции из Rust | overhead через Dynamic | прямой C FFI | ~3× |
| Горячий путь (много entity) | Dynamic allocation на каждый | table reuse | значительно |

Lua 5.4 имеет нативный to-be-closed и register-based VM. Rhai — tree-walking интерпретатор без VM.

### Экосистема

- Lua — 30-летняя история использования в играх (WoW, Factorio, Roblox, Redis)
- `mlua` — зрелый crate, production-ready, активно поддерживается
- Lua-скрипты пишут миллионы разработчиков, Rhai — нишевый язык

### Sandbox

Lua имеет встроенную систему окружений (`_ENV`), позволяющую изолировать скрипты:
```lua
-- Скрипт не видит io, os, require — только то, что разрешено
local env = { query = query, delta_time = delta_time }
local chunk = load(script_code, "script", "t", env)
```

В Rhai sandbox реализован через `Engine::set_*` ограничения, но менее гибко.

---

## 3. Архитектурные решения до начала

Перед кодом нужно принять несколько решений — они влияют на весь последующий код.

### Решение 1: Один Lua state или per-script?

**Вариант A: Один глобальный `mlua::Lua`**
- Все скрипты делят одно состояние
- Проще передавать данные между скриптами
- Риск: скрипты могут загрязнять глобальное состояние друг друга

**Вариант B: Отдельный `Lua` per script file с изолированным `_ENV`**
- Каждый скрипт — своя песочница
- Нет утечки состояния
- Сложнее если нужна коммуникация между скриптами

**Рекомендация: Вариант B** — один `Lua` instance (дорогой), но каждый чанк загружается с изолированным `_ENV`. Это даёт sandboxing без overhead нескольких VM.

```rust
// Один экземпляр Lua на ScriptEngine
let lua = Lua::new();
// Каждый скрипт компилируется как chunk с отдельным env
let chunk = lua.load(code).set_name(name).set_environment(sandbox_env)?;
```

### Решение 2: Как передавать компоненты в Lua?

**Вариант A: Таблицы (как в Rhai — Map)**
```lua
for entity in query({"Read:Position", "Write:Velocity"}) do
    entity.position.x = entity.position.x + entity.velocity.x * dt
end
```

**Вариант B: UserData (proxy объекты)**
```lua
for entity in query({"Position", "Velocity"}) do
    local pos = entity:get_position()
    entity:set_velocity_x(pos.x * 0.1)
end
```

**Рекомендация: Вариант A** — таблицы, ближе к текущему Rhai-API, проще для пользователя. UserData можно добавить позже как оптимизацию.

### Решение 3: Как регистрировать типы?

В Rhai нужен конструктор `Position(x, y)` в движке. В Lua этого не нужно — пользователь пишет `{x=1.0, y=2.0}` или `Position.new(1.0, 2.0)`.

**Рекомендация:** Зарегистрировать таблицу-конструктор через globals:
```lua
-- Автоматически доступно:
local p = Position.new(1.0, 2.0)  -- возвращает таблицу {x=1.0, y=2.0}
```

Это заменяет `register_rhai_type` — метод остаётся в trait, но генерирует другой код.

---

## 4. Карта изменений по файлам

### `apex-macros/src/lib.rs` — `#[derive(Scriptable)]`

**Что удалить:**
- Всю генерацию `to_dynamic` / `from_dynamic` (rhai-специфично)
- `register_rhai_type` — заменить на `register_lua_type`
- Импорты `rhai::*`

**Что написать заново:**

```rust
// Генерируется макросом для struct Position { x: f32, y: f32 }:
impl ScriptableRegistrar for Position {
    fn type_name_str() -> &'static str { "Position" }

    // Rust → Lua table
    fn to_lua<'lua>(&self, lua: &'lua Lua) -> mlua::Result<mlua::Value<'lua>> {
        let t = lua.create_table()?;
        t.set("x", self.x)?;
        t.set("y", self.y)?;
        Ok(mlua::Value::Table(t))
    }

    // Lua table → Rust
    fn from_lua(val: &mlua::Value) -> Option<Self> {
        let t = val.as_table()?;
        Some(Self {
            x: t.get::<f32>("x").ok()?,
            y: t.get::<f32>("y").ok()?,
        })
    }

    // Регистрирует Position.new(x, y) в глобалах Lua
    fn register_lua_type(lua: &Lua) -> mlua::Result<()> {
        let t = lua.create_table()?;
        t.set("new", lua.create_function(|lua, (x, y): (f32, f32)| {
            let t = lua.create_table()?;
            t.set("x", x)?;
            t.set("y", y)?;
            Ok(t)
        })?)?;
        lua.globals().set("Position", t)
    }
}
```

Для **tuple struct** `struct Health(f32)`:
```rust
// Health.new(100.0) → {_value=100.0} или просто скаляр 100.0
fn to_lua(&self, lua: &Lua) -> mlua::Result<mlua::Value> {
    Ok(mlua::Value::Number(self.0 as f64))
}
fn from_lua(val: &mlua::Value) -> Option<Self> {
    Some(Self(val.as_f64()? as f32))  // скалярный путь без таблицы
}
```

Для **C-like enum** `enum TileKind { Floor, Wall }`:
```lua
-- Регистрируется как: TileKind = { Floor = 0, Wall = 1 }
TileKind.Floor  -- число 0
TileKind.Wall   -- число 1
```

### `apex-scripting/src/registrar.rs`

Полностью переписывается. Удаляем `rhai::*`, добавляем `mlua::*`:

```rust
// БЫЛО:
pub trait ScriptableRegistrar: Sized + 'static {
    fn to_dynamic(&self) -> rhai::Dynamic;
    fn from_dynamic(d: &rhai::Dynamic) -> Option<Self>;
    fn register_rhai_type(engine: &mut rhai::Engine);
    fn primitive_info() -> Option<PrimitiveInfo> { None }
}

// СТАЛО:
pub trait ScriptableRegistrar: Sized + 'static {
    fn type_name_str() -> &'static str;
    fn field_names() -> &'static [&'static str];

    /// Конвертировать Rust-значение в mlua::Value (обычно Table)
    fn to_lua<'lua>(&self, lua: &'lua mlua::Lua) -> mlua::Result<mlua::Value<'lua>>;

    /// Восстановить из mlua::Value. None = несовместимый тип.
    fn from_lua(val: &mlua::Value) -> Option<Self>;

    /// Зарегистрировать конструктор типа в глобалах Lua.
    /// Например: Position.new(x, y), TileKind.Floor
    fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()>;
}
```

`ResourceBinding` и `EventBinding` — обновить сигнатуры:

```rust
pub struct ResourceBinding {
    pub name:  &'static str,
    pub read:  for<'lua> fn(&mlua::Lua, &apex_core::World) -> mlua::Result<mlua::Value<'lua>>,
    pub write: fn(&mlua::Value, &mut apex_core::World) -> bool,
}

pub struct EventBinding {
    pub name: &'static str,
    pub emit: fn(&mlua::Value, &mut apex_core::World) -> bool,
}
```

### `apex-scripting/src/field.rs`

**Удаляется целиком.** Больше не нужен.

В Rhai нужен был `ScriptableField` потому что `rhai::Dynamic` — union-тип с ручной конвертацией. В Lua конвертация встроена в mlua через `IntoLua` / `FromLua`. Примитивы конвертируются автоматически:

```rust
// mlua делает это за нас:
table.set("x", self.x)?;              // f32 → lua number
let x: f32 = table.get("x")?;        // lua number → f32
```

`PrimitiveInfo` тоже удаляется — zero-copy path для примитивов реализуется иначе (см. раздел 7).

### `apex-scripting/src/context.rs`

Самое серьёзное изменение. Вся архитектура `Arc<Mutex<ScriptContext>>` — следствие того, что Rhai захватывает замыкания в `register_fn`. В mlua такой проблемы нет — Lua state хранится в `ScriptEngine` напрямую.

```rust
// БЫЛО: ScriptContext в Arc<Mutex<>> — нужен был для захвата в rhai closures
pub struct ScriptContext {
    world_ptr: Option<NonNull<World>>,
    delta_time: f32,
    deferred: Mutex<Commands>,
    deferred_spawns: Mutex<Vec<SpawnRequest>>,
    bindings: HashMap<&'static str, ComponentBinding>,
    resource_bindings: HashMap<&'static str, ResourceBinding>,
    event_bindings: HashMap<&'static str, EventBinding>,
    deferred_resource_writes: Mutex<Vec<(String, Dynamic)>>,
    deferred_events: Mutex<Vec<(String, Dynamic)>>,
    query_cache: RwLock<HashMap<Vec<QueryDesc>, Vec<ArchState>>>,
}

// СТАЛО: ScriptContext упрощается — Lua API регистрируется через AppData
pub struct ScriptContext {
    world_ptr:   Option<NonNull<World>>,
    delta_time:  f32,
    deferred:    Commands,                            // без Mutex — однопоточно
    spawn_queue: Vec<SpawnRequest>,                   // без Mutex
    resource_writes: Vec<(&'static str, mlua::RegistryKey)>, // отложенные записи
    event_queue: Vec<(&'static str, mlua::RegistryKey)>,     // отложенные события
    bindings:         HashMap<&'static str, ComponentBinding>,
    resource_bindings: HashMap<&'static str, ResourceBinding>,
    event_bindings:   HashMap<&'static str, EventBinding>,
}
```

Ключевое упрощение: **Arc<Mutex<>> уходит**. В mlua функции Lua имеют доступ к `AppData` (данные хранящиеся внутри `Lua` instance), поэтому захват `Arc` в замыкания не нужен:

```rust
// Регистрация функции в mlua — нет нужды в Arc<Mutex<>>
lua.globals().set("delta_time", lua.create_function(|lua, ()| {
    let ctx = lua.app_data_ref::<ScriptContext>()
        .ok_or_else(|| mlua::Error::runtime("no context"))?;
    Ok(ctx.delta_time)
})?)?;
```

`ComponentBinding` обновляется:
```rust
pub struct ComponentBinding {
    pub name:  &'static str,
    pub id:    ComponentId,
    // Было: fn(*const u8) -> rhai::Dynamic
    // Стало: возвращает mlua::Value через Lua state
    pub read:  for<'lua> unsafe fn(*const u8, &'lua mlua::Lua) -> mlua::Result<mlua::Value<'lua>>,
    // Было: fn(*mut u8, &rhai::Dynamic) -> bool
    // Стало: принимает mlua::Value
    pub write: unsafe fn(*mut u8, &mlua::Value) -> bool,
}
```

### `apex-scripting/src/rhai_api.rs` → `lua_api.rs`

Файл удаляется, пишется новый `lua_api.rs`. Логика та же, синтаксис другой:

```rust
// БЫЛО: rhai_api.rs
engine.register_fn("delta_time", move || -> rhai::FLOAT {
    ctx.lock().unwrap().delta_time() as rhai::FLOAT
});

// СТАЛО: lua_api.rs
lua.globals().set("delta_time", lua.create_function(|lua, ()| {
    let ctx = lua.app_data_ref::<ScriptContext>()
        .expect("ScriptContext not set");
    Ok(ctx.delta_time as f64)
})?)?;
```

Полный список функций для переноса:

| Rhai функция | Lua функция | Изменения |
|---|---|---|
| `delta_time()` | `delta_time()` | Только синтаксис |
| `entity_count()` | `entity_count()` | Только синтаксис |
| `query(["Read:Pos"])` | `query({"Read:Pos"})` | `[]` → `{}`, синтаксис Lua array |
| `spawn_entity(#{pos: ...})` | `spawn_entity({pos=...})` | `#{}` → `{}` |
| `spawn_empty()` | `spawn_entity({})` | Убрать отдельную функцию |
| `despawn(idx)` | `despawn(idx)` | Только синтаксис |
| `read_resource("Gravity")` | `read_resource("Gravity")` | Без изменений |
| `write_resource("Score", #{...})` | `write_resource("Score", {...})` | `#{}` → `{}` |
| `emit_event("PlayerDied", #{...})` | `emit_event("PlayerDied", {...})` | `#{}` → `{}` |
| `log(msg)` | `log(msg)` | Без изменений |

### `apex-scripting/src/iterators.rs`

`RhaiQueryIter` — Rhai требует `Iterator<Item = rhai::Dynamic>` + регистрацию через `register_iterator`. В Lua итерация реализуется через callback или через Lua-итераторы (pairs/ipairs-подобные).

**Вариант: iterator factory** — самый Lua-идиоматичный:

```lua
-- Lua скрипт:
for entity in query({"Read:Position", "Write:Velocity"}) do
    entity.velocity.x = entity.velocity.x + entity.position.x * dt
    commit(entity)  -- записать изменения обратно
end
```

В Rust `query()` возвращает Lua-функцию-итератор:

```rust
lua.globals().set("query", lua.create_function(|lua, descs: mlua::Table| {
    // Строим список (arch_idx, row, components) заранее
    let items = build_query_items(lua, &descs)?;
    let items = Rc::new(RefCell::new(items.into_iter()));
    
    // Возвращаем Lua-функцию-итератор
    lua.create_function(move |lua, ()| {
        match items.borrow_mut().next() {
            Some(item) => Ok(mlua::Value::Table(item_to_table(lua, item)?)),
            None       => Ok(mlua::Value::Nil),
        }
    })
})?)?;
```

`flush_writes` реализуется через `commit(entity)`:
```rust
lua.globals().set("commit", lua.create_function(|lua, entity_table: mlua::Table| {
    // Читаем изменения из таблицы, записываем в Column
    flush_entity_writes(lua, &entity_table)?;
    Ok(())
})?)?;
```

**Альтернатива без `commit`** — использовать `__newindex` метаметод на entity-таблице, чтобы изменения записывались сразу. Это более прозрачно для пользователя, но сложнее в реализации.

### `apex-scripting/src/script_engine.rs`

Заменить `rhai::Engine + AST` на `mlua::Lua + chunk`:

```rust
// БЫЛО:
struct CompiledScript { ast: rhai::AST, path: PathBuf }
pub struct ScriptEngine {
    engine:  rhai::Engine,
    ctx:     Arc<Mutex<ScriptContext>>,
    scripts: HashMap<String, CompiledScript>,
    ...
}

// СТАЛО:
struct CompiledScript {
    // Lua хранит скомпилированный chunk как RegistryKey
    chunk_key: mlua::RegistryKey,
    path:      PathBuf,
}

pub struct ScriptEngine {
    lua:     mlua::Lua,               // единственный VM
    ctx:     ScriptContext,           // без Arc<Mutex<>> !
    scripts: HashMap<String, CompiledScript>,
    active_script: String,
    script_dir: Option<PathBuf>,
    // hot-reload остаётся тот же — notify::Watcher
    watcher:  Option<Box<dyn Watcher>>,
    watch_rx: Option<mpsc::Receiver<notify::Result<Event>>>,
    last_reload: HashMap<String, Instant>,
    spawn_appliers: HashMap<String, SpawnApplierFn>,
    spawn_queue: Vec<SpawnRequest>,
}
```

`run()` упрощается:

```rust
pub fn run(&mut self, dt: f32, world: &mut World) {
    // 1. Устанавливаем контекст через AppData
    self.ctx.delta_time = dt;
    unsafe { self.ctx.set_world_ptr(world); }
    self.lua.set_app_data(self.ctx.clone_shallow());  // или через unsafe ptr
    
    // 2. Выполняем chunk
    if let Some(script) = self.scripts.get(&self.active_script) {
        let chunk: mlua::Function = self.lua.registry_value(&script.chunk_key)?;
        if let Err(e) = chunk.call::<()>(()) {
            log::error!("[script] {}: {}", self.active_script, e);
        }
    }
    
    // 3. Применяем deferred изменения
    self.ctx.clear_world_ptr();
    self.ctx.deferred.apply(world);
    self.apply_spawn_queue(world);
}
```

`load_scripts()` — заменить `.rhai` на `.lua`:

```rust
// Компиляция в chunk:
let chunk_key = {
    let chunk = self.lua.load(&code)
        .set_name(&name)
        .set_environment(self.make_sandbox_env()?)?;
    self.lua.create_registry_value(chunk.into_function()?)?
};
```

`poll_hot_reload()` — изменить только фильтр расширения файлов (`.rhai` → `.lua`):
```rust
// Было:
p.extension().and_then(|e| e.to_str()) == Some("rhai")
// Стало:
p.extension().and_then(|e| e.to_str()) == Some("lua")
```

---

## 5. Пошаговый план

### Шаг 1 — Подготовка (день 1)

- [ ] Добавить `mlua` в `apex-scripting/Cargo.toml`:
  ```toml
  mlua = { version = "0.10", features = ["lua54", "vendored", "send"] }
  ```
  Убрать `rhai = { ... }`.
- [ ] Добавить `mlua` в `apex-macros/Cargo.toml` (нужен для генерации кода)
- [ ] Создать ветку `feat/lua-scripting`

**Важно о фичах mlua:**
- `lua54` — Lua 5.4 VM (максимальная производительность)
- `vendored` — компилирует Lua из исходников, нет зависимости от системной библиотеки
- `send` — делает `Lua: Send` (нужно для интеграции с движком)

### Шаг 2 — Новый trait ScriptableRegistrar (день 1–2)

Переписать `registrar.rs`. Это фундамент — остальное строится на нём.

- [ ] Удалить `rhai::*` импорты
- [ ] Заменить `to_dynamic/from_dynamic` на `to_lua/from_lua`
- [ ] Заменить `register_rhai_type` на `register_lua_type`
- [ ] Обновить `ResourceBinding` и `EventBinding`
- [ ] Написать unit-тест: вручную реализовать `ScriptableRegistrar` для `Health { current: f32, max: f32 }` и проверить round-trip

### Шаг 3 — Новый derive макрос (день 2–4)

Переписать `expand_scriptable` в `apex-macros`.

- [ ] `expand_named_struct` → генерирует `to_lua/from_lua/register_lua_type`
- [ ] `expand_tuple_struct` → скалярный путь (не таблица)
- [ ] `expand_c_like_enum` → таблица-namespace `TileKind.Floor = 0`
- [ ] Удалить `ScriptableField` trait (был только для Rhai)
- [ ] Обновить тест в `apex-examples`

### Шаг 4 — ScriptContext (день 4–5)

- [ ] Удалить `Arc<Mutex<>>` — контекст хранится в `ScriptEngine` напрямую
- [ ] Удалить `deferred_resource_writes: Mutex<Vec<(String, Dynamic)>>` — заменить на `Vec<(&'static str, mlua::RegistryKey)>`
- [ ] Заменить `ComponentBinding.read/write` — новые сигнатуры с `mlua::Lua`
- [ ] Убрать `query_cache: RwLock<HashMap<Vec<QueryDesc>, Vec<ArchState>>>` — кеш перенести в `ScriptEngine` (он теперь `&mut self`)
- [ ] Убрать `deferred_spawns: Mutex<Vec<SpawnRequest>>` → просто `Vec<SpawnRequest>`

### Шаг 5 — lua_api.rs (день 5–6)

Написать `lua_api.rs` с нуля. Зарегистрировать все глобальные функции.

- [ ] `delta_time()` → `lua.create_function`
- [ ] `entity_count()` → `lua.create_function`
- [ ] `query({...})` → factory возвращающий Lua-итератор
- [ ] `commit(entity)` → записывает изменения из таблицы обратно в ECS
- [ ] `spawn_entity({...})` → добавляет в `SpawnRequest`
- [ ] `despawn(idx)` → добавляет в `Commands`
- [ ] `read_resource("name")` → возвращает Lua таблицу
- [ ] `write_resource("name", {...})` → откладывает в очередь
- [ ] `emit_event("name", {...})` → откладывает в очередь
- [ ] `log(msg)` → `log::info!`

### Шаг 6 — iterators.rs (день 6–7)

Переписать логику итерации.

- [ ] Удалить `RhaiQueryIter`
- [ ] `parse_query_descs` — обновить под `mlua::Table` вместо `rhai::Array`
- [ ] `build_arch_states` — без изменений (работает с ECS, не с скриптовым API)
- [ ] `build_item` — заменить `rhai::Map` на `mlua::Table`
- [ ] `flush_writes` → переименовать в `commit_entity`, принимает `mlua::Table`
- [ ] Реализовать Lua-итератор через closure

### Шаг 7 — ScriptEngine (день 7–9)

- [ ] Заменить `rhai::Engine` на `mlua::Lua`
- [ ] `CompiledScript.ast: rhai::AST` → `chunk_key: mlua::RegistryKey`
- [ ] `load_scripts()` — сканировать `.lua` вместо `.rhai`
- [ ] `load_script_str()` — `lua.load(code)` вместо `engine.compile(code)`
- [ ] `run()` — упростить, убрать `Arc<Mutex<>>`
- [ ] `register_component()` — обновить под новые типы
- [ ] `register_resource()` — обновить
- [ ] `register_event()` — обновить
- [ ] `poll_hot_reload()` — только изменить фильтр расширения файлов
- [ ] `apply_spawn_queue()` — без изменений в логике

### Шаг 8 — error.rs (день 9)

- [ ] Удалить `ScriptError::compile(name, rhai::ParseError)` → `ScriptError::compile(name, mlua::Error)`
- [ ] Удалить `ScriptError::runtime(name, Box<rhai::EvalAltResult>)` → `ScriptError::runtime(name, mlua::Error)`
- [ ] Остальные варианты (`Io`, `NotFound`, `NoScriptDir`) — без изменений

### Шаг 9 — lib.rs и WorldScriptingExt (день 9–10)

- [ ] Удалить `pub use registrar::ScriptableRegistrar` (тот же re-export, новый trait)
- [ ] Удалить `pub use field::ScriptableField` — трейта больше нет
- [ ] `WorldScriptingExt` — без изменений в логике, только типы внутри

### Шаг 10 — Тесты и примеры (день 10–14)

- [ ] Обновить `apex-examples/examples/scripting.rs`
- [ ] Обновить `apex-examples/examples/hot_reload_test.rs`
- [ ] Написать тест round-trip: компонент → Lua table → компонент
- [ ] Написать тест query итерации с Write
- [ ] Написать тест hot-reload `.lua` файла
- [ ] Написать тест spawn из скрипта
- [ ] Benchmark: сравнить скорость итерации 10k entity на Rhai vs Lua

---

## 6. Новый синтаксис скриптов

### Было (Rhai)

```rhai
fn run() {
    let dt = delta_time();
    let gravity = read_resource("Gravity");

    for entity in query(["Read:Velocity", "Write:Position"]) {
        entity.velocity.y -= gravity.value * dt;
        entity.position.x += entity.velocity.x * dt;
        entity.position.y += entity.velocity.y * dt;
    }

    if entity_count() < 100 {
        spawn_entity(#{
            position: Position(0.0, 0.0),
            velocity: Velocity(1.0, 0.5),
        });
    }

    for entity in query(["Read:Health"]) {
        if entity.health.current <= 0.0 {
            despawn(entity.entity);
        }
    }

    write_resource("Score", #{ value: 100 });
    emit_event("PlayerDied", #{ x: 10.0, y: 20.0 });
}
```

### Стало (Lua)

```lua
function run()
    local dt = delta_time()
    local gravity = read_resource("Gravity")

    for entity in query({"Read:Velocity", "Write:Position"}) do
        entity.velocity.y = entity.velocity.y - gravity.value * dt
        entity.position.x = entity.position.x + entity.velocity.x * dt
        entity.position.y = entity.position.y + entity.velocity.y * dt
        commit(entity)  -- записать Write-изменения обратно в ECS
    end

    if entity_count() < 100 then
        spawn_entity({
            position = Position.new(0.0, 0.0),
            velocity = Velocity.new(1.0, 0.5),
        })
    end

    for entity in query({"Read:Health"}) do
        if entity.health.current <= 0.0 then
            despawn(entity.entity)
        end
    end

    write_resource("Score", { value = 100 })
    emit_event("PlayerDied", { x = 10.0, y = 20.0 })
end
```

### Ключевые различия синтаксиса

| Rhai | Lua | Примечание |
|---|---|---|
| `#{ key: val }` | `{ key = val }` | Map литерал |
| `["a", "b"]` | `{"a", "b"}` | Array литерал |
| `for x in iter {}` | `for x in iter() do ... end` | for-in |
| `if cond {}` | `if cond then ... end` | блоки |
| `Position(x, y)` | `Position.new(x, y)` | конструктор |
| `TileKind_Floor()` | `TileKind.Floor` | enum константа |
| `let x = 1;` | `local x = 1` | объявление |
| нет | `commit(entity)` | **новое** — запись Write |

---

## 7. Подводные камни и решения

### Проблема 1: `mlua::Lua` не `Send` без feature `send`

**Ситуация:** `ScriptEngine` хранит `mlua::Lua`. Если `Lua` не `Send` — `ScriptEngine` нельзя передать в другой поток (что и было в Rhai с `Arc<Mutex<>>`).

**Решение:** Включить feature `send`:
```toml
mlua = { version = "0.10", features = ["lua54", "vendored", "send"] }
```
С этой фичей `Lua: Send`. Но это означает что Lua 5.4 будет скомпилирован с дополнительными проверками. Небольшой overhead, но приемлемый.

### Проблема 2: Lifetime в `to_lua<'lua>`

**Ситуация:** `mlua::Value<'lua>` привязан к lifetime `Lua`. Это значит что нельзя хранить `Value` в структурах без `Lua` рядом.

**Решение:** Для отложенных операций (write_resource, emit_event) вместо `mlua::Value` хранить `mlua::RegistryKey` — это owned handle без lifetime:

```rust
// Плохо — Value привязан к lifetime:
deferred_resource_writes: Vec<(&'static str, mlua::Value<'lua>)>,

// Хорошо — RegistryKey owned, без lifetime:
deferred_resource_writes: Vec<(&'static str, mlua::RegistryKey)>,

// Применение:
let val: mlua::Value = lua.registry_value(&key)?;
(binding.write)(&val, world);
lua.remove_registry_value(key);
```

### Проблема 3: `commit(entity)` — неочевидно для пользователя

**Ситуация:** В Rhai изменения сбрасывались через `Drop` итератора автоматически. В Lua нет деструкторов.

**Решение A (explicit commit):** Пользователь вызывает `commit(entity)` явно. Понятно, но требует дисциплины.

**Решение B (auto commit):** После цикла `query` Lua-итератор возвращает `nil` — в этот момент Rust-код автоматически применяет все pending writes из внутреннего буфера.

**Рекомендация: Решение B** — прозрачнее. Реализация: `query()` возвращает closure с внутренним состоянием (Vec pending writes). Когда closure возвращает `nil`, сразу применяет накопленные write-back операции.

### Проблема 4: Zero-copy path для примитивов

В Rhai была `PrimitiveInfo` с `read_raw/write_raw`. В mlua примитивы конвертируются автоматически через `IntoLua/FromLua`. Overhead на создание `mlua::Value::Number` из `f32` — это allocation?

**Ответ:** Нет. `mlua::Value::Number(f64)` — это `Copy` тип без аллокации. `mlua::Table` — это ссылка на объект в Lua heap, что дороже. Поэтому для tuple struct (одно поле) лучше использовать скаляр, не таблицу:

```rust
// Health(f32) → Lua number, не таблица
fn to_lua(&self, _lua: &Lua) -> mlua::Result<mlua::Value> {
    Ok(mlua::Value::Number(self.0 as f64))
}
```

### Проблема 5: Sandbox — скрипты не должны делать `require`, `io.open`

**Решение:** Загружать chunk с изолированным `_ENV`:

```rust
fn make_sandbox_env(lua: &Lua) -> mlua::Result<mlua::Table> {
    let env = lua.create_table()?;
    // Разрешаем только безопасные функции
    for name in &["math", "string", "table", "ipairs", "pairs", "next",
                  "select", "tonumber", "tostring", "type", "unpack"] {
        if let Ok(val) = lua.globals().get::<mlua::Value>(name) {
            env.set(*name, val)?;
        }
    }
    // Наш API
    for name in &["delta_time", "entity_count", "query", "commit",
                  "spawn_entity", "despawn", "read_resource", "write_resource",
                  "emit_event", "log"] {
        if let Ok(val) = lua.globals().get::<mlua::Value>(name) {
            env.set(*name, val)?;
        }
    }
    Ok(env)
}
```

---

## 8. Что остаётся без изменений

Это важно — следующие части **не трогаем**:

| Компонент | Почему не меняется |
|---|---|
| `ScriptContext::world_ptr` паттерн | Safety архитектура правильная, только убираем Mutex |
| `ScriptContext::set_world_ptr/clear_world_ptr` | Lifetime management остаётся |
| `ScriptContext::apply_deferred()` | Commands::apply — не зависит от Rhai |
| `SpawnRequest` структура | Просто `Vec<(String, Value)>` — заменяем Dynamic на RegistryKey |
| `apply_spawn_queue()` логика | Поиск по type_name, вызов spawn_appliers |
| `spawn_appliers` HashMap | Та же архитектура function pointers |
| `FileWatcher` в hot-reload | Полностью независим от языка скриптов |
| `poll_hot_reload()` логика | Только фильтр расширения `.rhai` → `.lua` |
| `WorldScriptingExt` trait | Публичный API без изменений |
| `debounce` 50ms логика | Без изменений |
| `apex-hot-reload` крейт | Полностью независим |
| `apex-core`, `apex-scheduler` | Не знают о скриптинге |

---

## Итоговая оценка

**Масштаб:** средний рефакторинг, не переписывание движка.

**Риски:** низкие — архитектура ScriptEngine хорошо изолирована от остального ECS. Самый рискованный момент — корректность `unsafe` в `ComponentBinding.read/write` при новых сигнатурах.

**Рекомендуемый порядок:** строго по шагам 1→10. Шаг 3 (макрос) — самый технически сложный, делать его на свежую голову.

**Итого:** ~2–3 недели плотной работы одного разработчика.
