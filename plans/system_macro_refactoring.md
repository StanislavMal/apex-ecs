# Apex ECS — Рефакторинг: `system!` макрос + эргономика

> **Цель:** Уменьшить boilerplate при объявлении систем с 8-10 строк до 2-6 строк,
> сохранив все текущие преимущества: compile-time access-проверку, именованную диагностику
> конфликтов, быструю компиляцию.
>
> **Принцип:** macro_rules!, не proc-macro. Никаких новых зависимостей.
>
> **Статус:** ✅ Все фазы завершены. `system!` + `sequential_system!` покрывают весь API.

---

## 1. Результаты реализации

### Что сделано

| # | Описание | Где | Статус |
|---|----------|-----|--------|
| 1 | Default `type Events = ()` в трейте | `system_param.rs` | ❌ Отменено (unstable) |
| 1b | 1-element tuple impl для WorldQuery, ResourceAccessList, EventAccessList | `query.rs:468`, `system_param.rs:262,315` | ✅ |
| 2 | `system!` макрос — Variant A + B | `system_macro.rs` (~380 строк) | ✅ |
| 3 | `ctx: Ctx` — прямой доступ к SystemContext | `system_macro.rs` | ✅ |
| 4 | `__whole: WholeWorld` — NEEDS_WHOLE_WORLD | `system_macro.rs` | ✅ |
| 5 | Event writer: `EventWriter::send()` вместо buffer+flush | `system_macro.rs` | ✅ |
| 6 | Несколько `Emit<E>` в одной системе | `system_macro.rs` | ✅ |
| 7 | `compile_error!` для нераспознанных параметров | `system_macro.rs` | ✅ |
| 8 | Реэкспорт в `lib.rs` + prelude | `apex-core/src/lib.rs`, `apex-app/src/lib.rs` | ✅ |
| 9 | Relations: Commands (`add_relation`, `remove_relation`, `add_relation_batch`) | `commands.rs` | ✅ |
| 10 | Relations: SystemContext (`query_relation`, `query_wildcard`, `children_of`, `has_relation`, `get_relation_target`) | `world.rs` | ✅ |
| 11 | `App::add_sequential_system()` | `app.rs` | ✅ |
| 12 | `sequential_system!` — Variant A + B | `system_macro.rs` (~290 строк) | ✅ |
| 13 | Обновление `minimal.rs` — `system!` + `sequential_system!` | `apex-input/examples/minimal.rs` | ✅ |

### Метрики приёмки

- ✅ `cargo test -p apex-core` — 106 тестов
- ✅ `cargo test -p apex-app` — 19 тестов
- ✅ `cargo build --example minimal` — 11 систем (5 рабочих + 6 compile-check)
- ✅ `system!` Variant A — query, resources, events, commands, ctx, wholeworld
- ✅ `system!` Variant B — struct + Default + `s: &mut Self` + `impl AutoSystem`
- ✅ `sequential_system!` Variant A — `&mut World` + все параметры из `system!`
- ✅ `sequential_system!` Variant B — struct + Default + `into_system() -> impl FnMut`
- ✅ Multiple `Emit<E>` — через `EventWriter::send()`
- ✅ Relations — read via `ctx: Ctx`, write via `cmd: Cmd`

---

## 2. Отклонения от исходного плана

### 2.1 Default `type Events = ()` — НЕ СДЕЛАНО

