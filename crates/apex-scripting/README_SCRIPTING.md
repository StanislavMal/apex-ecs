# Apex Scripting — Lua Integration

Интеграция [Lua 5.4](https://www.lua.org/manual/5.4/) скриптинга с хот-релоадом в ApexForge_ECS.

Реализована через крейт [`mlua`](https://docs.rs/mlua/) — безопасные Rust-привязки к Lua.

> Полное руководство — раздел **§17 «Lua Scripting»** в `Apex_ECS_Руководство_пользователя.md`
> (в частности §17.9 — API `system{}`). Архитектурное решение (почему Lua-системы — main-thread
> `NonSend`-вид, а не `Arc<Mutex<VM>>`) — `decisions/ADR-005-script-systems-architecture.md`.

## Две модели запуска

Одна Lua-VM однопоточна (`ScriptEngine` привязан к потоку создания — **`!Send`**, держит `Rc<Lua>`).
Есть две модели интеграции:

- **Lua-системы `system{}`** (золотой путь) — отдельные скрипт-функции регистрируются как
  **первоклассные системы планировщика** через `engine.register_systems(&mut scheduler)`. Несмотря на
  однопоточность VM, они исполняются как main-thread `NonSend`-системы **конкурентно с непересекающимися
  Rust-системами**, с конфликт-детекцией по декларациям доступа, детерминированными спавнами и hot-reload.
- **Монолитный `run()`** — весь скрипт исполняется как одна эксклюзивная операция вне планировщика
  (`engine.run(dt, &mut world)`). Проще, но без параллелизма и без scheduler-декларированного доступа.

`ScriptEngine::run()` требует `&mut World`, поэтому его нельзя вызвать изнутри параллельной системы —
для интеграции с планировщиком используйте `system{}` (`register_systems`).

## Крейты

| Крейт | Назначение |
|---|---|
| `apex-macros` | `#[derive(Scriptable)]` proc-macro |
| `apex-scripting` | `ScriptEngine`, `ScriptContext`, Lua-итераторы; зависит от `apex-core` и `apex-scheduler` |

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

// Создаём ScriptEngine с каталогом скриптов (включает файловый watcher)
let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));

// Единый вызов — регистрирует и в ECS, и в ScriptEngine:
world.register_scriptable::<Position>(&mut engine);
world.register_scriptable::<Velocity>(&mut engine);

// Ресурсы: сперва вставить в мир, потом зарегистрировать в движке
world.insert_resource(Gravity { value: 9.8 });
world.insert_resource(Score { value: 0 });
world.register_scriptable_resource::<Gravity>(&mut engine);
world.register_scriptable_resource::<Score>(&mut engine);

// События — один вызов:
world.register_scriptable_event::<PlayerDied>(&mut engine);

// Загружаем .lua файлы
engine.load_scripts().expect("ошибка загрузки скриптов");
```

> **Порядок важен:** регистрируйте компоненты ДО `register_systems` — система, ссылающаяся на
> незарегистрированный компонент, будет отвергнута (иначе недо-декларация доступа = гонка).

### 3. Lua-системы `system{}` + планировщик (золотой путь)

Скрипт объявляет системы на верхнем уровне; `register_systems` транслирует их в системы планировщика.

```rust
use apex_scheduler::Scheduler;

let mut scheduler = Scheduler::new();
scheduler.set_deterministic_spawn(true);      // детерминированные id спавнов (опционально)
engine.register_systems(&mut scheduler);      // system{} → NonSend-системы планировщика

loop {
    engine.poll_hot_reload();                 // при изменении .lua-файла
    engine.register_systems(&mut scheduler);  // идемпотентно: заменяет прошлое поколение систем
    engine.set_delta_time(dt);                // dt, видимый всеми Lua-системами этого кадра
    scheduler.run(&mut world);                // Lua-системы бегут вместе с Rust-системами
    world.tick();
}
```

`system{}`-скрипт (`scripts/gameplay.lua`):

```lua
system{
    name  = "integrate",
    query = {"Read:Velocity", "Write:Position"},
    fn = function()
        for e in query({"Read:Velocity", "Write:Position"}) do
            e.position.x = e.position.x + e.velocity.x * delta_time()
            e.position.y = e.position.y + e.velocity.y * delta_time()
            commit(e)
        end
    end,
}

