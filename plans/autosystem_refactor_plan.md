# Расширенный `AutoSystem`: план рефакторинга

**Цель:** добавить ассоциированные типы `Resources` и `Events` в трейт `AutoSystem`,
устранив необходимость переходить на `ParSystem` ради доступа к ресурсам и событиям.
`ParSystem` остаётся `pub(crate)` — как внутренний механизм; из публичного API он исчезает.

---

## Архитектурное решение

### Почему не отдельные маркеры `Listen<E>` / `Emit<E>`

После анализа `access.rs` стало ясно: `AccessDescriptor` уже разделяет
`reads_event` и `writes_event` как два независимых вектора `Vec<TypeId>`.
Семантика событий асимметрична — читатель потребляет курсор, писатель пушит.
Значит нужны разные маркеры. Имена `Listen<E>` / `Emit<E>` лучше `Read<E>` / `Write<E>`,
потому что не создают путаницы с одноимёнными компонентными обёртками.

### Почему не переиспользовать `Read<T>` / `Write<T>` для ресурсов

`Read<T>` и `Write<T>` реализуют `WorldQuery` — а это означает логику
`fill_ids`, `matches_archetype`, `fetch_state`. Для ресурсов этой логики нет:
ресурс не живёт в архетипе. Смешивать два разных мира в один трейт — плохая идея.
Поэтому для ресурсов вводятся `Res<T>` / `ResMut<T>` как **маркеры списков**
(они уже есть как runtime-обёртки; превращаем их в маркеры через отдельный трейт).

> **Ключевое решение:** Чтобы не создавать путаницы между `Res<T>` как runtime-обёрткой
> (уже существует в `system_param.rs`) и `Res<T>` как маркером списка,
> вводим **новые трейты** `ResourceAccessList` и `EventAccessList`
> и **новые маркерные типы** `ResRead<T>` / `ResWrite<T>` / `Listen<E>` / `Emit<E>`.
> Это ломает меньше существующего кода и не смешивает две концепции.

### Итоговый дизайн трейта

```rust
pub trait AutoSystem: Send + Sync {
    type Query: WorldQuery + WorldQuerySystemAccess;
    type Resources: ResourceAccessList = ();   // default = нет доступа
    type Events:    EventAccessList    = ();   // default = нет доступа

    fn run(&mut self, ctx: SystemContext<'_>);
    fn name() -> &'static str where Self: Sized { std::any::type_name::<Self>() }
}
```

---

## Затрагиваемые файлы

| Файл | Вид изменения |
|---|---|
| `apex-core/src/system_param.rs` | Новые трейты + маркерные типы + impl |
| `apex-core/src/lib.rs` | Экспорт новых типов |
| `apex-scheduler/src/lib.rs` | `AutoSystemAdapter`, `add_auto_system_to_stage`, `ParSystem` → `pub(crate)` |
| `apex-examples/examples/basic.rs` | Миграция `PhysicsSystem` с `ParSystem` на `AutoSystem` |
| `Apex_ECS_Руководство_пользователя.md` | Раздел о системах |

---

## Патч 1 — `apex-core/src/system_param.rs`

### 1.1 Новые маркерные типы

Добавить сразу после блока `// ── Res / ResMut ───`:

```rust
// ── Маркеры для ResourceAccessList ────────────────────────────

/// Маркер: read-доступ к ресурсу T в `AutoSystem::Resources`.
///
/// Не путать с runtime-обёрткой `Res<'w, T>` — это только статическое
/// описание доступа для планировщика.
pub struct ResRead<T: Send + Sync + 'static>(PhantomData<T>);

/// Маркер: write-доступ к ресурсу T в `AutoSystem::Resources`.
pub struct ResWrite<T: Send + Sync + 'static>(PhantomData<T>);

// ── Маркеры для EventAccessList ────────────────────────────────

/// Маркер: подписка на события типа E в `AutoSystem::Events`.
///
/// Соответствует `ctx.event_reader::<E>()` внутри `run()`.
pub struct Listen<E: Send + Sync + 'static>(PhantomData<E>);

