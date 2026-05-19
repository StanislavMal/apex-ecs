# Apex ECS — Рефакторинг: `system!` макрос + эргономика

> **Цель:** Уменьшить boilerplate при объявлении систем с 8-10 строк до 2-6 строк,
> сохранив все текущие преимущества: compile-time access-проверку, именованную диагностику
> конфликтов, быструю компиляцию.
>
> **Принцип:** macro_rules!, не proc-macro. Никаких новых зависимостей.
>
> **Статус:** ✅ Фазы 1-3 завершены. Полный охват API AutoSystem.

---

## 1. Результаты реализации

### Что сделано

| # | Описание | Где | Статус |
|---|----------|-----|--------|
| 1 | Default `type Events = ()` в трейте | `system_param.rs` | ❌ Отменено (unstable) |
| 1b | 1-element tuple impl для WorldQuery, ResourceAccessList, EventAccessList | `query.rs:468`, `system_param.rs:262,315` | ✅ |
| 2 | `system!` макрос (macro_rules!) — Variant A + B | `system_macro.rs` (~530 строк) | ✅ |
| 3 | `ctx: Ctx` — прямой доступ к SystemContext | `system_macro.rs` | ✅ |
| 4 | `__whole: WholeWorld` — NEEDS_WHOLE_WORLD | `system_macro.rs` | ✅ |
| 5 | Event writer: `EventWriter::send()` вместо buffer+flush | `system_macro.rs` | ✅ |
| 6 | Несколько `Emit<E>` в одной системе | `system_macro.rs` | ✅ |
| 7 | `compile_error!` для нераспознанных параметров | `system_macro.rs` | ✅ |
| 8 | Реэкспорт в `lib.rs` + prelude | `apex-core/src/lib.rs`, `apex-app/src/lib.rs` | ✅ |
| 9 | Relations: Commands (`add_relation`, `remove_relation`, `add_relation_batch`) | `commands.rs` | ✅ |
| 10 | Relations: SystemContext (`query_relation`, `query_wildcard`, `children_of`, `has_relation`, `get_relation_target`) | `world.rs` | ✅ |
| 11 | Обновление `minimal.rs` — все системы на `system!` | `apex-input/examples/minimal.rs` | ✅ |

### Метрики приёмки

- ✅ `cargo test -p apex-core` — 106 тестов
- ✅ `cargo test -p apex-app` — 19 тестов
- ✅ `cargo build --example minimal` — 9 систем (5 рабочих + 4 compile-check)
- ✅ Variant A (stateless) — query, resources, events, commands, ctx, wholeworld
- ✅ Variant B (stateful) — struct + Default + `s: &mut Self`
- ✅ Multiple `&mut Vec<E>` — через `EventWriter::send()`
- ✅ Relations — read via `ctx: Ctx`, write via `cmd: Cmd`

---

## 2. Отклонения от исходного плана

### 2.1 Default `type Events = ()` — НЕ СДЕЛАНО