system{
    name  = "spawner",
    query = {"Write:Position"},
    fn = function()
        if entity_count() < 100 then
            spawn_entity({ position = Position.new(0.0, 0.0) })  -- детерминированный id
        end
    end,
}
```

**Модель исполнения.** Каждая Lua-система — main-thread `NonSend`-система. Все они декларируют скрытую
запись на маркер-токен `ScriptVm`, поэтому планировщик **сериализует Lua↔Lua** (одна VM), но
**параллелит Lua↔Rust** по реальным декларациям доступа. Внутри `fn` вызовы `query()`/`commit()`
резолвят **только** объявленные в `query` компоненты — обращение к необъявленному даёт громкую
аномалию (§0.2a) и пустой результат. Полный набор эффектов (`commit`, `spawn_entity`, `despawn`,
`write_resource`, `emit_event`) применяется через per-system `Commands`-слот планировщика после стадии.

### 4. Альтернатива — монолитный `run()` (без планировщика)

Если планировщик не используется, весь скрипт исполняется как одна операция. Скрипт определяет
функцию `run()`:

```rust
loop {
    engine.poll_hot_reload();     // проверяет изменения .lua файлов
    engine.run(dt, &mut world);   // выполняет функцию run() активного скрипта
    world.tick();
}
```

```lua
-- scripts/game.lua
function run()
    local dt = delta_time()
    local gravity = read_resource("Gravity")

    for entity in query({"Read:Velocity", "Write:Position"}) do
        entity.position.x = entity.position.x + entity.velocity.x * dt
        commit(entity)
    end

    if entity_count() < 100 then
        spawn_entity({ position = Position.new(0.0, 0.0), velocity = Velocity.new(1.0, 0.5) })
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

> Не смешивайте модели в одном скрипте: `system{}`-объявления обрабатываются `register_systems`, а
> `function run()` — `engine.run()`. Использование обоих привело бы к двойному исполнению.

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

```lua
-- Доступ как к полям таблицы
if entity.tilekind == TileKind.Wall then
    print("Entity is a wall")
end
-- TileKind.Floor = 0, TileKind.Wall = 1, TileKind.Water = 2
```

### Vec<T> и HashMap<String, V>

```rust
#[derive(Component, Clone, Debug, Scriptable)]
struct Tags { list: Vec<String> }

#[derive(Component, Clone, Debug, Scriptable)]
struct Stats { values: HashMap<String, f32> }
```

```lua
for entity in query({"Read:Tags"}) do
    print(entity.tags.list[1])            -- Vec → Lua массив (1-indexed)
end

for entity in query({"Read:Stats"}) do
    if entity.stats.values["hp"] > 50.0 then print("High HP") end  -- HashMap → хэш-таблица
end
```

## Глобальные функции Lua

| Функция | Описание |
|---|---|
| `system{ name, query, fn }` | Объявить Lua-систему (обрабатывается `register_systems`, см. §3) |
| `delta_time() → number` | Delta time текущего кадра |
| `entity_count() → integer` | Число живых entity (кешировано на момент запуска) |
| `query(descs) → iterator` | Итератор entity с компонентами |
| `commit(entity_table)` | Фиксирует Write-изменения в ECS |
| `spawn_entity(table)` | Создать entity с компонентами (отложено) |
| `despawn(entity_id)` | Уничтожить entity по id `entity.entity` (отложено; несовпадение поколения = no-op) |
| `read_resource(type_name) → value/nil` | Прочитать глобальный ресурс по имени типа |
| `write_resource(type_name, value)` | Записать глобальный ресурс (отложено) |
| `emit_event(type_name, value)` | Отправить событие (отложено) |
| `log(msg)` / `print(msg)` | Вывести `info` в лог движка |
| `log_debug(msg)` / `log_warn(msg)` / `log_error(msg)` | Вывести соответствующий уровень в лог |
| `inspect(value) → string` | Рекурсивный дамп Lua-значения (отладка) |

### Примеры работы с ресурсами и событиями

```lua
local g = read_resource("Gravity")           -- чтение (таблица с полями)
log("Gravity = " .. g.value)

write_resource("Score", { value = 100 })     -- запись (таблица полей структуры)
emit_event("PlayerDied", { x = 10.0, y = 20.0 })
```

> **Важно:** значения для `write_resource`/`emit_event` — Lua-таблицы с ключами по именам полей
> Rust-структуры. Для `Score { value: i64 }` — `{ value = 100 }`.

## Форматы query-дескрипторов

```lua
query({"Read:Position"})          -- явный Read
query({"Write:Velocity"})         -- явный Write (требует commit)
query({"Position"})               -- Read по умолчанию
query({"With:Player"})            -- фильтр: только entity с Player
query({"Without:Enemy"})          -- фильтр: только entity без Enemy
query({"Read:Position", "Changed:Position"})  -- реактивный: только изменённые с прошлого прогона
query({"Read:Health",  "Added:Health"})       -- реактивный: только добавленные с прошлого прогона
query({"Read:Position", "Write:Velocity", "With:Player"})  -- комбинированный
```