/// Маркер: публикация событий типа E в `AutoSystem::Events`.
///
/// Соответствует `ctx.event_writer::<E>()` внутри `run()`.
pub struct Emit<E: Send + Sync + 'static>(PhantomData<E>);
```

### 1.2 Трейт `ResourceAccessList`

```rust
// ── ResourceAccessList ─────────────────────────────────────────

/// Статическое описание доступа к ресурсам — используется в `AutoSystem::Resources`.
///
/// Реализован для:
/// - `()` — нет доступа к ресурсам (дефолт)
/// - `ResRead<T>` — read-доступ к ресурсу T
/// - `ResWrite<T>` — write-доступ к ресурсу T
/// - кортежи из вышеперечисленных (до 8 элементов)
pub trait ResourceAccessList {
    fn resource_accesses() -> crate::access::AccessDescriptor;
}

impl ResourceAccessList for () {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new()
    }
}

impl<T: Send + Sync + 'static> ResourceAccessList for ResRead<T> {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().read::<T>()
    }
}

impl<T: Send + Sync + 'static> ResourceAccessList for ResWrite<T> {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().write::<T>()
    }
}
```

Добавить поддержку кортежей через макрос (вставить после impl'ов выше):

```rust
macro_rules! impl_resource_access_list_tuple {
    ( $($R:ident),+ ) => {
        impl< $($R: ResourceAccessList),+ > ResourceAccessList for ( $($R,)+ ) {
            fn resource_accesses() -> crate::access::AccessDescriptor {
                crate::access::AccessDescriptor::new()
                    $( .merge(&$R::resource_accesses()) )+
            }
        }
    };
}

impl_resource_access_list_tuple!(A, B);
impl_resource_access_list_tuple!(A, B, C);
impl_resource_access_list_tuple!(A, B, C, D);
impl_resource_access_list_tuple!(A, B, C, D, E);
impl_resource_access_list_tuple!(A, B, C, D, E, F);
impl_resource_access_list_tuple!(A, B, C, D, E, F, G);
impl_resource_access_list_tuple!(A, B, C, D, E, F, G, H);
```

### 1.3 Трейт `EventAccessList`

```rust
// ── EventAccessList ────────────────────────────────────────────

/// Статическое описание доступа к событиям — используется в `AutoSystem::Events`.
///
/// Реализован для:
/// - `()` — нет доступа к событиям (дефолт)
/// - `Listen<E>` — подписка на события E (read_event)
/// - `Emit<E>`   — публикация событий E  (write_event)
/// - кортежи из вышеперечисленных (до 8 элементов)
pub trait EventAccessList {
    fn event_accesses() -> crate::access::AccessDescriptor;
}

impl EventAccessList for () {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new()
    }
}

impl<E: Send + Sync + 'static> EventAccessList for Listen<E> {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().read_event::<E>()
    }
}

impl<E: Send + Sync + 'static> EventAccessList for Emit<E> {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().write_event::<E>()
    }
}

macro_rules! impl_event_access_list_tuple {
    ( $($E:ident),+ ) => {
        impl< $($E: EventAccessList),+ > EventAccessList for ( $($E,)+ ) {
            fn event_accesses() -> crate::access::AccessDescriptor {
                crate::access::AccessDescriptor::new()
                    $( .merge(&$E::event_accesses()) )+
            }
        }
    };
}

impl_event_access_list_tuple!(A, B);
impl_event_access_list_tuple!(A, B, C);
impl_event_access_list_tuple!(A, B, C, D);
impl_event_access_list_tuple!(A, B, C, D, E);
impl_event_access_list_tuple!(A, B, C, D, E, F);
impl_event_access_list_tuple!(A, B, C, D, E, F, G);
impl_event_access_list_tuple!(A, B, C, D, E, F, G, H);
```

### 1.4 Обновление трейта `AutoSystem`

Заменить текущее определение:

```rust
// ДО:
pub trait AutoSystem: Send + Sync {
    type Query: WorldQuery + WorldQuerySystemAccess;
    fn run(&mut self, ctx: crate::world::SystemContext<'_>);
    fn name() -> &'static str where Self: Sized {
        std::any::type_name::<Self>()
    }
}

