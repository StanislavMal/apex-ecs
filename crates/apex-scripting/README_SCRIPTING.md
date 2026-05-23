# Apex Scripting — Lua Integration

Интеграция [Lua 5.4](https://www.lua.org/manual/5.4/) скриптинга с хот-релоадом в Apex ECS.

Реализована через крейт [`mlua`](https://docs.rs/mlua/) — безопасные Rust-привязки к Lua.

## Общие понятия о макросах систем

Перед началом работы с Lua-скриптингом важно понимать макросы, которыми объявляются
системы в Apex ECS:

- **`system!`** — для параллельных (`AutoSystem`) систем c декларацией доступа
  (`Read<T>`, `Write<T>`). Планировщик автоматически находит неконфликтующие системы
  и выполняет их параллельно. **`ScriptEngine::run()` нельзя вызывать внутри `ParSystem`**,
  так как `run()` требует `&mut self`.

- **`sequential_system!`** — для систем с эксклюзивным `&mut World`.
  Lua-движку нужен полный доступ к миру, поэтому **рекомендуемый способ интеграции
  скриптинга — через `sequential_system!` с состоянием**. Внутри такого макроса
  `ScriptEngine` получает `&mut World` и может свободно выполнять spawn/despawn,
  модифицировать компоненты, читать/писать ресурсы и отправлять события.

Подробнее — в [Руководстве пользователя Apex ECS](https://github.com/StanislavMal/apex-ecs#6-системы-и-планировщик).

## Новые крейты

| Крейт | Назначение |
|---|---|
| `apex-macros` | `#[derive(Scriptable)]` proc-macro |
| `apex-scripting` | `ScriptEngine`, `ScriptContext`, Lua-итераторы |

## Быстрый старт

### 1. Пометить компоненты, ресурсы и события

```rust
use apex_scripting::Scriptable;

// Компоненты
#[derive(Clone, Copy, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy, Scriptable)]
struct Velocity { x: f32, y: f32 }

// Ресурсы (глобальные синглтоны)
#[derive(Clone, Debug, PartialEq, Scriptable)]
struct Gravity { value: f32 }

#[derive(Clone, Debug, PartialEq, Scriptable)]
struct Score { value: i64 }

// События
#[derive(Clone, Debug, PartialEq, Scriptable)]
struct PlayerDied { x: f32, y: f32 }
```

`#[derive(Scriptable)]` генерирует реализацию [`ScriptableRegistrar`](#scriptableregistrar) — трейт
с методами `to_lua()`, `from_lua()`, `register_lua_type()`, `type_name_str()`, `field_names()`.

### 2. Настроить движок

```rust
use apex_scripting::{ScriptEngine, WorldScriptingExt};
use std::path::Path;

// Создаём ScriptEngine
let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));

// Единый вызов — регистрирует и в ECS, и в ScriptEngine:
world.register_scriptable::<Position>(&mut engine);
world.register_scriptable::<Velocity>(&mut engine);

// Ресурсы: сперва вставить в мир, потом зарегистрировать
world.insert_resource(Gravity { value: 9.8 });
world.insert_resource(Score { value: 0 });
world.register_scriptable_resource::<Gravity>(&mut engine);
world.register_scriptable_resource::<Score>(&mut engine);

// События — один вызов:
world.register_scriptable_event::<PlayerDied>(&mut engine);

// Загружаем .lua файлы
engine.load_scripts().expect("ошибка загрузки скриптов");
```

### 3. Интеграция с планировщиком через `sequential_system!` (рекомендуется)

**Рекомендуемый способ** — обернуть `ScriptEngine` в `sequential_system!`
с состоянием. Это даёт полный доступ к `&mut World` и корректно интегрируется
с планировщиком:

```rust
use apex_core::sequential_system;
use apex_scripting::ScriptEngine;

sequential_system! {
    struct LuaRunner {
        engine: ScriptEngine = ScriptEngine::with_dir("scripts/"),
    }

    fn run(
        s: &mut Self,
        world: &mut World,
        dt: &DeltaTime,       // ресурс (опционально, только для примера)
    ) {
        s.engine.poll_hot_reload();
        s.engine.run(dt.0, world);
    }
}
```

Затем зарегистрировать в планировщике:

```rust
use apex_scheduler::{Scheduler, StageLabel};

let mut sched = Scheduler::new();
let system = LuaRunner::default().into_system();
sched.add_system("lua_runner", system);
// или в конкретный этап:
// sched.add_system_to_stage("lua_runner", system, StageLabel::PostUpdate);

sched.compile().unwrap();
```

### 4. Альтернативный способ — ручной вызов `engine.run()` (без Scheduler)

Если вы не используете `Scheduler`:

```rust
loop {
    engine.poll_hot_reload(); // проверяет изменения .lua файлов
    engine.run(dt, &mut world); // выполняет функцию run() из скрипта
    world.tick();
}
```

### 5. Скрипт

```lua
-- scripts/game.lua

function run()
    local dt = delta_time()

    -- Чтение ресурсов
    local gravity = read_resource("Gravity")

    -- Итерация с Read и Write доступом
    for entity in query({"Read:Velocity", "Write:Position"}) do
        entity.position.x = entity.position.x + entity.velocity.x * dt
        entity.position.y = entity.position.y + entity.velocity.y * dt
        commit(entity)
    end

    -- Спавн новых entity
    if entity_count() < 100 then
        spawn_entity({
            position = Position.new(0.0, 0.0),
            velocity = Velocity.new(1.0, 0.5),
        })
    end

    -- Деспавн по условию
    for entity in query({"Read:Health"}) do
        if entity.health.current <= 0.0 then
            despawn(entity.entity)
        end
    end

    -- Запись ресурсов
    write_resource("Score", { value = 100 })

    -- Отправка событий
    emit_event("PlayerDied", { x = 10.0, y = 20.0 })
end
```

## Поддерживаемые типы полей

`#[derive(Scriptable)]` поддерживает следующие типы:

| Rust тип | Lua тип |
|---|---|
| `f32`, `f64` | `number` |
| `i32`, `i64`, `usize` | `integer` |
| `u32`, `u64` | `integer` (потеря для u64 > 2^53) |
| `bool` | `boolean` |
| `String` | `string` |
| `&'static str` | `string` |
| `Vec<T>` | `table` (массив) |
| `HashMap<String, V>` | `table` (хэш-часть) |
| `enum` (C-like) | таблица-неймспейс (`TileKind.Floor = 0`) |
| вложенные структуры | `table` с полями |

### C-like enum

`#[derive(Scriptable)]` для C-like enum автоматически создаёт **Lua-таблицу-неймспейс**:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Scriptable)]
enum TileKind { Floor, Wall, Water }
```

В Lua:

```lua
-- Доступ как к полям таблицы
if entity.tilekind == TileKind.Wall then
    print("Entity is a wall")
end

-- Сравнение через числовые значения тоже работает
-- TileKind.Floor = 0, TileKind.Wall = 1, TileKind.Water = 2
```

### Vec<T> и HashMap<String, V>

```rust
#[derive(Component, Clone, Debug, Scriptable)]
struct Tags {
    list: Vec<String>,
}

#[derive(Component, Clone, Debug, Scriptable)]
struct Stats {
    values: HashMap<String, f32>,
}
```

В Lua:

```lua
-- Vec → Lua массив (1-indexed)
for entity in query({"Read:Tags"}) do
    print(entity.tags.list[1])  -- первый элемент
end

-- HashMap → Lua хэш-таблица
for entity in query({"Read:Stats"}) do
    if entity.stats.values["hp"] > 50.0 then
        print("High HP");
    end
end
```

## Глобальные функции Lua

| Функция | Описание |
|---|---|
| `delta_time() → number` | Delta time текущего кадра |
| `entity_count() → integer` | Число живых entity (кешировано на момент `run()`) |
| `query(descs) → iterator` | Итератор entity с компонентами |
| `commit(entity_table)` | Фиксирует Write-изменения в ECS |
| `spawn_entity(table)` | Создать entity с компонентами (отложено) |
| `despawn(entity_index)` | Уничтожить entity по индексу (отложено) |
| `read_resource(type_name) → value/nil` | Прочитать глобальный ресурс по имени типа |
| `write_resource(type_name, value)` | Записать глобальный ресурс |
| `emit_event(type_name, value)` | Отправить событие |
| `log(msg)` | Вывести `info` в лог движка |
| `print(msg)` | Вывести `info` в лог движка |
| `log_debug(msg)` | Вывести `debug` в лог движка |
| `log_warn(msg)` | Вывести `warn` в лог движка |
| `log_error(msg)` | Вывести `error` в лог движка |
| `inspect(value) → string` | Рекурсивный дамп Lua-значения (отладка) |

### Примеры работы с ресурсами и событиями

```lua
-- Чтение ресурса (возвращает таблицу с полями)
local g = read_resource("Gravity")
log("Gravity = " .. g.value)

-- Запись ресурса (передаётся таблица с полями структуры)
write_resource("Score", { value = 100 })

-- Отправка события (передаётся таблица с полями структуры)
emit_event("PlayerDied", { x = 10.0, y = 20.0 })
```

> **Важно:** Значения для `write_resource` и `emit_event` должны передаваться как Lua-таблицы с ключами, соответствующими именам полей Rust-структуры. Например, для `Score { value: i64 }` — `{ value = 100 }`.

## Форматы query-дескрипторов

```lua
query({"Read:Position"})          -- явный Read
query({"Write:Velocity"})         -- явный Write
query({"Position"})               -- Read по умолчанию
query({"With:Player"})            -- фильтр: только entity с Player
query({"Without:Enemy"})          -- фильтр: только entity без Enemy
query({"Read:Position", "Write:Velocity", "With:Player"})  -- комбинированный
```

## Структура элемента query

Каждый элемент итератора — Lua-таблица со скрытым полем `_meta`:

```lua
for entity in query({"Read:Position", "Write:Velocity"}) do
    entity.entity       -- integer: индекс entity
    entity.position     -- table: { x = float, y = float } (Read-only)
    entity.velocity     -- table: { x = float, y = float } (Write-доступ)
end
```

Ключи в таблице — имена типов в **lowercase** (`"position"`, `"velocity"`).

**Важно:** Для Read-компонентов устанавливается `__newindex` метатаблица,
которая предупреждает при попытке модификации:

```lua
entity.position.x = 5.0  -- предупреждение в логе: "попытка изменить Read-компонент 'Position'"
```

Для Write-компонентов изменения применяются только после вызова `commit(entity)`.

## commit(entity_table)

Функция `commit()` записывает изменения Write-компонентов обратно в ECS:

```lua
for entity in query({"Write:Position", "Write:Velocity"}) do
    entity.position.x = entity.position.x + entity.velocity.x * dt
    entity.position.y = entity.position.y + entity.velocity.y * dt
    commit(entity)  -- фиксирует изменения
end
```

Без `commit()` изменения Write-компонентов **не будут применены**.

### auto_commit

`ScriptEngine::set_auto_commit(true)` включает автоматический вызов `commit()`
при переходе к следующей entity в цикле `for entity in query(...) do ... end`:

```rust
engine.set_auto_commit(true);
```

Когда авто-коммит включён, явный вызов `commit(entity)` не требуется.

## Конструкторы компонентов

`#[derive(Scriptable)]` генерирует Lua-конструктор для каждого компонента:

```lua
-- Position.new(x, y) — создаёт таблицу { x = ..., y = ... }
local pos = Position.new(10.0, 20.0)

-- Health.new(current, max)
local hp = Health.new(100.0, 100.0)

-- Velocity.new(x, y)
local vel = Velocity.new(1.0, 0.5)

-- Tags.new({"enemy", "boss"}) — Vec<String>
local tags = Tags.new({"enemy", "boss"})

-- Stats.new({ hp = 100.0, mp = 50.0 }) — HashMap<String, f32>
local stats = Stats.new({ hp = 100.0, mp = 50.0 })

-- TileKind.Floor — C-like enum (доступ как поле таблицы)
```

Конструкторы доступны внутри скриптов и используются в `spawn_entity()`:

```lua
spawn_entity({
    position = Position.new(0.0, 0.0),
    velocity = Velocity.new(1.0, 0.5),
    health   = Health.new(100.0, 100.0),
    tags     = Tags.new({"enemy", "boss"}),
    stats    = Stats.new({ hp = 100.0, mp = 50.0 }),
    tilekind = TileKind.Floor,
})
```

## Хот-релоад

`ScriptEngine::with_dir(path)` запускает файловый наблюдатель (`notify`).

При изменении `.lua` файла:

1. `poll_hot_reload()` обнаруживает событие
2. Перекомпилирует изменённый файл с новым sandbox-окружением
3. Заменяет `CompiledScript` в `HashMap`
4. Следующий вызов `run()` использует новую версию

### Дебаунс-защита

При сохранении файла ОС генерирует несколько событий. `poll_hot_reload()` использует
time-based debounce (50ms) — если файл был перезагружен менее 50ms назад, событие
пропускается. Это гарантирует ровно одну перекомпиляцию на одно сохранение.

При ошибке компиляции — старый скрипт продолжает работать, ошибка логируется.

## ScriptableRegistrar

Трейт, генерируемый `#[derive(Scriptable)]`. Можно реализовать вручную:

```rust
impl ScriptableRegistrar for Health {
    fn type_name_str() -> &'static str { "Health" }

    fn field_names() -> &'static [&'static str] { &["current", "max"] }

    fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
        let t = lua.create_table()?;
        t.set("current", self.current)?;
        t.set("max", self.max)?;
        Ok(mlua::Value::Table(t))
    }

    fn from_lua(val: &mlua::Value) -> Option<Self> {
        let t = val.as_table()?;
        let current = t.get::<f32>("current").ok()?;
        let max     = t.get::<f32>("max").ok()?;
        Some(Self { current, max })
    }

    fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
        // Регистрирует Health.new(current, max) в Lua
        let t = lua.create_table()?;
        t.set("new", lua.create_function(|lua, (current, max): (f32, f32)| {
            let t = lua.create_table()?;
            t.set("current", current)?;
            t.set("max", max)?;
            Ok(t)
        })?)?;
        lua.globals().set("Health", t)
    }
}
```

## WorldScriptingExt

`WorldScriptingExt` — extension trait, объединяющий регистрацию в `World` и `ScriptEngine`:

```rust
// Было (два вызова):
world.register_component::<Position>();
engine.register_component::<Position>(&world);

// Стало (один вызов):
world.register_scriptable::<Position>(&mut engine);
```

Методы:

| Метод | Действие |
|---|---|
| `world.register_scriptable::<T>(&mut engine)` | Регистрирует компонент `T` в World + ScriptEngine |
| `world.register_scriptable_resource::<T>(&mut engine)` | Регистрирует ресурс `T` в ScriptEngine |
| `world.register_scriptable_event::<T>(&mut engine)` | Регистрирует событие `T` в World + ScriptEngine |

## Sandbox-изоляция

Каждый скрипт выполняется в изолированном [`_ENV`](https://www.lua.org/manual/5.4/manual.html#2.2):

```text
Sandbox _ENV содержит:
├── Стандартные библиотеки: math, string, table, ipairs, pairs, next
├── API-функции: delta_time, entity_count, query, commit, spawn_entity,
│   despawn, read_resource, write_resource, emit_event, log, print,
│   log_debug/log_warn/log_error, inspect
└── Конструкторы компонентов: Position.new, Velocity.new, Health.new и т.д.
```

Стандартные библиотеки `io` и `os` **не включены** — скрипты не имеют доступа к файловой системе.

## Архитектура

```text
ScriptEngine
  ├── mlua::Lua              — Lua 5.4 VM
  ├── ScriptContext          — мост World ↔ Lua (Rc<RefCell<>>)
  │     ├── delta_time: f32
  │     ├── world_ptr: NonNull<World>  — живёт ≤ ScriptEngine::run()
  │     ├── deferred: Commands         — буфер spawn/despawn
  │     ├── deferred_spawns            — отложенные spawn_entity
  │     ├── deferred_resource_writes   — отложенные write_resource
  │     ├── deferred_events            — отложенные emit_event
  │     ├── query_cache                — кэш сборки архетипов
  │     ├── bindings: HashMap<name, ComponentBinding>
  │     ├── resource_bindings: HashMap<name, ResourceBinding>
  │     └── event_bindings: HashMap<name, EventBinding>
  ├── HashMap<name, CompiledScript>    — скомпилированные скрипты
  │     ├── chunk_key: RegistryKey     — main chunk
  │     └── env_key: RegistryKey       — sandbox _ENV
  ├── FileWatcher                      — хот-релоад .lua файлов
  └── spawn_appliers                   — обработчики компонентов для spawn
```

### Жизненный цикл `run()`:

1. Установить `world_ptr` + `delta_time` в `ScriptContext`
2. Выполнить main chunk (определяет `function run()` в sandbox `_ENV`)
3. Вызвать `run()` из sandbox-окружения
4. Применить `deferred` команды (despawn)
5. Применить `deferred_resource_writes` и `deferred_events`
6. Сбросить `world_ptr`
7. Применить отложенные `spawn_entity` (создание entity)

### Двухбуферность изменений

Spawn/despawn из скрипта нельзя применять во время итерации по архетипам:

- **Despawn** — накапливается в `Commands`, применяется после скрипта
- **Spawn** — накапливается в `deferred_spawns`, применяется после скрипта
- **Write-компоненты** — накапливаются через `commit()`, применяются через `flush_writes()`
- **Write-ресурсы и события** — буферизируются в `deferred_resource_writes` / `deferred_events`,
  применяются после скрипта

### Кэширование query-запросов

`ScriptContext` содержит `query_cache: HashMap<Vec<String>, Vec<ArchState>>`:

- Кэширует результат сканирования архетипов при повторных `query()` с теми же дескрипторами
- Инвалидируется при каждом новом запуске скрипта (в `set_world_ptr`)
- Ускоряет частые query из Lua-скриптов в 2-5× за счёт устранения повторного сканирования мира

### Change ticks при записи

При `commit()` автоматически обновляются change ticks. Это означает, что `Changed<T>`
query корректно видит изменения, сделанные из Lua-скриптов.

## Важные замечания

- **НЕ ИСПОЛЬЗУЙТЕ** `ScriptEngine::run()` внутри `ParSystem` / `system!` — `run()` требует
  `&mut self`, что несовместимо с параллельным доступом. Только `SequentialSystem`.
- **Рекомендуется** интеграция через `sequential_system!` с состоянием (см. шаг 3 Быстрого старта).
  Это даёт корректную интеграцию с планировщиком, эксклюзивный `&mut World` и возможность
  использовать параметры макроса (`&Resource`, `&[Event]`, `Cmd`, `Ctx`).
- **МОЖНО** передать `ScriptEngine` в другой поток при внешней синхронизации
  (`Mutex<ScriptEngine>`) — он реализует `Send`.
- Lua выполняется однопоточно внутри одного вызова `run()`. Для параллельной
  обработки данных используйте `ParSystem` и `Commands`.