- `Read` / `Write` — доступ к данным (значение видно в таблице элемента; `Write` требует `commit`).
- `With` / `Without` — структурные фильтры (значение НЕ возвращается).
- `Changed:X` / `Added:X` — реактивные фильтры (симметрично `With:`); чтобы получить значение
  отфильтрованного компонента, добавьте `Read:X`.

Внутри `query()`/`commit()` идут через ту же ядерную `DynQuery`/`DynQueryMut`-поверхность, что и
типизированные запросы (общий валидируемый механизм доступа, §10.8) — своего unsafe-пути у скриптинга нет.

## Структура элемента query

Каждый элемент итератора — Lua-таблица со скрытым полем `_meta`:

```lua
for entity in query({"Read:Position", "Write:Velocity"}) do
    entity.entity       -- string "index:generation": непрозрачный id (передавать в despawn как есть)
    entity.position     -- table: { x = float, y = float } (Read-only)
    entity.velocity     -- table: { x = float, y = float } (Write-доступ)
end
```

Ключи — имена типов в **lowercase** (`"position"`, `"velocity"`).

Для Read-компонентов ставится `__newindex`-метатаблица, предупреждающая при попытке модификации:

```lua
entity.position.x = 5.0  -- предупреждение в логе при Read:Position
```

## commit(entity_table)

`commit()` записывает изменения Write-компонентов обратно в ECS (немедленно, между итерациями скрипта —
через `DynQueryMut`, с автоматическим обновлением change ticks, так что `Changed<T>` их видит):

```lua
for entity in query({"Write:Position", "Write:Velocity"}) do
    entity.position.x = entity.position.x + entity.velocity.x * dt
    commit(entity)  -- без commit() изменения НЕ применяются
end
```

`commit` перерезолвит entity против ЖИВОГО мира по `_meta.entity` (полное поколение), поэтому устаревший/
подделанный элемент не может направить запись в чужую строку. Запись гейтится по декларированным `Write:`
(S7): попытка записать необъявленный компонент отвергается.

### auto_commit

`ScriptEngine::set_auto_commit(true)` включает автоматический `commit()` при переходе к следующей entity
в цикле `for entity in query(...) do ... end` — явный `commit(entity)` не нужен:

```rust
engine.set_auto_commit(true);
```

## Конструкторы компонентов

`#[derive(Scriptable)]` генерирует Lua-конструктор для каждого типа:

```lua
local pos   = Position.new(10.0, 20.0)
local hp    = Health.new(100.0, 100.0)
local tags  = Tags.new({"enemy", "boss"})           -- Vec<String>
local stats = Stats.new({ hp = 100.0, mp = 50.0 })  -- HashMap<String, f32>
-- TileKind.Floor — C-like enum (доступ как поле таблицы)
```

Используются в `spawn_entity()`:

```lua
spawn_entity({
    position = Position.new(0.0, 0.0),
    velocity = Velocity.new(1.0, 0.5),
    health   = Health.new(100.0, 100.0),
})
```

## Хот-релоад

`ScriptEngine::with_dir(path)` запускает файловый наблюдатель (`notify`). При изменении `.lua`:

1. `poll_hot_reload()` обнаруживает событие (с time-based debounce 50ms — ровно одна перекомпиляция
   на одно сохранение);
2. файл перекомпилируется с новым sandbox-`_ENV`, `CompiledScript` заменяется в `HashMap`;
3. монолитный `run()` сразу использует новую версию;
4. для `system{}`-модели повторный `register_systems` снимает прошлое поколение скрипт-систем из
   планировщика (`Scheduler::remove_system`) и регистрирует текущее — изменённый набор `system{}`
   заменяет старый без дублей.

При ошибке компиляции старый скрипт продолжает работать, ошибка логируется.

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
        Some(Self { current: t.get("current").ok()?, max: t.get("max").ok()? })
    }

    fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
        let t = lua.create_table()?;
        t.set("new", lua.create_function(|lua, (current, max): (f32, f32)| {
            let t = lua.create_table()?;
            t.set("current", current)?; t.set("max", max)?;
            Ok(t)
        })?)?;
        lua.globals().set("Health", t)
    }
}
```

## WorldScriptingExt

Extension trait, объединяющий регистрацию в `World` и `ScriptEngine`:

```rust
// Было (два вызова):
world.register_component::<Position>();
engine.register_component::<Position>(&world);