// ПОСЛЕ:
pub trait AutoSystem: Send + Sync {
    /// Компонентный запрос — из него выводится часть `AccessDescriptor`.
    type Query: WorldQuery + WorldQuerySystemAccess;

    /// Ресурсы, которые нужны системе.
    ///
    /// По умолчанию пуст — обратная совместимость сохраняется.
    ///
    /// # Пример
    /// ```ignore
    /// type Resources = (ResRead<DeltaTime>, ResWrite<PhysicsConfig>);
    /// ```
    type Resources: ResourceAccessList = ();

    /// События, которые система читает или пишет.
    ///
    /// По умолчанию пуст — обратная совместимость сохраняется.
    ///
    /// # Пример
    /// ```ignore
    /// type Events = (Listen<DamageEvent>, Emit<DeathEvent>);
    /// ```
    type Events: EventAccessList = ();

    fn run(&mut self, ctx: crate::world::SystemContext<'_>);

    fn name() -> &'static str where Self: Sized {
        std::any::type_name::<Self>()
    }
}
```

> **Примечание по стабильности:** associated type defaults (`= ()`) требуют
> `#![feature(associated_type_defaults)]` на stable Rust до 1.80.
> На Rust ≥ 1.80 (stabilized in 1.80) это работает без feature флага.
> Если нужна поддержка более старых версий — использовать helper-трейт
> (см. Альтернативу в конце документа).

---

## Патч 2 — `apex-core/src/lib.rs`

Добавить в pub use блок (и в `prelude`):

```rust
// В основном pub use блоке:
pub use system_param::{
    Res, ResMut, EventReader, EventWriter,
    ResRead, ResWrite, Listen, Emit,          // ← новые
    ResourceAccessList, EventAccessList,       // ← новые трейты
    WorldQuerySystemAccess, AutoSystem,
};

// В pub mod prelude:
pub use crate::system_param::{
    Res, ResMut, EventReader, EventWriter,
    ResRead, ResWrite, Listen, Emit,           // ← новые
    WorldQuerySystemAccess, AutoSystem,
};
```

---

## Патч 3 — `apex-scheduler/src/lib.rs`

### 3.1 `AutoSystemAdapter` — учёт Resources и Events

Сейчас `AutoSystemAdapter::access()` вызывает только `S::Query::system_access()`.
Нужно слить три источника:

```rust
// ДО:
impl<S: AutoSystem + 'static> ParSystem for AutoSystemAdapter<S> {
    fn access() -> AccessDescriptor where Self: Sized {
        S::Query::system_access()
    }
    // ...
}

// ПОСЛЕ:
impl<S: AutoSystem + 'static> ParSystem for AutoSystemAdapter<S> {
    fn access() -> AccessDescriptor where Self: Sized {
        S::Query::system_access()
            .merge(&S::Resources::resource_accesses())
            .merge(&S::Events::event_accesses())
    }
    // ...
}
```

### 3.2 `add_auto_system_to_stage` — аналогично

```rust
// ДО:
let access = S::Query::system_access();

// ПОСЛЕ:
let access = S::Query::system_access()
    .merge(&S::Resources::resource_accesses())
    .merge(&S::Events::event_accesses());
```

Та же замена в `add_auto_system`, `add_startup_auto_system` —
они делегируют в `add_auto_system_to_stage`, так что достаточно одного места.

### 3.3 `ParSystem` → `pub(crate)`

