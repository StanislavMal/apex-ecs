# Патч: унификация и улучшение API Apex ECS

**Версия:** 1.0  
**Дата:** 2026-04-28  
**Затронутые крейты:** `apex-core`, `apex-scripting`  
**Обратная совместимость:** не требуется (закрытый репозиторий)

---

## Содержание

1. [П-1 — `spawn_bundle` → `spawn`](#п-1--spawn_bundle--spawn)
2. [П-2 — `EntityRef` — единый интерфейс операций над entity](#п-2--entityref--единый-интерфейс-операций-над-entity)
3. [П-3 — Унификация итерации в `Query`](#п-3--унификация-итерации-в-query)
4. [П-4 — `DeferredQueue` поглощается `Commands`](#п-4--deferredqueue-поглощается-commands)
5. [П-5 — `TemplateParams` — типизированные ключи](#п-5--templateparams--типизированные-ключи)
6. [П-6 — Единая точка регистрации компонентов](#п-6--единая-точка-регистрации-компонентов)
7. [Итоговая картина нового API](#итоговая-картина-нового-api)

---

## П-1 — `spawn_bundle` → `spawn`

**Крейт:** `apex-core`  
**Файлы:** `world.rs`, `commands.rs`

### Проблема

`spawn_bundle` — термин из старого Bevy API (до 0.9), который сам Bevy выпилил, потому что «bundle» как концепция просочилась в пользовательский код без необходимости. В Apex bundle — это деталь реализации (трейт для кортежей), а не концепция которую должен знать пользователь. Разработчик просто хочет «создать entity с этими компонентами».

Дополнительно: сейчас сосуществуют `world.spawn()` (возвращает `EntityBuilder`) и `world.spawn_bundle()` (возвращает `Entity`) — два метода делающих одно и то же разными способами. Это сбивает.

### Решение

Единственный метод `spawn` принимает и пустой вызов и кортеж:

```rust
// Было:
let e = world.spawn_empty();
let e = world.spawn_bundle((Position { x: 0.0 }, Velocity { x: 1.0 }));
let e = world.spawn().insert(Pos).insert(Vel).id();

// Стало:
let e = world.spawn(());                              // пустая entity
let e = world.spawn((Position { x: 0.0 }, Velocity { x: 1.0 }));
let e = world.spawn((Pos, Vel));                      // то же, кратко
```

`EntityBuilder` и `spawn_empty` удаляются. Если нужно добавить компонент после создания — `world.entity(e).insert(...)` (см. П-2).

### Реализация

В `world.rs` — добавить `spawn<B: Bundle>`, убрать `spawn_bundle` и `spawn_empty`:

```rust
// БЫЛО:
pub fn spawn_empty(&mut self) -> Entity { ... }
pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Entity { ... }
pub fn spawn(&mut self) -> EntityBuilder<'_> { ... }

// СТАЛО — один метод принимающий любой Bundle (включая пустой кортеж):
pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
    let ids = bundle.component_ids(&mut self.registry);
    if ids.is_empty() {
        // Быстрый путь для пустой entity (spawn(()))
        let entity = self.entities.allocate();
        let row    = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
        self.entities.set_location(entity, EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        });
        return entity;
    }
    // Обычный путь — как старый spawn_bundle
    let archetype_id = self.get_or_create_archetype(&ids);
    let entity       = self.entities.allocate();
    let row          = self.archetypes[archetype_id.0 as usize].entities.len();
    let tick         = self.current_tick;
    self.archetypes[archetype_id.0 as usize].entities.push(entity);
    bundle.write_into(self, archetype_id, row, tick);
    self.entities.set_location(entity, EntityLocation { archetype_id, row: row as u32 });
    entity
}
```

Для пустого кортежа `()` нужна реализация `Bundle`:

```rust
// Добавить в world.rs рядом с impl_bundle!:
impl Bundle for () {
    fn component_ids(&self, _: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
        SmallVec::new()
    }
    fn write_into(self, _: &mut World, _: ArchetypeId, _: usize, _: Tick) {}
}
```

В `commands.rs` — аналогично:

```rust
// БЫЛО:
pub fn spawn_bundle<B: Bundle + Send + 'static>(&mut self, bundle: B) { ... }

// СТАЛО:
pub fn spawn<B: Bundle + Send + 'static>(&mut self, bundle: B) { ... }
```

`EntityBuilder` удаляется целиком из `world.rs` — его роль берёт на себя `EntityRef` из П-2.

---

## П-2 — `EntityRef` — единый интерфейс операций над entity

**Крейт:** `apex-core`  
**Файлы:** `world.rs` (новый тип `EntityRef`)

### Проблема

Операции над конкретной entity разбросаны по `World` как свободные методы с entity как первым аргументом:

```rust
world.insert(entity, Position { x: 0.0 });
world.remove::<Velocity>(entity);
world.despawn(entity);
world.get::<Health>(entity);
world.get_mut::<Health>(entity);
world.add_relation(entity, ChildOf, parent);
world.has_relation(entity, ChildOf, parent);
```

Это неудобно по нескольким причинам. `entity` всегда первым аргументом — повторение. Нет одного места где смотреть «что можно сделать с entity». Сложно понять из сигнатуры `world.get::<T>(e)` что `e` — entity, а не что-то ещё.

### Решение

`EntityRef<'w>` — тонкий фасад с `&mut World` и `Entity` внутри. Получается через `world.entity(e)`. Возвращает `None` если entity мертва.

```rust
// После изменения:
world.entity(e).insert(Position { x: 0.0 });
world.entity(e).remove::<Velocity>();
world.entity(e).despawn();
world.entity(e).get::<Health>();
world.entity(e).get_mut::<Health>();
world.entity(e).add_relation(ChildOf, parent);
world.entity(e).has_relation(ChildOf, parent);

// Чейнинг (entity builder паттерн при спавне):
let e = world.spawn(())
    .entity(|e| world.entity(e))  // не нужен, spawn уже возвращает Entity
```

Так как `spawn` теперь возвращает `Entity`, получить `EntityRef` сразу после спавна просто:

```rust
let e = world.spawn((Position::default(), Velocity::default()));
world.entity(e).add_relation(ChildOf, parent);
world.entity(e).insert(Health { current: 100.0 });
```

### Реализация

Добавить в `world.rs`:

```rust
/// Тонкий фасад для операций над одной entity.
///
/// Получается через [`World::entity`]. Все методы no-op если entity мертва.
///
/// # Пример
/// ```ignore
/// let e = world.spawn((Position::default(),));
/// world.entity(e)
///      .insert(Velocity { x: 1.0, y: 0.0 })
///      .add_relation(ChildOf, parent);
/// ```
pub struct EntityRef<'w> {
    world:  &'w mut World,
    entity: Entity,
}

impl<'w> EntityRef<'w> {
    /// Вернуть идентификатор entity.
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Проверить, жива ли entity.
    pub fn is_alive(&self) -> bool {
        self.world.entities.is_alive(self.entity)
    }

    /// Вставить компонент в entity.
    pub fn insert<T: Component>(&mut self, component: T) -> &mut Self {
        self.world.insert(self.entity, component);
        self
    }

    /// Удалить компонент типа T из entity.
    pub fn remove<T: Component>(&mut self) -> bool {
        self.world.remove::<T>(self.entity)
    }

    /// Деспавнить entity.
    pub fn despawn(&mut self) -> bool {
        self.world.despawn(self.entity)
    }

    /// Прочитать компонент T.
    pub fn get<T: Component>(&self) -> Option<&T> {
        self.world.get::<T>(self.entity)
    }

    /// Прочитать компонент T мутабельно.
    pub fn get_mut<T: Component>(&mut self) -> Option<&mut T> {
        self.world.get_mut::<T>(self.entity)
    }

    /// Добавить relation между этой entity и target.
    pub fn add_relation<R: RelationKind>(&mut self, kind: R, target: Entity) -> &mut Self {
        self.world.add_relation(self.entity, kind, target);
        self
    }

    /// Удалить relation.
    pub fn remove_relation<R: RelationKind>(&mut self, kind: R, target: Entity) -> &mut Self {
        self.world.remove_relation(self.entity, kind, target);
        self
    }

    /// Проверить наличие relation.
    pub fn has_relation<R: RelationKind>(&self, kind: R, target: Entity) -> bool {
        self.world.has_relation(self.entity, kind, target)
    }
}

impl World {
    /// Получить `EntityRef` для операций над конкретной entity.
    ///
    /// Возвращает `EntityRef` даже если entity мертва — методы будут no-op.
    /// Для явной проверки используйте [`EntityRef::is_alive`].
    pub fn entity(&mut self, entity: Entity) -> EntityRef<'_> {
        EntityRef { world: self, entity }
    }
}
```

Старые методы `world.insert(e, ...)`, `world.remove::<T>(e)`, `world.despawn(e)` и `world.get::<T>(e)` **остаются** — они нужны внутри систем и в других местах где `world` передаётся напрямую. `EntityRef` — удобный фасад поверх них, а не замена.

---

## П-3 — Унификация итерации в `Query`

**Крейт:** `apex-core`  
**Файл:** `world.rs`, `query.rs`

### Проблема

Сейчас два метода итерации с разными сигнатурами замыкания:

```rust
// Метод 1 — entity есть:
query.for_each(|entity, (vel, pos)| { ... });

// Метод 2 — entity нет, и метод называется по-другому:
ctx.query::<...>().for_each_component(|(vel, pos)| { ... });
```

Пользователь вынужден помнить два имени и два контракта. Если понадобилась entity — нужно переключить метод целиком, а не просто добавить параметр. В `SystemContext` `for_each_component` существует параллельно с `for_each` — запутывает.

### Решение

Единственный метод `for_each` всегда передаёт `(Entity, components)`. Если entity не нужна — игнорируем через `_`:

```rust
// После изменения — один метод, одна сигнатура:
query.for_each(|entity, (vel, pos)| {
    pos.x += vel.x;
});

// Entity не нужна — просто _:
query.for_each(|_, (vel, pos)| {
    pos.x += vel.x;
});

// Параллельная версия — та же сигнатура:
query.par_for_each(|_, (vel, pos)| {
    pos.x += vel.x;
});
```

`for_each_component` и `par_for_each_component` **удаляются**.

> **Оптимизация (2026-04-28):** После бенчмарков обнаружена регрессия +87% на fragmented\_iter.
> Причина: `for_each` читает `entities[row]` на каждой итерации через bound-checked Vec indexing.
> Исправление: `&entities[..a.len].iter().enumerate()` — один bound check на архетип,
> последовательный итератор без проверки границ на элемент.
> API не изменился: `for_each(|_, item)| { ... }` остаётся единым методом.

### Реализация

В `Query` (`world.rs`):

```rust
// БЫЛО:
pub fn for_each<F>(&self, mut f: F)
where
    F: FnMut(Entity, Q::Item<'_>),
{ ... }

pub fn for_each_component<F>(&self, mut f: F)
where
    F: FnMut(Q::Item<'_>),
{ ... }

// СТАЛО — оставляем только for_each, удаляем for_each_component:
pub fn for_each<F>(&self, mut f: F)
where
    F: FnMut(Entity, Q::Item<'_>),
{ ... }
// for_each_component — УДАЛИТЬ
```

В `SystemContext` (`world.rs`):

```rust
// БЫЛО:
pub fn for_each_component<Q, F>(&self, f: F) where ... { ... }
pub fn par_for_each_component<Q, F>(&self, f: F) where ... { ... }

// СТАЛО — удалить оба, оставить только for_each и par_for_each:
// (for_each уже есть в SystemContext и передаёт entity)
```

В тестах и примерах — заменить все вхождения `for_each_component` на `for_each`:

```rust
// Было:
ctx.query::<(Read<Velocity>, Write<Position>)>()
    .for_each_component(|(vel, pos)| {
        pos.x += vel.x;
    });

// Стало:
ctx.query::<(Read<Velocity>, Write<Position>)>()
    .for_each(|_, (vel, pos)| {
        pos.x += vel.x;
    });
```

---

## П-4 — `DeferredQueue` поглощается `Commands`

**Крейт:** `apex-core`  
**Файлы:** `commands.rs`, `world.rs`

### Проблема

`DeferredQueue` — публичный тип с subset функциональности `Commands`:

```rust
// DeferredQueue (сейчас):
let mut queue = DeferredQueue::new();
queue.despawn(entity);
queue.remove_raw(entity, component_id);
queue.apply(&mut world);

// Commands (сейчас):
let mut cmds = Commands::new();
cmds.despawn(entity);
cmds.remove_raw(entity, component_id); // тоже есть!
cmds.apply(&mut world);
```

Два типа делающих одно и то же. Разработчик вынужден знать когда использовать какой. Документация вынуждена объяснять разницу. `DeferredQueue` описан как «для систем где тип компонента неизвестен статически» — но `Commands` это тоже умеет через `remove_raw`.

### Решение

`DeferredQueue` удаляется. Все его уникальные методы (если есть) переносятся в `Commands`. Везде где использовался `DeferredQueue` — используется `Commands`.

### Реализация

Проверить что в `Commands` есть всё что было в `DeferredQueue`:

```rust
// В Commands убедиться что есть:
pub fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
    self.queue.push(Command::Remove { entity, component_id });
}
```

Этот метод в `Commands` уже присутствует (виден в коде — `Command::Remove { entity, component_id }`). Значит удаление `DeferredQueue` не требует добавления новых методов — только удаление типа и замена всех его использований на `Commands`.

```rust
// БЫЛО:
let mut queue = DeferredQueue::new();
queue.despawn(entity);
queue.remove_raw(entity, component_id);
queue.apply(&mut world);

// СТАЛО:
let mut cmds = Commands::new();
cmds.despawn(entity);
cmds.remove_raw(entity, component_id);
cmds.apply(&mut world);
```

`DeferredQueue` — **удалить из публичного API** (`pub struct DeferredQueue` → убрать или сделать `pub(crate)`).

---

## П-5 — `TemplateParams` — типизированные ключи

**Крейт:** `apex-core`  
**Файл:** `template.rs`

### Проблема

`TemplateParams` использует строковые ключи и стирание типов:

```rust
// Сейчас:
let params = TemplateParams::new()
    .with("hp", 150.0_f32)    // строка как ключ — опечатка = рантайм ошибка
    .with("speed", 10.0_f32)
    .build();

// Чтение:
let hp = params.get::<f32>("hp").copied().unwrap_or(self.base_hp);
//                  ^          ^
//                  тип надо указать вручную, а строку можно написать неверно
```

Ошибки в именах параметров или несовпадение типов проявляются только в рантайме. Это особенно болезненно когда шаблон вызывается в одном месте, а его параметры определяются в другом.

### Решение

Параметр как маркерный тип — ключ и тип значения связаны статически:

```rust
// Определяем параметры как типы (рядом с шаблоном):
struct HpParam;
impl TemplateParam for HpParam {
    type Value = f32;
}

struct SpeedParam;
impl TemplateParam for SpeedParam {
    type Value = f32;
}

// Использование — тип выводится из TemplateParam::Value:
let params = TemplateParams::new()
    .set::<HpParam>(150.0)    // тип f32 выводится из HpParam
    .set::<SpeedParam>(10.0);

// Чтение внутри шаблона:
let hp = params.get::<HpParam>().copied().unwrap_or(self.base_hp);
// Тип f32 известен на этапе компиляции — unwrap/copy работают без указания типа
```

Опечатка в имени параметра — ошибка компилятора. Несовпадение типов — ошибка компилятора.

### Реализация

В `template.rs`:

```rust
// ДОБАВИТЬ — трейт для типизированного параметра шаблона:

/// Маркерный трейт: связывает имя параметра (тип) с типом значения.
///
/// # Пример
/// ```ignore
/// struct HpParam;
/// impl TemplateParam for HpParam { type Value = f32; }
/// ```
pub trait TemplateParam: 'static {
    type Value: Send + Sync + 'static;
}

// ИЗМЕНИТЬ TemplateParams:

use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Типизированные параметры для инстанцирования шаблона entity.
///
/// Ключи — типы реализующие [`TemplateParam`]. Ошибки в именах параметров
/// и несовпадение типов обнаруживаются на этапе компиляции.
pub struct TemplateParams {
    data: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl TemplateParams {
    pub fn new() -> Self {
        Self { data: HashMap::default() }
    }

    /// Установить значение параметра.
    pub fn set<P: TemplateParam>(mut self, value: P::Value) -> Self {
        self.data.insert(TypeId::of::<P>(), Box::new(value));
        self
    }

    /// Получить значение параметра.
    pub fn get<P: TemplateParam>(&self) -> Option<&P::Value> {
        self.data
            .get(&TypeId::of::<P>())
            .and_then(|v| v.downcast_ref::<P::Value>())
    }
}

impl Default for TemplateParams {
    fn default() -> Self { Self::new() }
}
```

Старый `TemplateParams` со строковыми ключами — **удалить**. Обновить `EntityTemplate::spawn` во всех реализациях:

```rust
// Было (в MonsterTemplate):
fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
    let hp = params.get::<f32>("hp").copied().unwrap_or(self.base_hp);
    ...
}

// Стало:
fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
    let hp = params.get::<HpParam>().copied().unwrap_or(self.base_hp);
    ...
}
```

Для `Commands::spawn_from_template` и `spawn_template` сигнатура не меняется — `TemplateParams` передаётся как есть:

```rust
cmds.spawn_from_template("Monster", TemplateParams::new().set::<HpParam>(150.0));
```

---

## П-6 — Единая точка регистрации компонентов

**Крейты:** `apex-core`, `apex-scripting`  
**Файлы:** `world.rs`, `script_engine.rs`

### Проблема

Регистрация компонента для скриптинга требует двух отдельных вызовов:

```rust
// Сейчас — два вызова, легко забыть второй:
world.register_component::<Position>();         // 1. в ECS
engine.register_component::<Position>(&world); // 2. в скрипт-движке

// Если забыть второй — скрипт молча игнорирует компонент
// (только предупреждение в логе, не ошибка компиляции)
```

Аналогично для ресурсов и событий:

```rust
world.resources.insert(Gravity(9.8));
engine.register_resource::<Gravity>();  // отдельный вызов

world.add_event::<PlayerDied>();
engine.register_event::<PlayerDied>();  // отдельный вызов
```

### Решение

`WorldScriptingExt` — extension trait добавляющий методы к `World`, которые регистрируют и в ECS и в движке одновременно. `ScriptEngine` передаётся как параметр.

```rust
// После изменения — один вызов:
world.register_scriptable::<Position>(&mut engine);
world.register_scriptable_resource::<Gravity>(&mut engine);
world.register_scriptable_event::<PlayerDied>(&mut engine);

// Полный пример инициализации:
let mut world  = World::new();
let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));

world.register_scriptable::<Position>(&mut engine);
world.register_scriptable::<Velocity>(&mut engine);
world.register_scriptable::<Health>(&mut engine);

world.resources.insert(Gravity(9.8));
world.register_scriptable_resource::<Gravity>(&mut engine);
```

### Реализация

В `apex-scripting/src/lib.rs` — добавить extension trait:

```rust
use apex_core::world::World;
use crate::script_engine::ScriptEngine;
use crate::registrar::ScriptableRegistrar;

/// Extension trait: регистрирует типы одновременно в World и ScriptEngine.
///
/// Устраняет необходимость двойной регистрации:
/// ```ignore
/// // Было:
/// world.register_component::<Position>();
/// engine.register_component::<Position>(&world);
///
/// // Стало:
/// world.register_scriptable::<Position>(&mut engine);
/// ```
pub trait WorldScriptingExt {
    /// Зарегистрировать компонент в ECS и в ScriptEngine.
    fn register_scriptable<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + apex_core::component::Component;

    /// Зарегистрировать ресурс в ScriptEngine (ресурс должен уже быть вставлен в world).
    fn register_scriptable_resource<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static;

    /// Зарегистрировать событие в ECS и в ScriptEngine.
    fn register_scriptable_event<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static;
}

impl WorldScriptingExt for World {
    fn register_scriptable<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + apex_core::component::Component,
    {
        self.register_component::<T>();
        engine.register_component::<T>(self);
    }

    fn register_scriptable_resource<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static,
    {
        // Ресурс уже вставлен пользователем через world.resources.insert(...)
        engine.register_resource::<T>();
    }

    fn register_scriptable_event<T>(&mut self, engine: &mut ScriptEngine)
    where
        T: ScriptableRegistrar + Send + Sync + 'static,
    {
        self.add_event::<T>();
        engine.register_event::<T>();
    }
}
```

Экспортируется автоматически — трейт определён прямо в `lib.rs` как `pub trait`.

Старые методы `engine.register_component`, `engine.register_resource`, `engine.register_event` **остаются** — они нужны когда `World` недоступен напрямую. `WorldScriptingExt` — удобный фасад для типичного случая.

---

## Итоговая картина нового API

### Инициализация мира (типичный проект)

```rust
use apex_core::prelude::*;
use apex_scripting::{ScriptEngine, WorldScriptingExt};

let mut world  = World::new();
let mut engine = ScriptEngine::with_dir(Path::new("scripts/"));

// Компоненты — один вызов вместо двух:
world.register_scriptable::<Position>(&mut engine);
world.register_scriptable::<Velocity>(&mut engine);
world.register_scriptable::<Health>(&mut engine);

// Ресурсы:
world.resources.insert(Gravity(9.8));
world.register_scriptable_resource::<Gravity>(&mut engine);

// События:
world.register_scriptable_event::<PlayerDied>(&mut engine);
```

### Спавн и работа с entity

```rust
// Спавн с компонентами:
let player = world.spawn((
    Position { x: 0.0, y: 0.0 },
    Health { current: 100.0, max: 100.0 },
    Velocity::default(),
));

// Пустая entity:
let marker = world.spawn(());

// Операции через EntityRef — цепочка:
world.entity(player)
    .insert(Armor { value: 10.0 })
    .add_relation(ChildOf, root_entity);

// Проверки:
let hp = world.entity(player).get::<Health>().unwrap();
world.entity(player).despawn();
```

### Системы и итерация

```rust
// Один метод — entity всегда доступна:
Query::<(Read<Health>, Write<Position>)>::new(&world)
    .for_each(|entity, (hp, pos)| {
        if hp.current <= 0.0 {
            cmds.despawn(entity);
        }
        pos.y -= 9.8 * dt;
    });

// Entity не нужна — просто _:
Query::<(Read<Velocity>, Write<Position>)>::new(&world)
    .for_each(|_, (vel, pos)| {
        pos.x += vel.x * dt;
        pos.y += vel.y * dt;
    });
```

### Шаблоны с типизированными параметрами

```rust
// Параметры как типы:
struct HpParam;
impl TemplateParam for HpParam { type Value = f32; }

struct NameParam;
impl NameParam for NameParam { type Value = String; }

// Использование:
world.spawn_from_template("Boss", TemplateParams::new()
    .set::<HpParam>(500.0)
    .set::<NameParam>("Dragon King".to_string()));

// Внутри шаблона:
fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
    let hp   = params.get::<HpParam>().copied().unwrap_or(100.0);
    let name = params.get::<NameParam>().cloned().unwrap_or_default();
    world.spawn((Health { current: hp }, Name(name)))
}
```

### Commands — единственный буфер

```rust
// DeferredQueue больше не существует — везде Commands:
let mut cmds = Commands::new();
cmds.spawn((Position::default(), Velocity::default()));
cmds.despawn(old_entity);
cmds.insert(entity, NewComponent { value: 42 });
cmds.remove::<OldComponent>(entity);
cmds.remove_raw(entity, raw_component_id);  // для динамических случаев
cmds.apply(&mut world);
```

---

## Чеклист применения (обновлён по факту реализации)

```
П-1: spawn_bundle → spawn ✅
[x] Bundle для () — добавлен impl Bundle for ()
[x] World::spawn<B: Bundle> — новый унифицированный метод
[x] World::spawn_bundle — удалён
[x] World::spawn_empty — удалён
[x] EntityBuilder — удалён (заменён EntityRef из П-2)
[x] Commands::spawn<B> — переименован из spawn_bundle
[x] Все вхождения spawn_bundle → spawn
[x] Все вхождения spawn_empty → spawn(())

П-2: EntityRef ✅
[x] pub struct EntityRef<'w> — добавлен в world.rs
[x] World::entity(&mut self, Entity) -> EntityRef — добавлен
[x] EntityRef::insert, remove, despawn, get, get_mut — реализованы
[x] EntityRef::add_relation, remove_relation, has_relation — реализованы
[x] EntityRef::is_alive, id — реализованы
[x] Документация world.rs обновлена
[x] Примеры в apex-examples обновлены

П-3: Унификация итерации ✅
[x] Query::for_each_component — удалён
[x] CachedQuery::for_each_component — удалён
[x] CachedQuery::par_for_each_component — удалён
[x] QueryComponentIter — удалён
[x] Все вхождения for_each_component → for_each с |_, components|
[x] Оптимизация for_each: slice + iter вместо entities[row] (bound check elimination)
[x] Документация обновлена

П-4: DeferredQueue → Commands ✅
[x] Commands::remove_raw — присутствует
[x] Commands::insert_raw — добавлен
[x] DeferredQueue struct — удалён
[x] Все вхождения DeferredQueue → Commands
[x] Документация commands.rs обновлена

П-5: TemplateParams ✅
[x] pub trait TemplateParam — добавлен в template.rs
[x] TemplateParams переписан на TypeId-ключи (используется std::collections::HashMap)
[x] Старые .with("str", value) — удалены
[x] Все реализации EntityTemplate::spawn — обновлены
[x] Все вызовы spawn_from_template — обновлены
[x] Тесты template.rs обновлены

П-6: WorldScriptingExt ✅
[x] pub trait WorldScriptingExt — добавлен в apex-scripting (в lib.rs)
[x] impl WorldScriptingExt for World — реализован
[x] pub export — автоматически (pub trait в lib.rs)
[x] Все примеры (apex-examples) — обновлены

Финальная проверка:
[x] cargo check --workspace — без ошибок
[x] cargo test --workspace — все тесты проходят
[x] spawn_bundle — нет результатов
[x] spawn_empty — нет результатов
[x] for_each_component — нет результатов
[x] DeferredQueue — нет результатов
[x] .with("") в template контексте — нет результатов
```
