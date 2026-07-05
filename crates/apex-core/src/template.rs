//! EntityTemplate — программные шаблоны сущностей.
//!
//! Позволяет регистрировать именованные шаблоны в [`World`] и создавать
//! entity с предопределённым набором компонентов, поддерживая переопределение
//! полей через [`TemplateParams`].
//!
//! # Пример
//!
//! ```ignore
//! use apex_core::template::*;
//! use apex_core::World;
//!
//! struct MonsterTemplate {
//!     health: f32,
//!     speed:  f32,
//! }
//!
//! struct MonsterTemplate {
//!     health: f32,
//!     speed:  f32,
//! }
//!
//! struct MonsterHealth;
//! impl TemplateParam for MonsterHealth { type Value = f32; }
//!
//! struct MonsterSpeed;
//! impl TemplateParam for MonsterSpeed { type Value = f32; }
//!
//! impl EntityTemplate for MonsterTemplate {
//!     fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
//!         let health = params.get::<MonsterHealth>().copied().unwrap_or(self.health);
//!         let speed  = params.get::<MonsterSpeed>().copied().unwrap_or(self.speed);
//!
//!         world.spawn()
//!             .insert(Health { current: health, max: health })
//!             .insert(Velocity(Vec3::new(speed, 0.0, 0.0)))
//!             .insert(Name("Monster"))
//!             .id()
//!     }
//! }
//!
//! let mut world = World::new();
//! world.register_template("Monster", MonsterTemplate { health: 100.0, speed: 5.0 });
//!
//! let entity = world.spawn_template_with("Monster", &TemplateParams::new()
//!     .set::<MonsterSpeed>(10.0f32)
//! ).unwrap();
//! ```

use crate::{entity::Entity, relations::ChildOf, world::World};
use serde::Serialize;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

// ── TemplateParam trait ──────────────────────────────────────────

/// Маркерный трейт для типизированных параметров шаблонов.
///
/// Каждый параметр идентифицируется типом-маркером, реализующим `TemplateParam`.
/// Это позволяет использовать `TypeId` в качестве ключа вместо строк.
///
/// Чтобы параметр автоматически преобразовывался в overrides для PrefabManifest,
/// переопределите [`component_type_name()`](TemplateParam::component_type_name).
///
/// # Пример
///
/// ```ignore
/// struct SpawnX;
/// impl TemplateParam for SpawnX { type Value = f32; }
///
/// let params = TemplateParams::new()
///     .set::<SpawnX>(99.0f32);
/// let val = params.get::<SpawnX>().copied().unwrap_or(10.0);
/// ```
pub trait TemplateParam: Send + Sync + 'static {
    /// Тип значения параметра.
    type Value: Send + Sync + 'static + Serialize;

    /// Полное имя типа компонента, которым управляет этот параметр.
    ///
    /// Переопределите этот метод чтобы включить автоматическое преобразование
    /// параметров в overrides при спавне через `PrefabManifest`.
    ///
    /// # Пример
    /// ```ignore
    /// struct MonsterHealth;
    /// impl TemplateParam for MonsterHealth {
    ///     type Value = f32;
    ///     fn component_type_name() -> &'static str { "my_crate::Health" }
    /// }
    /// ```
    fn component_type_name() -> &'static str {
        ""
    }
}

// ── TemplateParams ───────────────────────────────────────────────

/// Параметры шаблона — значения для переопределения полей при спавне.
///
/// Хранит `HashMap<TypeId, Box<dyn Any + Send + Sync>>`. Доступ через
/// [`set::<P>()`](TemplateParams::set) и [`get::<P>()`](TemplateParams::get)
/// по типу-маркеру `P: TemplateParam`.
///
/// Также хранит обратный маппинг TypeId → имя типа компонента
/// и предсериализованные JSON-значения для автоматического
/// преобразования в overrides при спавне через `PrefabManifest`.
#[derive(Default)]
pub struct TemplateParams {
    params: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    /// TypeId → имя типа компонента (для PrefabManifest overrides)
    type_names: HashMap<TypeId, String>,
    /// TypeId → предсериализованное JSON-значение
    json_overrides: HashMap<TypeId, serde_json::Value>,
}

impl TemplateParams {
    pub fn new() -> Self {
        Self {
            params: HashMap::default(),
            type_names: HashMap::default(),
            json_overrides: HashMap::default(),
        }
    }