```rust
// ДО:
pub trait ParSystem: Send + Sync {
    fn access() -> AccessDescriptor where Self: Sized;
    fn run(&mut self, ctx: SystemContext<'_>);
    fn name() -> &'static str where Self: Sized { std::any::type_name::<Self>() }
}

// ПОСЛЕ:
pub(crate) trait ParSystem: Send + Sync {
    fn access() -> AccessDescriptor where Self: Sized;
    fn run(&mut self, ctx: SystemContext<'_>);
    fn name() -> &'static str where Self: Sized { std::any::type_name::<Self>() }
}
```

> Все `impl ParSystem` в примерах (`basic.rs`, `perf.rs`, бенчи) нужно
> мигрировать на `AutoSystem`. Внутри планировщика `ParSystem` остаётся
> как нижний слой — `AutoSystemAdapter` по-прежнему его реализует.

### 3.4 Убрать `ParSystem` из pub-экспорта `apex-scheduler`

В `apex-scheduler/src/lib.rs` найти строки вида:

```rust
pub use ...::{Scheduler, ParSystem, StageLabel};
```

Изменить на:

```rust
pub use ...::{Scheduler, StageLabel};
// ParSystem больше не экспортируется
```

---

## Патч 4 — `apex-examples/examples/basic.rs`

### Миграция `PhysicsSystem`

```rust
// ДО:
use apex_scheduler::{Scheduler, ParSystem, StageLabel};
use apex_core::access::AccessDescriptor;

struct PhysicsSystem;

impl ParSystem for PhysicsSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new()
            .read::<PhysicsConfig>()
            .read::<Mass>()
            .write::<Velocity>()
            .write::<Position>()
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        let cfg = ctx.resource::<PhysicsConfig>();
        // ...
    }
}

// ПОСЛЕ:
use apex_scheduler::{Scheduler, StageLabel};
use apex_core::prelude::*; // ResRead теперь в prelude

struct PhysicsSystem;

impl AutoSystem for PhysicsSystem {
    type Query     = (Read<Mass>, Write<Velocity>, Write<Position>);
    type Resources = (ResRead<PhysicsConfig>, ResRead<DeltaTime>);
    // Events не нужны — type Events = () по умолчанию

    fn run(&mut self, ctx: SystemContext<'_>) {
        let cfg = ctx.resource::<PhysicsConfig>();
        // ctx.query::<Self::Query>() работает как прежде
        // ...
    }
}
```

### Регистрация

```rust
// ДО:
sched.add_par_system_to_stage("physics", PhysicsSystem, StageLabel::Update);

// ПОСЛЕ:
sched.add_auto_system_to_stage("physics", PhysicsSystem, StageLabel::Update);
```

### Миграция `HealthClampSystem`

```rust
// ДО:
impl ParSystem for HealthClampSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().write::<Health>()
    }
    fn run(&mut self, ctx: SystemContext<'_>) { ... }
}

// ПОСЛЕ:
impl AutoSystem for HealthClampSystem {
    type Query = Write<Health>;
    // Resources = (), Events = () — по умолчанию

    fn run(&mut self, ctx: SystemContext<'_>) { ... }
}
```

---

## Патч 5 — Бенчи и тесты (`apex-bench`, `perf.rs`)

В `apex-bench/benches/benchmark.rs` и смежных файлах все `impl ParSystem`
без ресурсов/событий мигрировать механически:

```rust
// Шаблон миграции для систем без ресурсов:
// ДО:
impl ParSystem for FooSys {
    fn access() -> AccessDescriptor { AccessDescriptor::new().write::<X>() }
    fn run(&mut self, ctx: SystemContext<'_>) { ... }
}

// ПОСЛЕ:
impl AutoSystem for FooSys {
    type Query = Write<X>;
    fn run(&mut self, ctx: SystemContext<'_>) { ... }
}
```

---

## Патч 6 — Документация

В `Apex_ECS_Руководство_пользователя.md` обновить раздел «Системы»:

**Убрать** пример `ParSystem` из туториального пути (перенести в advanced/appendix).

**Обновить** таблицу иерархии API:

```
AutoSystem   — рекомендуемый способ (Query + Resources + Events)
FnParSystem  — замыкание с явным AccessDescriptor (продвинутый)
Sequential   — полный &mut World (структурные изменения)
```

