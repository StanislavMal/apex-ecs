# APEX ECS — Entity Component System Engine
### Руководство пользователя
> **Версия 0.1.0** | Rust Edition 2021

---

## Содержание

1. [Введение](#1-введение)
2. [Основные концепции](#2-основные-концепции)
3. [Архетипы и хранилище](#3-архетипы-и-хранилище)
4. [Query API](#4-query-api)
5. [Ресурсы и события](#5-ресурсы-и-события)
6. [Системы и планировщик](#6-системы-и-планировщик)
7. [Commands и DeferredQueue](#7-commands-и-deferredqueue)
8. [Relations (связи между entity)](#8-relations-связи-между-entity)
9. [Сериализация](#9-сериализация)
10. [Hot Reload](#10-hot-reload)
11. [Параллелизм](#11-параллелизм)
12. [Советы по производительности](#12-советы-по-производительности)
13. [Полный пример](#13-полный-пример)
14. [Быстрый справочник](#14-быстрый-справочник)
15. [Rhai Scripting](#15-rhai-scripting)

---

## 1. Введение

**Apex ECS** — это высокопроизводительный движок Entity Component System (ECS), написанный на Rust. Он разработан для применения в игровых движках и симуляциях, где требуется обработка сотен тысяч объектов с минимальными накладными расходами.

### 1.1 Ключевые возможности

- **Архетипное хранилище компонентов (SoA layout)** — данные одного типа хранятся рядом в памяти, что максимизирует использование CPU-кеша
- **Параллельное выполнение систем** — планировщик автоматически находит системы без конфликтов и запускает их параллельно через Rayon
- **Change Detection** — каждая строка данных хранит тик последнего изменения, запросы `Changed<T>` работают без overhead
- **Relations (связи между entity)** — иерархии, ownership и произвольные связи закодированы как компоненты
- **Сериализация мира** — снэпшот/восстановление состояния через JSON
- **Hot Reload конфигураций** — файловый watcher перезагружает JSON-конфиги без перезапуска
- **Batch API** — `spawn_many` создаёт тысячи entity за один проход

### 1.2 Структура крейтов

| Крейт | Назначение |
|---|---|
| `apex-core` | Ядро ECS: entity, component, archetype, query, world, events, relations, resources |
| `apex-scheduler` | Планировщик систем: компиляция графа зависимостей, параллельные Stage |
| `apex-graph` | Граф зависимостей: топологическая сортировка, обнаружение циклов |
| `apex-serialization` | Сериализация мира: WorldSnapshot, snapshot/restore |
| `apex-hot-reload` | Горячая перезагрузка: FileWatcher, HotReloadPlugin |
| `apex-macros` | Процедурные макросы: `#[derive(Scriptable)]` для интеграции с Rhai-скриптингом |
| `apex-scripting` | Rhai-скриптинг: ScriptEngine, регистрация компонентов/ресурсов/событий, хот-релоад `.rhai`-скриптов |

### 1.3 Установка

Добавьте зависимости в `Cargo.toml` вашего проекта:

```toml
[dependencies]
apex-core          = { path = "path/to/apex-ecs/crates/apex-core" }
apex-scheduler     = { path = "path/to/apex-ecs/crates/apex-scheduler" }
apex-serialization = { path = "path/to/apex-ecs/crates/apex-serialization" }
apex-hot-reload    = { path = "path/to/apex-ecs/crates/apex-hot-reload" }
apex-macros        = { path = "path/to/apex-ecs/crates/apex-macros" }
apex-scripting     = { path = "path/to/apex-ecs/crates/apex-scripting" }

# Для параллельного режима:
[features]
parallel = ["apex-core/parallel", "apex-scheduler/parallel"]
```

---

## 2. Основные концепции

### 2.1 Entity

Entity — это уникальный идентификатор объекта в мире. Он не хранит данные напрямую, только указывает на строку в архетипе.

```rust
// Entity — generational index: index + generation
// Generational counter защищает от use-after-free
pub struct Entity {
    index:      u32,   // позиция в аллокаторе
    generation: u32,   // инкрементируется при повторном использовании
}

// Проверка жизни entity:
world.is_alive(entity)   // -> bool
entity.index()           // -> u32
entity.generation()      // -> u32
```

> **Примечание:** Entity никогда не хранит компоненты напрямую. Все данные живут в Column-буферах архетипа. Entity — это только ключ для поиска.

### 2.2 Component

Компонент — это чистые данные без логики. Любой тип, реализующий `Send + Sync + 'static`, автоматически является компонентом.

```rust
// Компонент — просто struct с данными:
#[derive(Clone, Copy, Debug)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy)]
struct Velocity { x: f32, y: f32 }

#[derive(Clone, Copy)]
struct Health { current: f32, max: f32 }

// Маркерный компонент (ZST — zero-sized type):
struct Player;
struct Enemy;

// Регистрация без сериализации:
world.register_component::<Position>();

// Регистрация с сериализацией (требует Serialize + Deserialize):
#[derive(Serialize, Deserialize)]
struct Position { x: f32, y: f32 }
world.register_component_serde::<Position>();
```

> **Для Rhai-скриптинга** компоненты дополнительно помечаются `#[derive(Scriptable)]` из крейта `apex-macros`. Это автоматически реализует трейты `ScriptableField` для полей и `ScriptableRegistrar` для структуры, позволяя читать/писать компоненты из `.rhai`-скриптов. Подробнее — в разделе [Rhai Scripting](#15-rhai-scripting).

### 2.3 World

World — центральный контейнер, который хранит всё: entity, компоненты, ресурсы, события, relations.

```rust
use apex_core::prelude::*;

let mut world = World::new();

// Регистрация компонентов
world.register_component::<Position>();
world.register_component::<Velocity>();
world.register_component::<Health>();

// Создание entity с набором компонентов (Bundle):
let player = world.spawn_bundle((
    Position { x: 0.0, y: 0.0 },
    Velocity { x: 1.0, y: 0.0 },
    Health { current: 100.0, max: 100.0 },
));

// Создание entity пошагово:
let entity = world.spawn()
    .insert(Position { x: 5.0, y: 0.0 })
    .insert(Velocity { x: 0.0, y: 0.0 })
    .id();

// Batch-спавн (самый быстрый способ):
let entities = world.spawn_many(1000, |i| (
    Position { x: i as f32, y: 0.0 },
    Velocity { x: 0.1, y: 0.0 },
));

// Уничтожение entity:
world.despawn(player);

// Добавление/удаление компонентов:
world.insert(entity, Health { current: 50.0, max: 100.0 });
world.remove::<Velocity>(entity);

// Чтение компонента:
if let Some(pos) = world.get::<Position>(entity) {
    println!("pos: ({}, {})", pos.x, pos.y);
}

// Мутабельное чтение:
if let Some(hp) = world.get_mut::<Health>(entity) {
    hp.current -= 10.0;
}
```

---

## 3. Архетипы и хранилище

Apex ECS использует архетипное хранилище (Archetype Storage). Entity с одинаковым набором компонентов хранятся в одном архетипе — это обеспечивает cache-friendly итерацию.

### 3.1 Как работает хранилище

```
Archetype [Position, Velocity, Health]
┌─────────────┬─────────────┬─────────────────┐
│  Position   │  Velocity   │     Health      │
├─────────────┼─────────────┼─────────────────┤
│ (0.0, 0.0)  │ (1.0, 0.0)  │ {100.0, 100.0}  │ entity 0
│ (5.0, 3.0)  │ (0.5, 0.0)  │ {75.0, 100.0}   │ entity 1
│ (10.0, 0.0) │ (0.0, -1.0) │ {50.0, 100.0}   │ entity 2
└─────────────┴─────────────┴─────────────────┘

Данные одного компонента — contiguous в памяти → SIMD-friendly
```

> **Примечание:** При добавлении или удалении компонента entity перемещается в другой архетип. Используйте `add_edges`/`remove_edges` кеш для O(1) поиска нужного архетипа при повторных операциях.

### 3.2 Граф переходов архетипов

Каждый архетип хранит карту переходов: при добавлении компонента A — в какой архетип перейти, при удалении A — в какой вернуться.

```rust
// Внутренняя логика (для понимания):
// world.insert(entity, NewComponent { ... })
//   → find_or_create_archetype_with(current_arch, component_id)
//   → проверяем add_edges cache (O(1) при повторе)
//   → move_entity: копируем общие компоненты, swap_remove из старого
//   → записываем новый компонент в новый архетип
```

---

## 4. Query API

Query — основной способ итерации по компонентам. Apex ECS предоставляет несколько уровней Query API.

### 4.1 Параметры запроса

| Параметр | Тип доступа | Описание |
|---|---|---|
| `Read<T>` | Иммутабельный (`&T`) | Чтение компонента |
| `Write<T>` | Мутабельный (`&mut T`) | Запись компонента |
| `With<T>` | Только фильтр | Entity должен иметь T |
| `Without<T>` | Только фильтр | Entity не должен иметь T |
| `Changed<T>` | Иммутабельный (`&T`) | Только изменённые с тика |

### 4.2 `Query<Q>`

```rust
use apex_core::prelude::*;

// Простой запрос — итерация по Position:
Query::<Read<Position>>::new(&world)
    .for_each_component(|pos| {
        println!("pos: ({}, {})", pos.x, pos.y);
    });

// Запрос с Entity + мутацией:
Query::<(Read<Velocity>, Write<Position>)>::new(&world)
    .for_each(|entity, (vel, pos)| {
        pos.x += vel.x * 0.016;
        pos.y += vel.y * 0.016;
        println!("entity {:?} moved", entity);
    });

// Фильтрация по маркерному компоненту:
Query::<(Read<Health>, With<Player>)>::new(&world)
    .for_each_component(|hp| {
        println!("player HP: {}/{}", hp.current, hp.max);
    });

// Исключение компонента:
Query::<(Read<Position>, Without<Enemy>)>::new(&world)
    .for_each_component(|pos| { /* только не-Enemy */ });

// Change detection:
let last_tick = world.current_tick();
// ... (следующий тик) ...
Query::<Changed<Position>>::new_with_tick(&world, last_tick)
    .for_each_component(|pos| {
        println!("position changed: ({}, {})", pos.x, pos.y);
    });

// Итератор (стандартный Iterator trait):
let count = Query::<Read<Health>>::new(&world)
    .iter()
    .filter(|(_, hp)| hp.current < 25.0)
    .count();
```

> **Примечание:** `Query::new()` собирает список подходящих архетипов при создании. Для горячих путей используйте `CachedQuery`, который переиспользует этот список.

### 4.3 `CachedQuery`

`CachedQuery` кеширует список архетипов и инвалидируется только при изменении состава архетипов мира.

```rust
// CachedQuery — переиспользует список архетипов:
world.query_typed::<Read<Position>>()
    .for_each_component(|pos| { /* ... */ });

// С change detection:
world.query_changed::<(Read<Velocity>, Write<Position>)>(last_tick)
    .for_each(|entity, (vel, pos)| { /* ... */ });
```

### 4.4 `QueryBuilder` (динамический запрос)

Когда типы компонентов не известны статически — используйте `QueryBuilder`.

```rust
// QueryBuilder — runtime запрос:
let arch_ids = world.query()
    .read::<Position>()
    .write::<Velocity>()
    .exclude::<Enemy>()
    .matching_archetype_ids();

println!("Подходящих архетипов: {}", arch_ids.len());
```

---

## 5. Ресурсы и события

### 5.1 Resources

Ресурс — это глобальный синглтон, доступный из любой системы. Типичные примеры: конфиг физики, delta time, статистика кадра.

```rust
#[derive(Clone, Copy)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Default)]
struct FrameStats { frame: u32, total_entities: usize }

// Вставка ресурса:
world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
world.insert_resource(FrameStats::default());

// Чтение (паникует если не найден):
let cfg = world.resource::<PhysicsConfig>();
println!("gravity: {}", cfg.gravity);

// Мутабельный доступ:
world.resource_mut::<PhysicsConfig>().gravity = 1.62;

// Безопасное чтение (Option):
if let Some(stats) = world.try_resource::<FrameStats>() {
    println!("frame: {}", stats.frame);
}

// Безопасный мутабельный доступ (Option):
if let Some(mut stats) = world.try_resource_mut::<FrameStats>() {
    stats.frame += 1;
}

// Проверка наличия:
world.has_resource::<PhysicsConfig>() // -> bool

// Удаление:
let old_cfg = world.remove_resource::<PhysicsConfig>();
```

### 5.2 Events

События используют двойную буферизацию: `current` (текущий тик) и `previous` (прошлый тик). Вызов `world.tick()` переключает буферы.

```rust
#[derive(Clone, Copy)]
struct DamageEvent { target: Entity, amount: f32 }

#[derive(Clone, Copy)]
struct DeathEvent { entity: Entity }

// Регистрация типа события:
world.add_event::<DamageEvent>();
world.add_event::<DeathEvent>();

// Отправка события (паникует если тип не зарегистрирован):
world.send_event(DamageEvent { target: enemy, amount: 35.0 });

// Безопасная отправка (возвращает bool, не паникует):
if world.try_send_event(DamageEvent { target: enemy, amount: 35.0 }) {
    // событие отправлено
} else {
    // тип не зарегистрирован — вызовите world.add_event::<DamageEvent>()
}

// Чтение событий предыдущего тика (стандартный режим):
for ev in world.events::<DamageEvent>().iter_previous() {
    println!("damage: {} → entity {:?}", ev.amount, ev.target);
}

// Чтение событий текущего тика:
for ev in world.events::<DamageEvent>().iter_current() { /* ... */ }

// Мутабельный доступ к очереди событий:
let mut events = world.events_mut::<DamageEvent>();
events.clear(); // очистить все события

// Все события (current + previous):
for ev in world.events::<DamageEvent>().iter_all() { /* ... */ }

// Переключение буферов (вызывать раз в кадр):
world.tick(); // current → previous, новый current пустой
```

---

## 6. Системы и планировщик

Apex ECS предоставляет четыре уровня API для систем — от простого к гибкому.

### 6.1 `AutoSystem` (рекомендуется)

`AutoSystem` автоматически выводит `AccessDescriptor` из типа Query. Это исключает класс ошибок, где разработчик забыл задекларировать компонент.

> **Как это работает:** `AutoSystem` анализирует `type Query = (Read<A>, Write<B>, With<C>, Without<D>)` и автоматически строит `AccessDescriptor`:
> - `Read<T>` → read access к компоненту `T`
> - `Write<T>` → write access к компоненту `T`
> - `With<T>` / `Without<T>` → read access (для фильтрации)
> - Если система использует ресурсы или события — используйте `ParSystem` с явным `AccessDescriptor`

```rust
use apex_scheduler::{Scheduler, ParSystem};
use apex_core::prelude::*;

struct MovementSystem;

impl AutoSystem for MovementSystem {
    // Доступ выводится автоматически из Query:
    // reads: [Velocity], writes: [Position]
    type Query = (Read<Velocity>, Write<Position>);

    fn run(&mut self, ctx: SystemContext<'_>) {
        ctx.for_each_component::<Self::Query, _>(|(vel, pos)| {
            pos.x += vel.x * 0.016;
            pos.y += vel.y * 0.016;
        });
    }
}

let mut sched = Scheduler::new();
sched.add_auto_system("movement", MovementSystem);
```

### 6.2 `ParSystem` (явный access)

Используйте, когда система использует несколько Query, ресурсы или события — то, что `AutoSystem` не может вывести автоматически.

```rust
struct PhysicsSystem;

impl ParSystem for PhysicsSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new()
            .read::<PhysicsConfig>()  // ресурс
            .read::<Mass>()
            .write::<Velocity>()
            .write::<Position>()
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        let cfg = ctx.resource::<PhysicsConfig>();
        let dt = cfg.dt;
        let g = cfg.gravity;

        ctx.for_each_component::<(Read<Mass>, Write<Velocity>, Write<Position>), _>(
            |(mass, vel, pos)| {
                vel.y -= g * mass.0 * dt;
                pos.x += vel.x * dt;
                pos.y += vel.y * dt;
            }
        );
    }
}

sched.add_par_system("physics", PhysicsSystem);
```

### 6.3 `FnParSystem` (замыкание)

```rust
// Inline-система без отдельного struct:
sched.add_fn_par_system(
    "enemy_ai",
    |ctx: SystemContext<'_>| {
        ctx.for_each_component::<(Read<Enemy>, Write<Velocity>), _>(|(_, vel)| {
            vel.x *= 0.99;
            vel.y *= 0.99;
        });
    },
    AccessDescriptor::new()
        .read::<Enemy>()
        .write::<Velocity>(),
);
```

### 6.4 Sequential система

Sequential система получает `&mut World` и выполняется строго одна в своём Stage — используется для structural changes (spawn/despawn).

```rust
// Sequential системы — замыкания fn(&mut World):
sched.add_system("despawn_dead", |world: &mut World| {
    let deaths: Vec<Entity> = world
        .events::<DeathEvent>()
        .iter_current()
        .map(|ev| ev.entity)
        .collect();

    for entity in deaths {
        world.despawn(entity);
    }
});
```

> **Правило порядка:** Регистрируйте все Par-системы **ПЕРЕД** Sequential-системами. Sequential создаёт барьер «все системы до → я → все системы после», поэтому Par-система после Sequential автоматически получает зависимость от неё — это может создавать циклы через data-конфликты.

### 6.5 Компиляция и запуск планировщика

```rust
let mut sched = Scheduler::new();

// Регистрация — СНАЧАЛА все Par, ПОТОМ Sequential:
sched.add_par_system("physics",      PhysicsSystem);
sched.add_par_system("health_clamp", HealthClampSystem);
sched.add_auto_system("movement",    MovementSystem);

// Sequential ПОСЛЕ:
let damage_id  = sched.add_system("damage_apply", damage_apply).id();
let despawn_id = sched.add_system("despawn_dead", despawn_dead).id();
let stats_id   = sched.add_system("stats_update", stats_update).id();

// Явные зависимости (опционально):
sched.add_dependency(despawn_id, damage_id);  // despawn после damage
sched.add_dependency(stats_id,   despawn_id); // stats после despawn

// Компиляция — строит граф, проверяет циклы, группирует в Stage:
sched.compile().expect("circular dependency detected");

// Диагностика плана:
println!("{}", sched.debug_plan());

// Последовательный запуск:
sched.run_sequential(&mut world);

// Параллельный запуск (feature = "parallel"):
sched.run(&mut world);
```

### 6.6 `SystemContext`

`SystemContext` — read-only view на мир, доступный внутри системы. Предоставляет доступ к Query, ресурсам и событиям.

```rust
fn run(&mut self, ctx: SystemContext<'_>) {
    // Query:
    let q = ctx.query::<(Read<Velocity>, Write<Position>)>();
    q.for_each(|entity, (vel, pos)| { /* ... */ });

    // Сокращённая форма:
    ctx.for_each_component::<(Read<Vel>, Write<Pos>), _>(|(v, p)| { /* ... */ });

    // Ресурсы:
    let cfg   = ctx.resource::<PhysicsConfig>();        // Res<T>
    let mut s = ctx.resource_mut::<FrameStats>();       // ResMut<T>

    // События:
    let reader     = ctx.event_reader::<DamageEvent>(); // EventReader<T>
    let mut writer = ctx.event_writer::<DeathEvent>();  // EventWriter<T>
    writer.send(DeathEvent { entity });

    // Количество entity:
    ctx.entity_count() // -> usize

    // Параллельная итерация (feature = "parallel"):
    ctx.par_for_each_component::<(Read<Vel>, Write<Pos>), _>(|(v, p)| {
        /* выполняется на нескольких потоках */
    });
}
```

---

## 7. Commands и DeferredQueue

### 7.1 Commands

`Commands` буферизуют structural changes (spawn/despawn/insert/remove) для применения после завершения текущей итерации.

```rust
let mut cmds = Commands::new();

// Буферизация команд во время Query:
Query::<(Read<Health>, Read<Position>)>::new(&world)
    .for_each(|entity, (hp, pos)| {
        if hp.current <= 0.0 {
            cmds.despawn(entity);
        }
    });

// Применение всех команд за один проход:
cmds.apply(&mut world);

// Все поддерживаемые операции:
cmds.despawn(entity);
cmds.spawn_bundle((Position { x: 0.0, y: 0.0 }, Velocity { x: 1.0, y: 0.0 }));
cmds.insert(entity, NewComponent { value: 42 });
cmds.remove::<OldComponent>(entity);
cmds.add(|world: &mut World| { /* произвольная команда */ });
```

> **Совет:** `Commands::with_capacity(n)` — предаллоцирует буфер для `n` команд. Используйте, когда заранее знаете примерное количество команд.

### 7.2 DeferredQueue

`DeferredQueue` работает с raw `ComponentId` — используется в системах, где тип компонента неизвестен статически.

```rust
let mut queue = DeferredQueue::new();
queue.despawn(entity);
queue.remove_raw(entity, component_id);
queue.apply(&mut world);
```

---

## 8. Relations (связи между entity)

Relations позволяют создавать иерархии, ownership и произвольные связи между entity. Внутри они кодируются как специальные компоненты.

### 8.1 Встроенные виды связей

```rust
// Встроенные RelationKind:
// ChildOf — иерархия (cascade delete при уничтожении parent)
// Owns    — ownership
// Likes   — произвольная связь

// Добавление связи:
world.add_relation(child, ChildOf, parent);
world.add_relation(player, Owns, sword);

// Проверка:
world.has_relation(child, ChildOf, parent) // -> bool

// Получение target:
let parent_entity = world.get_relation_target(child, ChildOf); // -> Option<Entity>

// Итерация по дочерним entity:
for child in world.children_of(ChildOf, parent) {
    println!("child: {:?}", child);
}

// Удаление связи:
world.remove_relation(child, ChildOf, parent);

// Рекурсивное уничтожение иерархии:
world.despawn_recursive(ChildOf, root); // удаляет root + всех потомков
```

### 8.2 Кастомный `RelationKind`

```rust
// Создание своего типа связи:
#[derive(Clone, Copy)]
struct Targets;  // "атакует"

impl RelationKind for Targets {
    // Опционально: cascade delete — удалять subject при удалении target?
    fn cascade_delete_on_target_despawn() -> bool { false }
}

world.add_relation(archer, Targets, goblin);
```

### 8.3 Query по Relations

```rust
// Найти всех entity с ChildOf-связью к конкретному parent:
for (entity, pos) in world.query_relation::<ChildOf, Read<Position>>(ChildOf, parent) {
    println!("child {:?} at ({}, {})", entity, pos.x, pos.y);
}

// Wildcard — все entity с любым ChildOf-target:
for (entity, hp) in world.query_wildcard::<ChildOf, Read<Health>>(ChildOf) {
    println!("entity with parent: {:?}", entity);
}
```

---

## 9. Сериализация

`apex-serialization` предоставляет механизм сохранения/загрузки состояния мира. Сериализуются только компоненты, явно зарегистрированные через `register_component_serde`.

### 9.1 Настройка

```rust
use apex_serialization::{WorldSerializer, WorldSnapshot};
use serde::{Serialize, Deserialize};

// Только Serialize + Deserialize компоненты:
#[derive(Serialize, Deserialize, Clone, Copy)]
struct Position { x: f32, y: f32 }

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Health { current: f32, max: f32 }

// Не сериализуемый компонент (runtime данные):
struct RenderHandle(u64);  // нет derive Serialize

// Регистрация:
world.register_component_serde::<Position>(); // → в снэпшот
world.register_component_serde::<Health>();   // → в снэпшот
world.register_component::<RenderHandle>();   // → НЕ в снэпшот
```

### 9.2 Сохранение

```rust
// Создать снэпшот текущего состояния мира:
let snapshot = WorldSerializer::snapshot(&world)
    .expect("serialization failed");

// Сериализовать в JSON:
let json = snapshot.to_json().expect("json failed");

// Записать на диск:
std::fs::write("savegame.json", &json).unwrap();

// Информация о снэпшоте:
println!("entities: {}", snapshot.entities.len());
println!("relations: {}", snapshot.relations.len());
```

### 9.3 Загрузка

```rust
// Прочитать с диска:
let json = std::fs::read("savegame.json").unwrap();
let snapshot = WorldSnapshot::from_json(&json).unwrap();

// Подготовить новый мир (зарегистрировать те же типы):
let mut world = World::new();
world.register_component_serde::<Position>();
world.register_component_serde::<Health>();

// Восстановить — НЕ очищает мир (merge семантика):
let entity_map = WorldSerializer::restore(&mut world, &snapshot)
    .expect("restore failed");

// entity_map: HashMap<old_index, new_Entity>
// Используйте для патча внешних ссылок:
let new_player_entity = entity_map[&old_player_index];
```

> **Примечание:** Relations восстанавливаются автоматически на шаге 2 `restore` — после того как все entity уже созданы. Если тип `RelationKind` не зарегистрирован в мире — relation пропускается с предупреждением в лог.

---

## 10. Hot Reload

Apex ECS поддерживает два вида горячей перезагрузки:

- **JSON-конфиги** — через `apex-hot-reload` (ресурсы мира)
- **Rhai-скрипты** — через `apex-scripting` (игровая логика)

### 10.1 Hot Reload конфигураций (JSON)

`apex-hot-reload` позволяет изменять JSON-конфиги без перезапуска приложения. Изменения применяются в game loop без блокировки потока.

### 10.1.1 Настройка

```rust
use apex_hot_reload::HotReloadPlugin;
use serde::{Serialize, Deserialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Serialize, Deserialize, Clone)]
struct AudioConfig { master_volume: f32, music_volume: f32 }
```

### 10.1.2 Инициализация

```rust
// Создать plugin — следит за директорией assets/:
let mut hot = HotReloadPlugin::with_default_debounce(Path::new("assets/"))
    .expect("watcher init failed");

// Зарегистрировать конфиг — немедленно загружает файл:
hot.watch_config::<PhysicsConfig>(
    Path::new("assets/physics.json"),
    &mut world,
).expect("watch_config failed");

hot.watch_config::<AudioConfig>(
    Path::new("assets/audio.json"),
    &mut world,
).expect("watch_config failed");

// После watch_config ресурсы уже доступны в мире:
let cfg = world.resource::<PhysicsConfig>();
println!("gravity: {}", cfg.gravity);
```

### 10.1.3 Game loop

```rust
// В game loop — вызывать каждый кадр:
loop {
    // apply_changes < 1µs если нет изменений (non-blocking poll):
    let changed = hot.apply_changes(&mut world);
    for c in &changed {
        log::info!("reloaded: {:?}", c.path);
    }

    // Планировщик использует уже обновлённые ресурсы:
    scheduler.run(&mut world);
    world.tick();

    if should_exit { break; }
}
```

> **Формат файлов:** JSON. Структура файла должна точно соответствовать полям Rust struct (serde_json десериализация). При ошибке чтения/десериализации предыдущее значение ресурса остаётся в мире, ошибка пишется в `log::error!`.

### 10.1.4 Кастомный загрузчик

Если вам нужен нестандартный формат — реализуйте `ConfigLoader`:

```rust
use apex_hot_reload::ConfigLoader;

struct TomlConfigLoader;

impl ConfigLoader for TomlConfigLoader {
    fn reload(&self, path: &Path, world: &mut World) -> Result<(), HotReloadError> {
        let text = std::fs::read_to_string(path)?;
        let cfg: MyConfig = toml::from_str(&text)?;
        world.insert_resource(cfg);
        Ok(())
    }
}

hot.watch_config_with_loader(
    Path::new("assets/config.toml"),
    &mut world,
    TomlConfigLoader,
);
```

### 10.2 Hot Reload Rhai-скриптов

`apex-scripting` поддерживает горячую перезагрузку `.rhai`-файлов. При изменении файла на диске скрипт автоматически перекомпилируется и применяется в следующем кадре.

```rust
use apex_scripting::ScriptEngine;

// Следить за директорией scripts/:
let mut engine = ScriptEngine::with_dir("scripts/");

// В game loop:
loop {
    engine.poll_hot_reload();  // проверить изменения
    engine.run(dt, &mut world);
    world.tick();
}
```

Подробнее — в разделе [Rhai Scripting](#15-rhai-scripting).

---

## 11. Параллелизм

### 11.1 Параллельный запуск систем

Планировщик автоматически группирует совместимые Par-системы в одну Stage и запускает их параллельно через Rayon.

```toml
# Включение параллелизма (Cargo.toml):
[features]
parallel = ["apex-core/parallel", "apex-scheduler/parallel"]
```

```bash
# Запуск:
cargo run --features parallel
```

Правила параллелизма — аналог Rust borrow checker:

| Комбинация | Результат |
|---|---|
| `Read` + `Read` | Нет конфликта → параллельны |
| `Write` + `Read` | Конфликт → разные Stage |
| `Write` + `Write` | Конфликт → разные Stage |

**Пример:** `PhysicsSystem` (write `Velocity`, write `Position`, read `Mass`) и `HealthClampSystem` (write `Health`) не имеют общих Write → выполняются в одном Stage параллельно.

### 11.2 Параллельная итерация внутри системы

`par_for_each_component` использует chunk-level параллелизм: архетип разбивается на chunks по `PAR_CHUNK_SIZE` (4096) entity, каждый chunk обрабатывается независимо в Rayon thread pool.

```rust
impl ParSystem for PhysicsSystem {
    fn run(&mut self, ctx: SystemContext<'_>) {
        ctx.par_for_each_component::<(Read<Mass>, Write<Velocity>, Write<Position>), _>(
            |(mass, vel, pos)| {
                vel.y -= 9.8 * mass.0 * 0.016;
                pos.x += vel.x * 0.016;
                pos.y += vel.y * 0.016;
            }
        );
    }
}

// par_for_each — то же с Entity:
ctx.par_for_each::<Read<Position>, _>(|entity, pos| {
    /* обрабатывается параллельно */
});
```

> **Примечание:** `par_for_each` даёт реальный выигрыш когда архетип содержит **> 4096 entity** И вычисления CPU-bound (не memory-bandwidth bound). Для маленьких датасетов overhead Rayon превысит выигрыш.

---

## 12. Советы по производительности

### 12.1 Spawn

- Используйте `spawn_many()` вместо цикла `spawn_bundle()` — один batch-аллокатор вместо N отдельных
- `spawn_many_silent()` — то же что `spawn_many`, но без возврата `Vec<Entity>` — экономит heap-аллокацию
- Определяйте компоненты для entity сразу при спавне — структурные изменения после спавна дороже

### 12.2 Query

- `CachedQuery` (`world.query_typed<Q>()`) переиспользует список архетипов — дешевле `Query::new()` в hot path
- Используйте `With<T>`/`Without<T>` для фильтрации вместо `if` внутри closure
- `for_each_component()` быстрее `for_each()` — не загружает Entity для каждой строки

### 12.3 Structural changes

- Минимизируйте `insert`/`remove` в hot path — каждый вызов перемещает entity между архетипами
- Группируйте изменения через `Commands.apply()` — один проход вместо N структурных изменений
- Маркерные компоненты (ZST) бесплатны по памяти, но всё равно вызывают переход архетипа

### 12.4 Планировщик

- Регистрируйте все Par-системы **ДО** Sequential — это максимизирует размер параллельных Stage
- Один `compile()` при старте, потом только `run()` — `compile` дорогой, `run` дешёвый
- Чем больше Par-систем без конфликтов — тем лучше масштабируется на N ядер

### 12.5 Релизная сборка

```toml
# В Cargo.toml (уже настроено):
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
```

```bash
# Запуск с параллелизмом:
cargo run --release --features parallel
```

---

## 13. Полный пример

Минимальный рабочий пример, демонстрирующий все основные концепции:

```rust
use apex_core::prelude::*;
use apex_scheduler::{Scheduler, ParSystem};
use apex_core::access::AccessDescriptor;
use serde::{Serialize, Deserialize};

// Компоненты
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Velocity { x: f32, y: f32 }

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Health { current: f32, max: f32 }

#[derive(Clone, Copy)]
struct Player;

// Ресурс
#[derive(Clone, Copy, Serialize, Deserialize)]
struct DeltaTime(f32);

// Событие
#[derive(Clone, Copy)]
struct DeathEvent { entity: Entity }

// Par-система (AutoSystem)
struct MovementSystem;

impl AutoSystem for MovementSystem {
    type Query = (Read<Velocity>, Write<Position>);

    fn run(&mut self, ctx: SystemContext<'_>) {
        let dt = ctx.resource::<DeltaTime>().0;
        ctx.for_each_component::<Self::Query, _>(|(vel, pos)| {
            pos.x += vel.x * dt;
            pos.y += vel.y * dt;
        });
    }
}

fn main() {
    let mut world = World::new();

    // Регистрация
    world.register_component_serde::<Position>();
    world.register_component_serde::<Velocity>();
    world.register_component_serde::<Health>();
    world.register_component::<Player>();
    world.insert_resource(DeltaTime(0.016));
    world.add_event::<DeathEvent>();

    // Спавн
    let player = world.spawn_bundle((
        Position { x: 0.0, y: 0.0 },
        Velocity { x: 1.0, y: 0.0 },
        Health   { current: 100.0, max: 100.0 },
        Player,
    ));

    world.spawn_many(500, |i| (
        Position { x: i as f32, y: 0.0 },
        Velocity { x: 0.1, y: 0.0 },
        Health   { current: 50.0, max: 50.0 },
    ));

    // Планировщик
    let mut sched = Scheduler::new();

    sched.add_auto_system("movement", MovementSystem);
    sched.add_system("cleanup", |world: &mut World| {
        let mut cmds = Commands::new();
        Query::<Read<Health>>::new(world).for_each(|e, hp| {
            if hp.current <= 0.0 { cmds.despawn(e); }
        });
        cmds.apply(world);
    });

    sched.compile().unwrap();

    // Game loop
    for _ in 0..3 {
        sched.run(&mut world);
        world.tick();
    }

    println!("entities: {}", world.entity_count());

    // Сохранение
    use apex_serialization::WorldSerializer;
    let snap = WorldSerializer::snapshot(&world).unwrap();
    std::fs::write("save.json", snap.to_json().unwrap()).unwrap();
    println!("saved {} entities", snap.entities.len());
}
```

> **Вариант с Rhai-скриптингом:** Тот же пример, но логика движения вынесена в `.rhai`-скрипт:
>
> ```rust
> use apex_scripting::ScriptEngine;
> use apex_macros::Scriptable;
>
> // Компоненты — добавляем Scriptable для доступа из Rhai
> #[derive(Clone, Scriptable)]
> struct Position { x: f32, y: f32 }
>
> #[derive(Clone, Scriptable)]
> struct Velocity { x: f32, y: f32 }
>
> #[derive(Clone, Scriptable)]
> struct Health { current: f32, max: f32 }
>
> #[derive(Clone, Scriptable)]
> struct Player;
>
> #[derive(Clone, Scriptable)]
> struct DeltaTime(f32);
>
> fn main() {
>     let mut world = World::new();
>
>     world.register_component::<Position>();
>     world.register_component::<Velocity>();
>     world.register_component::<Health>();
>     world.register_component::<Player>();
>     world.insert_resource(DeltaTime(0.016));
>
>     // Настройка ScriptEngine
>     let mut engine = ScriptEngine::new();
>     engine.register_component::<Position>(&world);
>     engine.register_component::<Velocity>(&world);
>     engine.register_component::<Health>(&world);
>     engine.register_component::<Player>(&world);
>     engine.register_resource::<DeltaTime>();
>
>     engine.load_script_str("move", r#"
>         let dt = delta_time();
>         for entity in query([Read(Velocity), Write(Position)]) {
>             entity.pos.x += entity.vel.x * dt;
>             entity.pos.y += entity.vel.y * dt;
>         }
>     "#).unwrap();
>     engine.set_active("move").unwrap();
>
>     // Спавн
>     let _player = world.spawn_bundle((
>         Position { x: 0.0, y: 0.0 },
>         Velocity { x: 1.0, y: 0.0 },
>         Health   { current: 100.0, max: 100.0 },
>         Player,
>     ));
>
>     world.spawn_many(500, |i| (
>         Position { x: i as f32, y: 0.0 },
>         Velocity { x: 0.1, y: 0.0 },
>         Health   { current: 50.0, max: 50.0 },
>     ));
>
>     // Game loop
>     for _ in 0..3 {
>         engine.run(0.016, &mut world);
>         world.tick();
>     }
>
>     println!("entities: {}", world.entity_count());
> }
> ```

---

## 14. Быстрый справочник

### World API

| Метод | Описание |
|---|---|
| `spawn()` | Создать EntityBuilder для пошагового добавления компонентов |
| `spawn_bundle(bundle)` | Создать entity с набором компонентов |
| `spawn_many(n, \|i\| bundle)` | Batch-спавн N entity (возвращает Vec) |
| `spawn_many_silent(n, \|i\| bundle)` | Batch-спавн N entity (без возврата Vec) |
| `spawn_empty()` | Создать entity без компонентов |
| `despawn(entity)` | Уничтожить entity |
| `insert(entity, component)` | Добавить компонент |
| `remove::<T>(entity)` | Удалить компонент |
| `get::<T>(entity)` | Прочитать компонент → `Option<&T>` |
| `get_mut::<T>(entity)` | Изменить компонент → `Option<&mut T>` |
| `insert_resource(value)` | Вставить ресурс |
| `resource::<T>()` | Прочитать ресурс (panic если нет) |
| `resource_mut::<T>()` | Изменить ресурс |
| `try_resource::<T>()` | Безопасное чтение ресурса → `Option<Res<T>>` |
| `try_resource_mut::<T>()` | Безопасное мутабельное чтение → `Option<ResMut<T>>` |
| `has_resource::<T>()` | Проверить наличие ресурса → `bool` |
| `remove_resource::<T>()` | Удалить ресурс → `Option<T>` |
| `add_event::<T>()` | Зарегистрировать тип события |
| `send_event(event)` | Отправить событие (panic если не зарегистрирован) |
| `try_send_event(event)` | Безопасная отправка события → `bool` |
| `events::<T>()` | Получить `EventQueue<T>` (иммутабельно) |
| `events_mut::<T>()` | Получить `EventQueue<T>` (мутабельно) |
| `tick()` | Переключить буферы событий, +1 тик |
| `query_typed::<Q>()` | CachedQuery — кешированный запрос |
| `query_changed::<Q>(tick)` | CachedQuery с change detection |
| `query_relation::<K, Q>(kind, target)` | Query по relation |
| `query_wildcard::<K, Q>(kind)` | Query по relation (любой target) |
| `add_relation(s, kind, t)` | Создать связь subject→target |
| `has_relation(s, kind, t)` | Проверить наличие связи |
| `get_relation_target(s, kind)` | Получить target связи → `Option<Entity>` |
| `children_of(kind, parent)` | Итерация по дочерним entity |
| `despawn_recursive(kind, e)` | Удалить entity + потомков |
| `register_component::<T>()` | Зарегистрировать компонент |
| `register_component_serde::<T>()` | Зарегистрировать + сериализация |
| `entity_count()` | Количество живых entity → `usize` |
| `is_alive(entity)` | Проверить, жив ли entity → `bool` |
| `current_tick()` | Текущий тик мира → `Tick` |

### Scheduler API

| Метод | Описание |
|---|---|
| `add_auto_system(name, sys)` | Добавить AutoSystem |
| `add_par_system(name, sys)` | Добавить ParSystem |
| `add_fn_par_system(name, f, acc)` | Добавить FnParSystem (closure) |
| `add_system(name, f)` | Добавить Sequential систему |
| `add_dependency(a, b)` | `a` выполняется после `b` |
| `compile()` | Скомпилировать план → `Result` |
| `run(&mut world)` | Запустить (параллельно если возможно) |
| `run_sequential(&mut world)` | Запустить последовательно |
| `debug_plan()` | Краткий план выполнения |
| `debug_plan_verbose()` | Подробная диагностика плана |

### ScriptEngine API

| Метод | Описание |
|---|---|
| `new()` | Создать ScriptEngine |
| `with_dir(path)` | Создать ScriptEngine с файловым watcher для `.rhai` |
| `register_component::<T>(&world)` | Зарегистрировать компонент для доступа из Rhai |
| `register_resource::<T>()` | Зарегистрировать ресурс для доступа из Rhai |
| `register_event::<T>()` | Зарегистрировать событие для отправки из Rhai |
| `load_script_str(name, code)` | Загрузить скрипт из строки |
| `load_scripts()` | Загрузить все `.rhai`-файлы из директории |
| `set_active(name)` | Установить активный скрипт |
| `run(dt, &mut world)` | Выполнить активный скрипт |
| `poll_hot_reload()` | Проверить изменения `.rhai`-файлов на диске |

---

## 15. Rhai Scripting

`apex-scripting` интегрирует скриптовый язык **Rhai** в Apex ECS. Скрипты можно использовать для описания игровой логики, прототипирования и хот-релоада поведения без перекомпиляции Rust.

### 15.1 Быстрый старт

```rust
use apex_scripting::ScriptEngine;
use apex_macros::Scriptable;

// 1. Пометить компоненты, ресурсы и события
#[derive(Clone, Scriptable)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Scriptable)]
struct Velocity { x: f32, y: f32 }

#[derive(Clone, Scriptable)]
struct Gravity(f32);  // ресурс

#[derive(Clone, Scriptable)]
struct CollisionEvent { entity: Entity }  // событие

fn main() {
    let mut world = World::new();

    // 2. Настроить движок
    let mut engine = ScriptEngine::new();
    engine.register_component::<Position>(&world);
    engine.register_component::<Velocity>(&world);
    engine.register_resource::<Gravity>();
    engine.register_event::<CollisionEvent>();

    // 3. Загрузить скрипт
    engine.load_script_str("game", r#"
        let dt = delta_time();
        for entity in query([Read(Velocity), Write(Position)]) {
            entity.pos.x += entity.vel.x * dt;
            entity.pos.y += entity.vel.y * dt;
        }
    ").unwrap();
    engine.set_active("game").unwrap();

    // 4. Game loop
    loop {
        engine.run(0.016, &mut world);
        world.tick();
    }
}
```

### 15.2 Глобальные функции Rhai

| Функция | Сигнатура | Описание |
|---|---|---|
| `delta_time` | `|| → f64` | Текущий dt, переданный в `run()` |
| `entity_count` | `|| → i64` | Количество entity в мире |
| `query` | `\|[QueryDesc]\| → Iterator` | Итерация по компонентам |
| `spawn` | `\|[ComponentValue]\| → Entity` | Создать entity с компонентами |
| `despawn` | `\|Entity\|` | Уничтожить entity |
| `read_resource` | `\|type_name\| → Dynamic` | Прочитать ресурс (Rhai Map) |
| `write_resource` | `\|type_name, value\|` | Записать ресурс |
| `emit_event` | `\|type_name, value\|` | Отправить событие |
| `log` | `\|level, message\|` | Логирование (trace/debug/info/warn/error) |

### 15.3 Формат query-дескрипторов

```rust
// query([Read(ComponentName), Write(ComponentName), With(Marker), Without(Exclude)])
// Read — иммутабельное чтение
// Write — мутабельное чтение
// With — фильтр наличия компонента
// Without — фильтр отсутствия компонента

// Примеры:
query([Read(Position)])                              // только чтение
query([Read(Velocity), Write(Position)])              // чтение + запись
query([Read(Health), With(Player)])                   // фильтр по маркеру
query([Read(Position), Without(Enemy)])               // исключение
query([Read(Transform), Read(Health), Write(Velocity)]) // множественные компоненты
```

### 15.4 Структура элемента query

Каждый элемент итератора `query()` — это Rhai Map с полями компонентов, именованными по **snake_case** имени типа:

```rust
// Для компонентов:
//   struct Velocity { x: f32, y: f32 }
//   struct Position { x: f32, y: f32 }
// Поля в Rhai:
//   entity.vel.x, entity.vel.y, entity.pos.x, entity.pos.y

// Для компонентов-кортежей (ZST или newtype):
//   struct Gravity(f32);
//   entity.gravity.0

// Для маркерных компонентов (ZST без полей):
//   struct Player;
//   entity.player  // → true (есть компонент)
```

### 15.5 Работа с ресурсами и событиями

```rust
// Запись ресурса:
write_resource("Gravity", 9.8);

// Чтение ресурса (возвращает Rhai Map):
let g = read_resource("Gravity");
log("info", `gravity value: ${g.0}`);

// Отправка события:
emit_event("CollisionEvent", #{ entity: entity_id });

// Внутренняя архитектура: все write_resource и emit_event
// буферизуются во время выполнения скрипта и применяются
// после завершения скрипта — это предотвращает RefCell double-borrow
// при вызове внутри query()-итерации.
```

### 15.6 Хот-релоад скриптов

`ScriptEngine` поддерживает горячую перезагрузку `.rhai`-файлов из директории:

```rust
// Следить за директорией scripts/:
let mut engine = ScriptEngine::with_dir("scripts/");

// В game loop:
loop {
    engine.poll_hot_reload();  // проверить изменения файлов
    engine.run(dt, &mut world);
    world.tick();
}
```

При изменении `.rhai`-файла движок автоматически перекомпилирует и применяет новый скрипт. Если компиляция не удалась — старое поведение сохраняется, ошибка пишется в лог.

### 15.7 Поддерживаемые типы полей

| Rust тип | В Rhai |
|---|---|
| `f32`, `f64` | `f64` (число с плавающей точкой) |
| `i32`, `i64` | `i64` (целое) |
| `u32`, `u64` | `i64` (целое, беззнаковые конвертируются) |
| `usize` | `i64` |
| `bool` | `bool` |
| `String` | `string` |
| `&'static str` | `string` |
| `(A, B)` | `[a, b]` (массив из 2 элементов) |
| `(A, B, C)` | `[a, b, c]` (массив из 3 элементов) |
| `Option<T>` | `null` или значение типа `T` |

### 15.8 Публичное API apex-core для скриптинга

Методы `World`, используемые `apex-scripting`:

| Метод | Описание |
|---|---|
| `world.registry().get_id::<T>()` | Получить ComponentId по типу |
| `world.archetypes()` | Список архетипов для итерации |
| `world.insert_resource(value)` | Вставить ресурс |
| `world.try_resource::<T>()` | Безопасное чтение ресурса |
| `world.try_resource_mut::<T>()` | Безопасное мутабельное чтение |
| `world.try_send_event(event)` | Безопасная отправка события |
| `world.events_mut::<T>()` | Мутабельный доступ к очереди событий |
| `world.resource_raw_ptr::<T>()` | Raw pointer для скриптинга |

---

*Apex ECS v0.1.0 • Rust Edition 2021 • MIT License*