    /// Установить параметр по типу-маркеру.
    ///
    /// Если [`TemplateParam::component_type_name()`] переопределён,
    /// значение сериализуется в JSON и сохраняется для автоматического
    /// преобразования в overrides при спавне через `PrefabManifest`.
    ///
    /// # Пример
    /// ```ignore
    /// struct HealthParam;
    /// impl TemplateParam for HealthParam { type Value = f32; }
    ///
    /// let params = TemplateParams::new()
    ///     .set::<HealthParam>(150.0f32)
    ///     .set::<NameParam>("Elite Monster".to_string());
    /// ```
    pub fn set<P: TemplateParam>(mut self, value: P::Value) -> Self {
        let type_id = TypeId::of::<P>();
        let name = P::component_type_name();
        if !name.is_empty() {
            self.type_names.insert(type_id, name.to_string());
            match serde_json::to_value(&value) {
                Ok(json) => {
                    self.json_overrides.insert(type_id, json);
                }
                // §0.2a (B10): a param that fails to serialise silently produced
                // no override — the prefab would spawn with the default value and
                // the caller's `.set(...)` would look ignored. Surface it.
                Err(e) => crate::warn_once!(
                    "TemplateParams::set::<{}>: value failed to serialise ({e}) — override for '{name}' dropped",
                    std::any::type_name::<P>(),
                ),
            }
        }
        self.params.insert(type_id, Box::new(value));
        self
    }

    /// Получить значение параметра по типу-маркеру.
    pub fn get<P: TemplateParam>(&self) -> Option<&P::Value> {
        self.params
            .get(&TypeId::of::<P>())
            .and_then(|b| b.downcast_ref::<P::Value>())
    }

    /// Есть ли переопределения?
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    /// Итератор по всем парам (component_type_name, JSON-значение) для override-ов.
    ///
    /// Используется `PrefabManifest::spawn()` для автоматического
    /// преобразования параметров в overrides компонентов.
    pub fn json_overrides_iter(&self) -> impl Iterator<Item = (&str, &serde_json::Value)> {
        self.type_names.iter().filter_map(|(tid, name)| {
            self.json_overrides
                .get(tid)
                .map(|json| (name.as_str(), json))
        })
    }

    /// Есть ли хотя бы один override с именем типа компонента?
    pub fn has_json_overrides(&self) -> bool {
        !self.json_overrides.is_empty()
    }
}

// ── EntityTemplate trait ─────────────────────────────────────────

/// Трейт для шаблонов сущностей.
///
/// Позволяет создавать entity с предопределённым набором компонентов,
/// поддерживая переопределение полей через [`TemplateParams`].
///
/// # Реализация
///
/// 1. Реализовать `EntityTemplate` для вашей структуры
/// 2. Зарегистрировать через [`World::register_template`]
/// 3. Создавать entity через [`World::spawn_from_template`]
///
/// `Send + Sync` требуется для хранения в `TemplateRegistry`
/// (доступ из параллельных систем).
pub trait EntityTemplate: Send + Sync {
    /// Создать entity в указанном мире с параметрами.
    ///
    /// `params` содержит переопределения полей, заданные пользователем
    /// при вызове `spawn_from_template`. Если поле не переопределено —
    /// использовать значения по умолчанию из шаблона.
    fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity;

    /// Опциональный родитель для создаваемой entity.
    ///
    /// Если вернуть `Some(parent_entity)`, то после спавна будет
    /// автоматически установлено отношение `ChildOf(parent)`.
    /// По умолчанию возвращает `None` (без родителя).
    fn parent(&self) -> Option<Entity> {
        None
    }
}

// ── TemplateRegistry ─────────────────────────────────────────────

/// Реестр именованных шаблонов.
///
/// Хранит `HashMap<String, Arc<dyn EntityTemplate>>`. `Arc` (а не `Box`)
/// позволяет клонировать хэндл шаблона и отпустить заём реестра ДО вызова
/// `spawn` — иначе шаблон, перерегистрирующий себя из своего же `spawn`,
/// освободил бы `Box` под живым указателем (UAF, B4).
/// Каждый шаблон можно вызвать по имени через [`World::spawn_from_template`].
pub struct TemplateRegistry {
    templates: HashMap<String, Arc<dyn EntityTemplate>>,
}

impl TemplateRegistry {
    pub fn new() -> Self {
        Self {
            templates: HashMap::default(),
        }
    }