**Добавить** пример с ресурсами и событиями:

```rust
struct PhysicsSystem;

impl AutoSystem for PhysicsSystem {
    type Query     = (Read<Mass>, Write<Velocity>, Write<Position>);
    type Resources = ResRead<DeltaTime>;          // один ресурс — без кортежа
    type Events    = Emit<CollisionEvent>;

    fn run(&mut self, ctx: SystemContext<'_>) {
        let dt = ctx.resource::<DeltaTime>().0;
        let mut writer = ctx.event_writer::<CollisionEvent>();

        ctx.query::<Self::Query>().for_each(|entity, (mass, vel, pos)| {
            vel.y -= 9.8 * mass.0 * dt;
            pos.x += vel.x * dt;
            pos.y += vel.y * dt;
            if pos.y < 0.0 {
                writer.send(CollisionEvent { entity, normal: Vec2::Y });
            }
        });
    }
}

sched.add_auto_system("physics", PhysicsSystem);
```

---

## Обработка граничного случая: stable Rust < 1.80

Если проект должен поддерживать Rust < 1.80 без associated type defaults, 
используется helper-трейт вместо default:

```rust
// В system_param.rs добавить sealed helper:
pub trait AutoSystemAccess: AutoSystem {
    fn full_access() -> AccessDescriptor;
}

// Бланкетный impl — выводит access из всех трёх источников:
impl<S: AutoSystem> AutoSystemAccess for S
where
    S::Resources: ResourceAccessList,
    S::Events:    EventAccessList,
{
    fn full_access() -> AccessDescriptor {
        S::Query::system_access()
            .merge(&S::Resources::resource_accesses())
            .merge(&S::Events::event_accesses())
    }
}
```

В `AutoSystemAdapter` использовать `S::full_access()` вместо прямого merge.
Трейт `AutoSystem` при этом **не** использует ассоциированные типы с defaults —
вместо этого `Resources` и `Events` остаются обязательными, но прячутся за
вспомогательный макрос `auto_system!` или `#[derive(AutoSystem)]`.

> Это усложняет API — рекомендуется **требовать Rust ≥ 1.80**
> (вышел май 2024, на момент написания — стабилен почти год).

---

## Порядок выполнения

```
Патч 1  →  apex-core/src/system_param.rs  (новые типы и трейты)
Патч 2  →  apex-core/src/lib.rs           (экспорт)
Патч 3  →  apex-scheduler/src/lib.rs      (адаптер + pub(crate))
Патч 4  →  apex-examples/examples/basic.rs
Патч 5  →  apex-bench/**  +  apex-examples/examples/perf.rs
Патч 6  →  Apex_ECS_Руководство_пользователя.md
```

Каждый патч компилируется и тестируется отдельно:

```bash
# После Патча 1+2:
cargo check -p apex-core

# После Патча 3:
cargo check -p apex-scheduler

# После Патчей 4+5:
cargo test --workspace
cargo run -p apex-examples --example basic

# Финальная проверка:
cargo test --workspace --features parallel
cargo bench -p apex-bench
```

---

## Итог: что меняется для пользователя

| Сценарий | До | После |
|---|---|---|
| Только компоненты | `AutoSystem { type Query = ... }` | Без изменений |
| Компоненты + ресурс | `ParSystem { fn access() { ... } }` | `AutoSystem { type Resources = ResRead<T> }` |
| Компоненты + события | `ParSystem { fn access() { ... } }` | `AutoSystem { type Events = Listen<E> }` |
| Всё вместе | `ParSystem` с длинным access | `AutoSystem` с тремя ассоциированными типами |
| Динамический access | `ParSystem` / `FnParSystem` | `FnParSystem` (остаётся) |

`ParSystem` исчезает из публичного API. `FnParSystem` (`add_fn_par_system`) 
остаётся для продвинутых сценариев с динамически конструируемым дескриптором.
