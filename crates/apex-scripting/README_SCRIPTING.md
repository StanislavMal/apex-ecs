# Apex Scripting — Rhai Integration

Интеграция [Rhai](https://rhai.rs) скриптинга с хот-релоадом в Apex ECS.

## Новые крейты

| Крейт | Назначение |
|---|---|
| `apex-macros` | `#[derive(Scriptable)]` proc-macro |
| `apex-scripting` | ScriptEngine, ScriptContext, итераторы |

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

### 2. Настроить движок

```rust
use apex_scripting::ScriptEngine;
use std::path::Path;

// Регистрируем компоненты в мире (обычно)
world.register_component::<Position>();
world.register_component::<Velocity>();

// Регистрируем ресурсы и события
world.resources.insert(Gravity { value: 9.8 });
world.resources.insert(Score { value: 0 });
world.add_event::<PlayerDied>();

// Создаём ScriptEngine
let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));

// Подключаем компоненты к скриптовому движку
engine.register_component::<Position>(&world);
engine.register_component::<Velocity>(&world);

// Подключаем ресурсы и события
engine.register_resource::<Gravity>();
engine.register_resource::<Score>();
engine.register_event::<PlayerDied>();

// Загружаем .rhai файлы
engine.load_scripts().expect("ошибка загрузки скриптов");
```

### 3. Game loop

```rust
loop {
    engine.poll_hot_reload(); // проверяет изменения .rhai файлов
    engine.run(dt, &mut world);
    world.tick();
}
```

### 4. Скрипт

```rhai
// scripts/game.rhai

fn run() {
    let dt = delta_time();

    // Чтение ресурсов
    let gravity = read_resource("Gravity");

    // Итерация с Read и Write доступом
    for entity in query(["Read:Velocity", "Write:Position"]) {
        entity.position.x += entity.velocity.x * dt;
        entity.position.y += entity.velocity.y * dt;
    }

    // Спавн новых entity
    if entity_count() < 100 {
        spawn_entity(#{
            position: Position(0.0, 0.0),
            velocity: Velocity(1.0, 0.5),
        });
    }

    // Деспавн по условию
    for entity in query(["Read:Health"]) {
        if entity.health.current <= 0.0 {
            despawn(entity.entity);
        }
    }

    // Запись ресурсов
    write_resource("Score", #{ value: 100 });

    // Отправка событий
    emit_event("PlayerDied", #{ x: 10.0, y: 20.0 });
}
```

## Поддерживаемые типы полей

`ScriptableField` реализован для:

| Rust тип | Rhai тип |
|---|---|
| `f32`, `f64` | `FLOAT` (f64) |
| `i32`, `i64`, `usize` | `INT` (i64) |
| `u32`, `u64` | `INT` (i64, lossy для u64) |
| `bool` | `bool` |
| `String` | `ImmutableString` |
| `(A, B)` | `Array [a, b]` |
| `(A, B, C)` | `Array [a, b, c]` |
| `Option<T>` | `()` или `T` |
| `Vec<T>` | `Array` |
| `HashMap<String, V>` | `Map` |
| `enum` (C-like) | `i64` (через `#[derive(Scriptable)]`) |

> **⚠️ C-like enum константы:** Константы C-like enum (`TileKind_Floor`, `TileKind_Wall`) регистрируются как **функции** Rhai. В скрипте обязательно используйте `TileKind_Floor()` **со скобками**. Без скобок Rhai выдаст ошибку `Variable not found`.

> **💡 snake_case в spawn_entity:** Ключи в `spawn_entity(#{tile_kind: TileKind_Floor()})` могут быть как в snake_case (`tile_kind`), так и PascalCase (`TileKind`). Движок нормализует оба варианта.

Для вложенных структур — реализуй `ScriptableRegistrar` вручную или добавь `#[derive(Scriptable)]`.

## Глобальные функции Rhai

| Функция | Описание |
|---|---|
| `delta_time() → float` | Delta time текущего кадра |
| `entity_count() → int` | Число живых entity (кешировано на момент `run()`) |
| `query(descs) → iter` | Итератор entity с компонентами |
| `spawn_entity(map)` | Создать entity с компонентами (отложено) |
| `spawn_empty()` | Создать пустую entity (отложено) |
| `despawn(entity_idx)` | Уничтожить entity (отложено) |
| `read_resource(type_name) → Dynamic` | Прочитать глобальный ресурс по имени типа |
| `write_resource(type_name, value)` | Записать глобальный ресурс (value — `Map` с полями) |
| `emit_event(type_name, value)` | Отправить событие (value — `Map` с полями) |
| `log(msg)` | Вывести в лог движка |
| `print(msg)` | Вывести в stdout |

### Примеры работы с ресурсами и событиями

```rust
// На стороне Rust — регистрация
engine.register_resource::<Gravity>();
engine.register_resource::<Score>();
engine.register_event::<PlayerDied>();
```

```rhai
// В скрипте — чтение ресурса (возвращает Map)
let g = read_resource("Gravity");
log(`Gravity = ${g.value}`);

// Запись ресурса (передаётся Map с полями структуры)
write_resource("Score", #{ value: 100 });

// Отправка события (передаётся Map с полями структуры)
emit_event("PlayerDied", #{ x: 10.0, y: 20.0 });
```

> **Важно:** Значения для `write_resource` и `emit_event` должны передаваться как `Map` с ключами, соответствующими именам полей Rust-структуры. Например, для `Score { value: i64 }` — `#{ value: 100 }`.

## Форматы query-дескрипторов

```rhai
query(["Read:Position"])          // явный Read
query(["Write:Velocity"])         // явный Write
query(["Position"])               // Read по умолчанию
query(["Read<Position>"])         // альтернативный синтаксис
query(["Write<Velocity>"])        // альтернативный синтаксис
```

## Структура элемента query

```rhai
for entity in query(["Read:Position", "Write:Velocity"]) {
    entity.entity     // INT: индекс entity
    entity.position   // Map: { x: float, y: float }
    entity.velocity   // Map: { x: float, y: float }
}
```

Ключи в Map — имена типов в **lowercase** (`"position"`, `"velocity"`).

## Хот-релоад

`ScriptEngine::with_dir(path)` запускает файловый наблюдатель (`notify`).

При изменении `.rhai` файла:
1. `poll_hot_reload()` обнаруживает событие
2. Перекомпилирует изменённый файл
3. Заменяет AST в HashMap (атомарно для однопоточного use-case)
4. Следующий вызов `run()` использует новую версию

При ошибке компиляции — старый скрипт продолжает работать, ошибка логируется.

> **Hot Reload префабов:** Файловые префабы (`.prefab`) также поддерживают горячую перезагрузку через `PrefabPlugin` из крейта `apex-hot-reload`. При изменении файла префаба все entity, созданные по этому префабу, автоматически пересоздаются через `reapply_asset()`/`reapply_all()`. Подробнее — в разделе [Hot Reload префабов (PrefabPlugin)](#11-hot-reload) руководства пользователя.

## Публичное API `apex-core`, используемое скриптингом

Скриптинг полагается на следующие публичные методы `apex-core`:

### `World` (world.rs)

| Метод | Описание |
|---|---|
| `archetypes() → &[Archetype]` | Доступ к архетипам для итерации |
| `registry() → &ComponentRegistry` | Реестр компонентов для поиска по имени |
| `entity_allocator() → &EntityAllocator` | Аллокатор entity для проверки живости |
| `component_id_by_name(name) → Option<ComponentId>` | Поиск ComponentId по строковому имени |
| `insert_raw_pub(entity, component_id, bytes, tick)` | Вставка компонента по сырым данным |
| `resources.try_get::<T>() → Option<&T>` | Чтение ресурса по типу |
| `resources.insert::<T>(value)` | Запись ресурса по типу |
| `send_event::<T>(event)` | Отправка события (паникует если не зарегистрировано) |
| `try_send_event::<T>(event) → bool` | Безопасная отправка события (возвращает false если не зарегистрировано) |
| `add_event::<T>()` | Регистрация типа события |

### `Archetype` (archetype.rs)

| Метод | Описание |
|---|---|
| `columns_raw() → &[Column]` | Доступ к колонкам компонентов для чтения/записи |
| `entity(row) → Entity` | Entity по индексу строки |
| `len() → usize` | Количество живых entity в архетипе |

### `Column` (archetype.rs)

`pub struct Column` — публичный тип, доступный из внешних крейтов.

### `EntityTemplate` + `TemplateRegistry` (template.rs)

| Метод | Описание |
|---|---|
| `World::register_template(name, template)` | Регистрация именованного шаблона в мире |
| `World::spawn_from_template(name, params) → Entity` | Создание entity по шаблону |
| `World::has_template(name) → bool` | Проверка наличия шаблона |
| `Commands::spawn_template(name)` | Отложенный спавн по шаблону (через Commands) |
| `Commands::spawn_from_template(name, params)` | Отложенный спавн с параметрами |

Эти методы используются скриптингом через `World::spawn_from_template()` при интеграции с `PrefabManifest` (который реализует `EntityTemplate`).

> **⚠️ ВАЖНО: Однопоточность Rhai**
>
> `apex-scripting` использует `Rc<RefCell<>>` (не `Arc<Mutex<>>`), потому что
> Rhai без фичи `"sync"` — однопоточный.
>
> - **НЕ ИСПОЛЬЗУЙТЕ** `ScriptEngine::run()` внутри `ParSystem` или параллельных
>   потоков — это приведёт к панике при попытке клонирования `Rc`
> - **НЕ ИСПОЛЬЗУЙТЕ** `#[derive(Scriptable)]` компоненты в параллельных системах,
>   работающих с тем же миром — `ScriptContext` не `Sync`
> - **НЕ ПЫТАЙТЕСЬ** передать `ScriptEngine` в другой поток — он не реализует `Send`
>
> Скриптинг предназначен **только для однопоточного последовательного выполнения**
> в главном игровом цикле. Для параллельной обработки данных используйте
> `ParSystem` и `Commands` (см. раздел 13 руководства пользователя).

## Архитектурные решения

### Однопоток: `Rc<RefCell<>>` вместо `Arc<Mutex<>>`

Rhai без фичи `"sync"` — однопоточный. `Rc<RefCell<>>` достаточно и не несёт
накладных расходов атомиков. Попытка использовать `ScriptContext` из другого
потока — compile error.

### `NonNull<World>` вместо `*mut World`

`ScriptContext` хранит `Option<NonNull<World>>`:
- `None` вне `run()` — любое обращение к world безопасно завершается ошибкой
- `NonNull` внутри `run()` — явный `unsafe` при разыменовании сигнализирует о намерении

### Два буфера для отложенных изменений

Spawn/despawn из скрипта нельзя применять во время итерации по архетипам.

- **Despawn** — накапливается в `Commands` (требует `Send`), применяется через `apply_deferred()` после скрипта
- **Spawn** — накапливается в `deferred_spawns: RefCell<Vec<SpawnRequest>>` (содержит `rhai::Dynamic` с `Rc`, не `Send`), перемещается в `ScriptEngine.spawn_queue` и применяется через `apply_spawn_queue()`

### Двухбуферность для Resources/Events

- **Чтение ресурсов** (`read_resource`) — использует shared borrow (`world_ref()`), безопасно во время выполнения скрипта
- **Запись ресурсов** (`write_resource`) и **отправка событий** (`emit_event`) — буферизируются в `deferred_resource_writes` / `deferred_events` (RefCell<Vec<(String, Dynamic)>>) во время выполнения скрипта
- **Применение** — после завершения скрипта вызывается `apply_deferred_resources_and_events()`, которая извлекает буферы и применяет их через `world_mut()`, что гарантирует отсутствие RefCell double-borrow при вызове внутри query()-итерации

### `ScriptableField` для примитивов, `ScriptableRegistrar` для структур

Двухуровневый дизайн позволяет:
- Автоматическую конвертацию примитивов без дополнительного кода
- Ручную реализацию для сложных типов (вложенные struct, enum)
- `#[derive(Scriptable)]` покрывает 90%+ случаев