// Стало (один вызов):
world.register_scriptable::<Position>(&mut engine);
```

| Метод | Действие |
|---|---|
| `world.register_scriptable::<T>(&mut engine)` | Регистрирует компонент `T` в World + ScriptEngine |
| `world.register_scriptable_resource::<T>(&mut engine)` | Регистрирует ресурс `T` в ScriptEngine (в мир вставьте `insert_resource` отдельно) |
| `world.register_scriptable_event::<T>(&mut engine)` | Регистрирует событие `T` в World + ScriptEngine |

## Sandbox-изоляция

Каждый скрипт выполняется в изолированном [`_ENV`](https://www.lua.org/manual/5.4/manual.html#2.2):

```text
Sandbox _ENV содержит:
├── Стандартные библиотеки: math, string, table, ipairs, pairs, next, select,
│   tonumber, tostring, type, unpack
├── API-функции: delta_time, entity_count, query, commit, system, spawn_entity,
│   despawn, read_resource, write_resource, emit_event, log, print,
│   log_debug/log_warn/log_error, inspect
└── Конструкторы компонентов: Position.new, Velocity.new, Health.new и т.д.
```

Библиотеки `io` и `os` **не включены** — скрипты не имеют доступа к файловой системе.

## Архитектура

```text
ScriptEngine  (!Send — держит Rc<Lua>, привязан к потоку создания)
  ├── lua: Rc<mlua::Lua>             — Lua 5.4 VM (Rc: раннеры system{} делят её)
  ├── ctx: Rc<RefCell<ScriptContext>>  — мост World ↔ Lua
  │     ├── delta_time: f32
  │     ├── world_ptr: NonNull<World>  — из &World; читается ТОЛЬКО как &World (живёт ≤ прогона)
  │     ├── deferred_despawns: Vec<Entity>          — отложенные despawn
  │     ├── deferred_spawns: Vec<SpawnRequest>      — отложенные spawn_entity
  │     ├── spawn_appliers: HashMap<name, applier>  — вставка компонентов спавна через Commands
  │     ├── deferred_resource_writes / deferred_events — отложенные write_resource / emit_event
  │     ├── bindings / resource_bindings / event_bindings — реестры доступа из Lua
  │     ├── script_systems: Vec<ScriptSystemDecl>   — объявления system{} (дренит register_systems)
  │     └── declared: Option<DeclaredAccess>        — декл. доступ бегущей system{} (enforcement)
  ├── scripts: HashMap<name, CompiledScript>        — { chunk_key, env_key }
  ├── watcher / watch_rx / last_reload              — хот-релоад .lua
  └── registered_system_ids: Vec<SystemId>          — id зарегистрированных system{} (для re-register)
```

Component/Resource/Event-запись идёт через `DeferredWorldOp = Box<dyn FnOnce(&mut World) + Send>`:
типизированное значение извлекается пока жива VM, затем применяется — монолитом напрямую или скрипт-
системой через `commands.add`. **`&mut World` из `world_ptr` не выводится нигде** (коммиты — через
declared-cell `DynQueryMut` с interior-mutable колонками; структурные/глобальные ops — через `Commands`).

### Жизненный цикл монолитного `run()`

1. Установить `delta_time` + `world_ptr` (из `&World`) в `ScriptContext`.
2. Выполнить main chunk (определяет `function run()` в sandbox `_ENV`; там же исполняются любые
   top-level-вызовы).
3. Вызвать `run()` из sandbox-окружения (если определена).
4. Дренировать ВСЕ отложенные эффекты (despawn → spawn → resource → event) в один `Commands`-буфер
   (с reserver мира → детерминированные id) и применить.
5. Сбросить `world_ptr`.

Для `system{}`-модели ту же роль играет per-system `Commands`-слот планировщика (см. §3).

### Отложенность изменений

Структурные и глобальные изменения нельзя применять во время итерации по архетипам — они
буферизуются и дренятся в `Commands` после скрипта:

- **Despawn** → `deferred_despawns: Vec<Entity>` → `commands.despawn`.
- **Spawn** → `deferred_spawns` → `commands.spawn().id()` + component appliers (детерминированные id).
- **Write-ресурсы / события** → `deferred_resource_writes` / `deferred_events` → `commands.add(closure)`.
- **Write-компоненты** — применяются немедленно при `commit()` через `DynQueryMut` (не буферизуются).

## Важные замечания

- **Золотой путь интеграции с планировщиком — `system{}` + `register_systems`** (§3): Lua-логика
  становится первоклассными системами с конфликт-детекцией и детерминизмом.
- `engine.run(dt, &mut world)` — простой монолитный путь без планировщика (§4); требует `&mut World`,
  поэтому не вызывается изнутри параллельной системы.
- **`ScriptEngine` — `!Send`** (держит `Rc<Lua>`): одна VM живёт на своём потоке. Настоящий Lua↔Lua
  параллелизм потребовал бы share-nothing VM-пула (не реализовано — ROI-gated, `decisions/ADR-005`).
  Для CPU-bound обработки тысяч сущностей используйте чистые Rust-системы.