    /// Зарегистрировать именованный шаблон.
    pub fn register(&mut self, name: &str, template: impl EntityTemplate + 'static) {
        self.templates.insert(name.to_string(), Arc::new(template));
    }

    /// Создать entity из зарегистрированного шаблона.
    ///
    /// Если шаблон возвращает `Some(parent)` из [`EntityTemplate::parent()`],
    /// то после спавна автоматически устанавливается `ChildOf(parent)`.
    pub fn spawn_from_template(
        &self,
        world: &mut World,
        name: &str,
        params: &TemplateParams,
    ) -> Option<Entity> {
        self.templates.get(name).map(|t| {
            let entity = t.spawn(world, params);
            if let Some(parent) = t.parent() {
                world.add_relation(entity, ChildOf, parent);
            }
            entity
        })
    }

    /// Клонировать хэндл шаблона по имени. Клон `Arc` отвязывает шаблон от
    /// заёма реестра, поэтому вызывающий может держать `&mut World` во время
    /// `spawn` без риска UAF, даже если шаблон перерегистрирует сам себя (B4).
    pub(crate) fn get_arc(&self, name: &str) -> Option<Arc<dyn EntityTemplate>> {
        self.templates.get(name).cloned()
    }

    /// Проверить, зарегистрирован ли шаблон.
    pub fn has(&self, name: &str) -> bool {
        self.templates.contains_key(name)
    }

    /// Количество зарегистрированных шаблонов.
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}

impl Default for TemplateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Pre-exported macro ───────────────────────────────────────────

