# Apex Scripting — Rhai Integration

Интеграция [Rhai](https://rhai.rs) скриптинга с хот-релоадом в Apex ECS.

## Новые крейты

| Крейт | Назначение |
|---|---|
| `apex-macros` | `#[derive(Scriptable)]` proc-macro |
| `apex-scripting` | ScriptEngine, ScriptContext, итераторы |

## Быстрый старт

### 1. Пометить компоненты

```rust
use apex_scripting::Scriptable;

#[derive(Clone, Copy, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy, Scriptable)]
struct Velocity { x: f32, y: f32 }
```

### 2. Настроить движок

```rust
use apex_scripting::ScriptEngine;
use std::path::Path;

// Регистрируем компоненты в мире (обычно)
world.register_component::<Position>();
world.register_component::<Velocity>();

// Создаём ScriptEngine
let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));

// Подключаем компоненты к скриптовому движку
engine.register_component::<Position>(&world);
engine.register_component::<Velocity>(&world);

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
| `log(msg)` | Вывести в лог движка |
| `print(msg)` | Вывести в stdout |

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

### `Archetype` (archetype.rs)

| Метод | Описание |
|---|---|
| `columns_raw() → &[Column]` | Доступ к колонкам компонентов для чтения/записи |
| `entity(row) → Entity` | Entity по индексу строки |
| `len() → usize` | Количество живых entity в архетипе |

### `Column` (archetype.rs)

`pub struct Column` — публичный тип, доступный из внешних крейтов.

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

### `ScriptableField` для примитивов, `ScriptableRegistrar` для структур

Двухуровневый дизайн позволяет:
- Автоматическую конвертацию примитивов без дополнительного кода
- Ручную реализацию для сложных типов (вложенные struct, enum)
- `#[derive(Scriptable)]` покрывает 90%+ случаев