**Причина:** `associated_type_defaults` — unstable feature (#29661) на stable Rust 1.94.0.

**Решение:** макрос всегда генерирует `type Events = (...)` явно. Для пустого случая — `type Events = ()`.

### 2.2 Именование структур

| План | Факт |
|------|------|
| `fn movement(...)` → `struct MovementSystem` | `fn movement_system(...)` → `struct movement_system` |

**Причина:** macro_rules! не может преобразовывать snake_case → CamelCase. Имя функции = имя структуры. Атрибут `#[allow(non_camel_case_types)]` подавляет warning.

### 2.3 Variant B: `s: &mut Self` вместо `self: &mut Self`

**Причина:** `self` — keyword, не захватывается как `$slf:ident`. Пользователь выбирает ЛЮБОЕ имя (например `s`, `state`). Макрос генерирует `let s = &mut *self;` в начале `fn run`.

### 2.4 Event reader: `EventReader<T>` вместо `&[E]`

Фактический API `EventReader` не совпал с предположениями плана. Биндинг: `let events = ctx.event_reader::<E>();`, пользователь вызывает `.iter()`.

### 2.5 Event writer: `EventWriter::send()` вместо buffer+flush

Исходный план: `Vec<E>` buffer + flush. Факт: прямой `EventWriter` с `.send()`. Это устранило проблему уникальных имён буферов и поддержало несколько эмиттеров без ограничений.

### 2.6 Hygiene: `ctx` и `s` передаются как metavariables

Для обхода hygiene-проблем в рекурсивных macro_rules!:
- `ctx` передаётся как `$ctx:ident` через все уровни рекурсии
- `s` (state accessor) передаётся как `@slf: [ $slf ]` через accumulator

---

## 3. Текущий синтаксис `system!` (актуальный)

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
// Использование: app.add_system(Update, movement_system);
```

### Вариант А — с полным набором параметров

```rust
system! {
    fn full_featured(
        q: (Read<Position>, Write<Velocity>),
        keys: &Input<KeyCode>,           // resource read
        exit: &mut Exit,                 // resource write
        events: &[CollisionEvent],       // event reader
        out: &mut Vec<DamageEvent>,      // event writer (.send())
        cmd: Cmd,                        // commands (spawn, insert, relations)
        ctx: Ctx,                        // SystemContext (entity_count, relations)
        __whole: WholeWorld,             // NEEDS_WHOLE_WORLD = true
    ) {
        // ctx.entity_count() — доступен
        // ctx.children_of(ChildOf, parent) — relations read
        // cmd.add_relation(e, ChildOf, p) — relations write
        // out.send(DamageEvent { ... }) — event emit
    }
}
```

### Вариант Б — с состоянием

```rust
system! {
    struct WaveSpawner {
        wave: u32 = 1,
        enemies_spawned: u32 = 0,
    }
    fn run(
        s: &mut Self,
        cmd: Cmd,
        ctx: Ctx,
    ) {
        if s.wave <= 5 {
            cmd.spawn((Enemy, Position::default()));
            s.enemies_spawned += 1;
        }
    }
}
// Генерирует: struct + impl Default + impl AutoSystem
// Использование: app.add_system(Update, WaveSpawner::default());
```

### Полная таблица параметров

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
| В Relations: `ctx.children_of(R, parent)`, `ctx.has_relation(...)`, etc. | |
| В Relations: `cmd.add_relation(e, R, t)`, `cmd.remove_relation(e, R, t)` | |

---

## 4. Relations API

Доступны через `ctx: Ctx` (чтение) и `cmd: Cmd` (запись). Новых параметров макроса не потребовалось.

### Чтение (SystemContext)

```rust
system! {
    fn parent_system(ctx: Ctx) {
        // Все дети parent'а
        for child in ctx.children_of(ChildOf, root) { ... }

        // Запрос с компонентами
        let iter = ctx.query_relation::<ChildOf, Read<Position>>(ChildOf, parent);
        for (entity, pos) in iter { ... }

        // Wildcard — все entity с любым ChildOf
        let iter = ctx.query_wildcard::<ChildOf, Read<Health>>(ChildOf);

        // Проверки
        if ctx.has_relation(subject, Owns, target) { ... }
        if let Some(parent) = ctx.get_relation_target(entity, ChildOf) { ... }
    }
}
```

### Запись (Commands)

```rust
system! {
    fn link_system(cmd: Cmd) {
        cmd.add_relation(entity, ChildOf, parent);
        cmd.remove_relation(entity, Owns, target);
        cmd.add_relation_batch(vec![e1, e2, e3], ChildOf, root);
    }
}
```

### Реализация

| Метод Commands | Тип команды | Аллокация |
|---------------|-------------|-----------|
| `add_relation` | `Command::AddRelation` (function pointer) | Нет |
| `remove_relation` | `Command::RemoveRelation` (function pointer) | Нет |
| `add_relation_batch` | `Command::Apply(Box<dyn FnOnce>)` (замыкание) | Box |

| Метод SystemContext | Что возвращает |
|--------------------|---------------|
| `query_relation<R, Q>(kind, target)` | `RelationIter<Q>` |
| `query_wildcard<R, Q>(kind)` | `RelationIter<Q>` |
| `children_of<R>(kind, parent)` | `impl Iterator<Item = Entity>` |
| `has_relation<R>(subject, kind, target)` | `bool` |
| `get_relation_target<R>(subject, kind)` | `Option<Entity>` |

---

## 5. `sequential_system!` — проект

### Мотивация

`system!` даёт параллельную систему с `SystemContext` и `Commands`. Но есть сценарии,
где нужен эксклюзивный `&mut World`:
- **Lua-скриптинг** — движку нужен полный доступ к миру
- **`despawn_recursive`** — рекурсивное удаление требует `&mut World`
- **Массовые structural changes** — перестройка архетипов вне ASD-чанков
- **Hot-reload / сериализация** — операции над всем миром целиком

### Предлагаемый синтаксис

**Вариант А — без состояния:**
```rust
sequential_system! {
    fn cleanup(
        world: &mut World,
        events: &[DeathEvent],
        config: &CleanupConfig,
    ) {
        for ev in events.iter() {
            if config.despawn_on_death {
                world.despawn_recursive(ChildOf, ev.entity);
            }
        }
    }
}
```

**Вариант Б — с состоянием (для Lua и кешей):**
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
        s.engine.poll_hot_reload();
    }
}
```

### Что генерируется

В отличие от `system!`:
- **Нет associated types** — не нужно `type Query`, `type Resources`, `type Events`
- **Нет AutoSystem** — генерируется обычная функция `fn name(&mut World)`
- **`world: &mut World`** — распознаётся как особый параметр, маппится на параметр функции
- Ресурсы/события — биндятся через `world.resource::<T>()`, `world.event_reader::<E>()` и т.д.
- Для Variant Б: структура + `impl Default`, метод `run(&mut self, world: &mut World)`

### Что нужно для реализации

**1. Добавить convenience-методы в `World`** (аналогично `SystemContext`):
```rust
impl World {
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> { ... }
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> { ... }
    // resource() и resource_mut() уже есть, но возвращают &T / &mut T,
    // а SystemContext возвращает Res<T> / ResMut<T>
}
```

**2. `sequential_system!` macro_rules!** (~300 строк):
- Переиспользует парсинг параметров из `system!`
- `world: &mut World` → функция получает `world: &mut World`
- `name: &T` → `let name: &T = world.resource::<T>();`
- `name: &mut T` → `let name: &mut T = world.resource_mut::<T>();`
- `name: &[E]` → `let name = world.event_reader::<E>();`
- `name: &mut Vec<E>` → `let mut name = world.event_writer::<E>();`
- `name: Cmd` → `let mut name = Commands::new();` + `name.apply(world);` в конце
- `s: &mut Self` (Variant B) → `let s = &mut *self;`

**3. Для Variant Б с состоянием — нужно продумать:**
Где хранится состояние между вызовами? Варианты:
- **A: Resource** — `world.resource_mut::<LuaRunner>()` — макрос генерирует код, который достаёт состояние из ресурса
- **B: Замыкание** — генерируется `move |world: &mut World| { let s = &mut state; ... }`, где state захвачен
- **C: Trait SequentialSystem** — аналог AutoSystem: `trait SequentialSystem { fn run(&mut self, world: &mut World); }`

**Рекомендация:** начать с Variant A (stateless) — он покрывает `despawn_recursive` и cleanup. Variant Б отложить до проработки хранения состояния.

### Метрики приёмки (для V1, stateless)

- [ ] `sequential_system!` с 0, 1, 2, 3 параметрами компилируется
- [ ] `world: &mut World` + `events: &[E]` + `config: &T` — полный пример
- [ ] Сгенерированная функция имеет сигнатуру `fn name(&mut World)`
- [ ] `cmd: Cmd` — auto-apply в конце функции
- [ ] 106 тестов не сломаны

---

## 6. Что осталось на будущее

| Задача | Приоритет | Оценка |
|--------|-----------|--------|
| `sequential_system!` — Variant A (stateless) | High | 2-3h |
| `sequential_system!` — Variant B (stateful) | Medium | 2-3h (нужно решить где хранить состояние) |
| Default `type Events` в трейте | Low | Ждать stable Rust |
| CamelCase-именование структур | Low | Ждать paste или proc-macro |
| Обновление остальных примеров (basic, event_pipeline, perf) | Low | 30min |
| Документирование макроса в rustdoc | Low | 30min |