**Причина:** `associated_type_defaults` — unstable feature (#29661) на stable Rust 1.94.0.

**Решение:** макрос всегда генерирует `type Events = (...)` явно.

### 2.2 Именование структур

| План | Факт |
|------|------|
| `fn movement(...)` → `struct MovementSystem` | `fn movement_system(...)` → `struct movement_system` |

**Причина:** macro_rules! не может snake_case → CamelCase. `#[allow(non_camel_case_types)]` подавляет warning.

### 2.3 Variant B: `s: &mut Self` вместо `self: &mut Self`

**Причина:** `self` — keyword, не захватывается как `$slf:ident`. Пользователь выбирает ЛЮБОЕ имя.

### 2.4 Event reader: `EventReader<T>` вместо `&[E]`

Фактический API EventReader не совпал с предположениями. Биндинг: `let ev = ctx.event_reader::<E>();`, вызов `.iter()`.

### 2.5 Event writer: `EventWriter::send()` вместо buffer+flush

Устранило проблему уникальных имён буферов. Несколько эмиттеров без ограничений.

### 2.6 Hygiene: metavariables для `ctx`, `world`, `s`

- `ctx` передаётся как `$ctx:ident` через рекурсию `__system_impl!`
- `world` передаётся как `@world` accumulator (из захваченного `$pname:ident : &mut World`)
- `s` (state accessor) передаётся как `@slf: [ $slf ]`

### 2.7 `sequential_system!` использует `SystemFn = Box<dyn FnMut(&mut World) + Send>`

Оказалось, что в движке уже есть тип sequential систем — `SystemFn` в scheduler `lib.rs:152`.
Не нужен новый трейт. Variant B генерирует `into_system() -> impl FnMut`, захватывая состояние в замыкание.

### 2.8 `cmd: Cmd` в sequential — ручной apply

В parallel системах Commands применяются планировщиком после stage. В sequential — пользователь
вызывает `cmd.apply(world)` вручную, когда готов. Это даёт контроль над порядком и временем жизни borrow'ей.

---

## 3. Текущий синтаксис `system!`

### Вариант А — без состояния

```rust
system! {
    fn movement_system(
        q: (Read<Velocity>, Write<Position>),
        keys: &Input<KeyCode>,
    ) {
        for (_, (vel, pos)) in q.iter() {
            if keys.pressed(KeyCode::A) { pos.x -= vel.x; }
        }
    }
}
// Регистрация: app.add_system(Update, movement_system);
```

### Вариант Б — с состоянием

```rust
system! {
    struct WaveSpawner {
        wave: u32 = 1,
        enemies_spawned: u32 = 0,
    }
    fn run(s: &mut Self, cmd: Cmd, ctx: Ctx) {
        if s.wave <= 5 {
            cmd.spawn((Enemy, Position::default()));
            s.enemies_spawned += 1;
        }
    }
}
// Генерирует: struct + impl Default + impl AutoSystem
// Регистрация: app.add_system(Update, WaveSpawner::default());
```

### Полная таблица параметров `system!`

| Параметр | Associated type | Let-биндинг |
|----------|----------------|-------------|
| `q: (Read<A>, Write<B>)` | `type Query = (Read<A>, Write<B>)` | `let q = ctx.query::<Self::Query>();` |
| `q: Read<A>` (bare) | `type Query = (Read<A>)` | `let q = ctx.query::<Self::Query>();` |
| `name: &T` | `ResRead<T>` | `let name: &T = &*ctx.resource::<T>();` |
| `name: &mut T` | `ResWrite<T>` | `let name: &mut T = &mut *ctx.resource_mut::<T>();` |
| `name: &[E]` | `Listen<E>` | `let name = ctx.event_reader::<E>();` |
| `name: &mut Vec<E>` | `Emit<E>` | `let mut name = ctx.event_writer::<E>();` (`.send()`) |
| `name: Cmd` | *(none)* | `let name: &mut Commands = ctx.commands();` |
| `name: Ctx` | *(none)* | `let name: &SystemContext = &ctx;` |
| `__whole: WholeWorld` | `const NEEDS_WHOLE_WORLD = true` | *(none)* |

---

## 4. `sequential_system!` — актуальный синтаксис

### Вариант А — без состояния

```rust
sequential_system! {
    fn cleanup(
        world: &mut World,       // → параметр функции (&mut World)
        events: &[DeathEvent],   // → world.event_reader::<DeathEvent>()
        config: &CleanupConfig,  // → world.resource::<CleanupConfig>()
        cmd: Cmd,                // → let mut cmd = Commands::new();
    ) {
        for ev in events.iter() {
            if config.active { cmd.despawn(ev.entity); }
        }
        cmd.apply(world);        // ручной apply
    }
}
// Генерирует: fn cleanup(world: &mut World) { ... }
// Регистрация: app.add_sequential_system(PostUpdate, "cleanup", cleanup);
```

### Вариант Б — с состоянием

```rust
sequential_system! {
    struct LuaRunner {
        engine: ScriptEngine = ScriptEngine::with_dir("scripts/"),
    }
    fn run(
        s: &mut Self,
        world: &mut World,
        dt: &Time,
    ) {
        s.engine.run(dt, world);
    }
}
// Генерирует: struct + impl Default + fn into_system(self) -> impl FnMut(&mut World) + Send
// Регистрация:
//   let system = LuaRunner::default().into_system();
//   app.scheduler_mut().add_system("lua", system);
```

### Таблица параметров `sequential_system!`

| Параметр | Let-биндинг | Примечание |
|----------|-------------|-----------|
| `world: &mut World` | параметр функции (не биндинг) | эксклюзивный доступ к миру |
| `q: (Read<A>, Write<B>)` | `let q = CachedQuery::new(&world, Tick::ZERO);` | |
| `q: Read<A>` (bare) | `let q = CachedQuery::new(&world, Tick::ZERO);` | |
| `name: &T` | `let name: &T = world.resource::<T>();` | |
| `name: &mut T` | `let name: &mut T = world.resource_mut::<T>();` | |
| `name: &[E]` | `let name = world.event_reader::<E>();` | |
| `name: &mut Vec<E>` | `let mut name = world.event_writer::<E>();` | `.send()` |
| `name: Cmd` | `let mut name = Commands::new();` | **ручной** `cmd.apply(world);` |
| `name: Ctx` | `let name: &World = &world;` | все read-only методы World |
| `__whole: WholeWorld` | *(none)* | бессмысленно для sequential |

### Ключевые отличия от `system!`

| | `system!` (parallel) | `sequential_system!` |
|---|---|---|
| Контекст | `SystemContext<'_>` | `&mut World` |
| Trait | `AutoSystem` | нет (просто функция или замыкание) |
| Associated types | `type Query`, `type Resources`, `type Events` | нет |
| Регистрация | `App::add_system(label, system)` | `App::add_sequential_system(label, name, func)` |
| `cmd: Cmd` | `&mut Commands` (авто-apply после stage) | `Commands` (ручной `cmd.apply(world)`) |
| `ctx: Ctx` | `&SystemContext<'_>` | `&World` |
| Параллельность | Да (ASD-чанкование) | Нет (строго последовательно) |

---

## 5. Relations API

Доступны через `ctx: Ctx` и `cmd: Cmd` в обоих макросах.

### Чтение (SystemContext / World)

```rust
// В system!: через ctx: Ctx
ctx.children_of(ChildOf, root)
ctx.query_relation::<ChildOf, Read<Position>>(ChildOf, parent)
ctx.query_wildcard::<ChildOf, Read<Health>>(ChildOf)
ctx.has_relation(subject, Owns, target)
ctx.get_relation_target(entity, ChildOf)

// В sequential_system!: через ctx: Ctx (даёт &World)
// — все те же методы, т.к. они определены на World
```

### Запись (Commands)

```rust
cmd.add_relation(entity, ChildOf, parent);
cmd.remove_relation(entity, Owns, target);
cmd.add_relation_batch(vec![e1, e2, e3], ChildOf, root);
```

### Реализация Commands

| Метод | Тип команды | Аллокация |
|-------|------------|-----------|
| `add_relation` | `Command::AddRelation` (function pointer) | Нет |
| `remove_relation` | `Command::RemoveRelation` (function pointer) | Нет |
| `add_relation_batch` | `Command::Apply(Box<dyn FnOnce>)` (замыкание) | Box |

### Реализация SystemContext

| Метод | Возвращает |
|-------|-----------|
| `query_relation<R, Q>(kind, target)` | `RelationIter<Q>` |
| `query_wildcard<R, Q>(kind)` | `RelationIter<Q>` |
| `children_of<R>(kind, parent)` | `impl Iterator<Item = Entity>` |
| `has_relation<R>(subject, kind, target)` | `bool` |
| `get_relation_target<R>(subject, kind)` | `Option<Entity>` |

---

## 6. Что осталось на будущее

| Задача | Приоритет | Оценка |
|--------|-----------|--------|
| Обновление остальных примеров (basic, event_pipeline, perf, stages) | Low | 30min |
| Документирование макросов в rustdoc | Low | 30min |
| Default `type Events` в трейте | Low | Ждать stable Rust |
| CamelCase-именование структур | Low | Ждать paste или proc-macro |
| `App::add_sequential_system` для Variant B (сейчас через scheduler напрямую) | Low | 30min |