/// Макрос для удобной имплементации `EntityTemplate` с замыканием.
///
/// # Пример
///
/// ```ignore
/// use apex_core::template::*;
/// use apex_core::entity::Entity;
/// use apex_core::World;
///
/// struct MonsterTemplate { health: f32, speed: f32 }
///
/// struct MonsterHealth;
/// impl TemplateParam for MonsterHealth { type Value = f32; }
///
/// struct MonsterSpeed;
/// impl TemplateParam for MonsterSpeed { type Value = f32; }
///
/// impl_entity_template!(MonsterTemplate, |this, world, params| {
///     let health = params.get::<MonsterHealth>().copied().unwrap_or(this.health);
///     let speed  = params.get::<MonsterSpeed>().copied().unwrap_or(this.speed);
///     world.spawn()
///         .insert(Health { current: health, max: health })
///         .insert(Velocity(Vec3::new(speed, 0.0, 0.0)))
///         .id()
/// });
/// ```
#[macro_export]
macro_rules! impl_entity_template {
    ($ty:ty, |$this:ident, $world:ident, $params:ident| $body:expr) => {
        impl $crate::template::EntityTemplate for $ty {
            fn spawn(
                &self,
                $world: &mut $crate::World,
                $params: &$crate::template::TemplateParams,
            ) -> $crate::entity::Entity {
                let $this = self;
                $body
            }
        }
    };
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::World;

    use crate::component::Component;

    struct Position {
        x: f32,
        y: f32,
    }
    impl Component for Position {}

    struct Label(String);
    impl Component for Label {}

    // ── Marker-типы для типизированных параметров ────────────────

    struct ParamX;
    impl TemplateParam for ParamX {
        type Value = f32;
    }

    struct ParamY;
    impl TemplateParam for ParamY {
        type Value = f32;
    }

    struct ParamLabel;
    impl TemplateParam for ParamLabel {
        type Value = String;
    }

    struct ParamVal;
    impl TemplateParam for ParamVal {
        type Value = i32;
    }

    // ── Helper template ──────────────────────────────────────────

    struct TestTemplate {
        default_x: f32,
        default_y: f32,
    }

    impl EntityTemplate for TestTemplate {
        fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
            let x = params.get::<ParamX>().copied().unwrap_or(self.default_x);
            let y = params.get::<ParamY>().copied().unwrap_or(self.default_y);
            let label = params
                .get::<ParamLabel>()
                .cloned()
                .unwrap_or_else(|| "default".to_string());

            world.spawn((Position { x, y }, Label(label)))
        }
    }

    // ── Tests ────────────────────────────────────────────────────

    #[test]
    fn template_register_and_spawn() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Label>();

        world.register_template(
            "test",
            TestTemplate {
                default_x: 10.0,
                default_y: 20.0,
            },
        );

        let entity = world
            .spawn_template_with("test", &TemplateParams::new())
            .unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.x, 10.0);
        assert_eq!(pos.y, 20.0);
        let label = world.get::<Label>(entity).unwrap();
        assert_eq!(label.0, "default");
    }

    #[test]
    fn template_with_params() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Label>();

        world.register_template(
            "test",
            TestTemplate {
                default_x: 10.0,
                default_y: 20.0,
            },
        );

        let entity = world
            .spawn_template_with(
                "test",
                &TemplateParams::new()
                    .set::<ParamX>(99.0f32)
                    .set::<ParamLabel>("custom".to_string()),
            )
            .unwrap();

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.x, 99.0); // override
        assert_eq!(pos.y, 20.0); // default
        let label = world.get::<Label>(entity).unwrap();
        assert_eq!(label.0, "custom"); // override
    }

    #[test]
    fn template_default_params() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Label>();

        world.register_template(
            "test",
            TestTemplate {
                default_x: 10.0,
                default_y: 20.0,
            },
        );

        let entity = world.spawn_template("test").unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.x, 10.0);
        assert_eq!(pos.y, 20.0);
    }

    #[test]
    fn template_not_found() {
        let mut world = World::new();
        let result = world.spawn_template_with("nonexistent", &TemplateParams::new());
        assert!(result.is_none());
    }

    #[test]
    fn template_registry_has() {
        let mut world = World::new();
        world.register_template(
            "a",
            TestTemplate {
                default_x: 1.0,
                default_y: 2.0,
            },
        );
        world.register_template(
            "b",
            TestTemplate {
                default_x: 3.0,
                default_y: 4.0,
            },
        );

        assert!(world.template_registry().has("a"));
        assert!(world.template_registry().has("b"));
        assert!(!world.template_registry().has("c"));
        assert_eq!(world.template_registry().len(), 2);
    }

    #[test]
    fn template_macro_works() {
        struct MyTemplate {
            value: i32,
        }
        impl Component for MyTemplate {}

        impl_entity_template!(MyTemplate, |this, world, params| {
            let val = params.get::<ParamVal>().copied().unwrap_or(this.value);
            world.spawn((MyTemplate { value: val },))
        });

        let mut world = World::new();
        world.register_component::<MyTemplate>();
        world.register_template("my", MyTemplate { value: 42 });

        let entity = world.spawn_template("my").unwrap();
        let v = world.get::<MyTemplate>(entity).unwrap();
        assert_eq!(v.value, 42);

        let entity2 = world
            .spawn_template_with("my", &TemplateParams::new().set::<ParamVal>(100i32))
            .unwrap();
        let v2 = world.get::<MyTemplate>(entity2).unwrap();
        assert_eq!(v2.value, 100);
    }

    #[test]
    fn template_in_commands() {
        use crate::query::Read;

        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Label>();
        world.register_template(
            "test",
            TestTemplate {
                default_x: 10.0,
                default_y: 20.0,
            },
        );

        let mut commands = crate::commands::Commands::new();
        commands.spawn_template("test");
        commands.apply(&mut world);

        // Должна быть ровно одна entity с Position
        let query = world.query::<Read<Position>>();
        let mut count = 0;
        query.for_each(|_, _| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn template_parent_relation() {
        use crate::relations::ChildOf;

        #[allow(dead_code)]
        struct ChildTemplate;

        impl EntityTemplate for ChildTemplate {
            fn spawn(&self, world: &mut World, _params: &TemplateParams) -> Entity {
                world.spawn((Position { x: 1.0, y: 2.0 },))
            }
            fn parent(&self) -> Option<Entity> {
                // Будет установлен внешним кодом через замыкание или хранение parent в структуре.
                // В этом тесте мы проверяем механизм через регистрацию.
                None
            }
        }

        struct ParentBoundChild {
            parent: Entity,
        }

        impl EntityTemplate for ParentBoundChild {
            fn spawn(&self, world: &mut World, _params: &TemplateParams) -> Entity {
                world.spawn((Label("child".to_string()),))
            }
            fn parent(&self) -> Option<Entity> {
                Some(self.parent)
            }
        }

        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Label>();

        let parent = world.spawn(());

        world.register_template("child", ParentBoundChild { parent });

        let child = world.spawn_template("child").unwrap();

        // Проверяем, что child имеет отношение ChildOf(parent)
        assert!(world.has_relation(child, ChildOf, parent));
    }
}
