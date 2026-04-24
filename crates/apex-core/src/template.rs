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
//! impl EntityTemplate for MonsterTemplate {
//!     fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
//!         let health = params.get::<f32>("health").copied().unwrap_or(self.health);
//!         let speed  = params.get::<f32>("speed").copied().unwrap_or(self.speed);
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
//! let entity = world.spawn_from_template("Monster", &TemplateParams::new()
//!     .with("speed", 10.0f32)
//! ).unwrap();
//! ```

use crate::{
    entity::Entity,
    world::World,
};
use rustc_hash::FxHashMap;
use std::any::Any;

// ── TemplateParams ───────────────────────────────────────────────

/// Параметры шаблона — значения для переопределения полей при спавне.
///
/// Хранит `HashMap<String, Box<dyn Any + Send>>`. Типизированный доступ
/// через [`get::<T>()`](TemplateParams::get).
#[derive(Default)]
pub struct TemplateParams {
    overrides: FxHashMap<String, Box<dyn Any + Send>>,
}

impl TemplateParams {
    pub fn new() -> Self {
        Self { overrides: FxHashMap::default() }
    }

    /// Переопределить значение по ключу.
    ///
    /// # Пример
    /// ```ignore
    /// let params = TemplateParams::new()
    ///     .with("health", 150.0f32)
    ///     .with("name", "Elite Monster".to_string());
    /// ```
    pub fn with<T: Send + 'static>(mut self, key: &str, value: T) -> Self {
        self.overrides.insert(key.to_string(), Box::new(value));
        self
    }

    /// Получить значение по ключу, если оно было переопределено и тип совпадает.
    pub fn get<T: 'static>(&self, key: &str) -> Option<&T> {
        self.overrides.get(key)?.downcast_ref::<T>()
    }

    /// Есть ли переопределения?
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty()
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
}

// ── TemplateRegistry ─────────────────────────────────────────────

/// Реестр именованных шаблонов.
///
/// Хранит `HashMap<String, Box<dyn EntityTemplate>>`.
/// Каждый шаблон можно вызвать по имени через [`World::spawn_from_template`].
pub struct TemplateRegistry {
    templates: FxHashMap<String, Box<dyn EntityTemplate>>,
}

impl TemplateRegistry {
    pub fn new() -> Self {
        Self { templates: FxHashMap::default() }
    }

    /// Зарегистрировать именованный шаблон.
    pub fn register(&mut self, name: &str, template: impl EntityTemplate + 'static) {
        self.templates.insert(name.to_string(), Box::new(template));
    }

    /// Создать entity из зарегистрированного шаблона.
    pub fn spawn_from_template(
        &self,
        world: &mut World,
        name: &str,
        params: &TemplateParams,
    ) -> Option<Entity> {
        self.templates.get(name).map(|t| t.spawn(world, params))
    }

    /// Получить raw pointer на шаблон по имени (для обхода borrow checker).
    ///
    /// # Safety
    /// Вызывающий должен гарантировать, что шаблон жив на момент вызова `spawn`.
    pub(crate) fn get_raw(&self, name: &str) -> Option<*const dyn EntityTemplate> {
        self.templates.get(name).map(|t| t.as_ref() as *const dyn EntityTemplate)
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
    fn default() -> Self { Self::new() }
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
/// impl_entity_template!(MonsterTemplate, |this, world, params| {
///     let health = params.get::<f32>("health").copied().unwrap_or(this.health);
///     let speed  = params.get::<f32>("speed").copied().unwrap_or(this.speed);
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

    struct Position {
        x: f32,
        y: f32,
    }

    struct Label(String);

    // ── Helper template ──────────────────────────────────────────

    struct TestTemplate {
        default_x: f32,
        default_y: f32,
    }

    impl EntityTemplate for TestTemplate {
        fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
            let x = params.get::<f32>("x").copied().unwrap_or(self.default_x);
            let y = params.get::<f32>("y").copied().unwrap_or(self.default_y);
            let label = params.get::<String>("label").cloned().unwrap_or_else(|| "default".to_string());

            world.spawn()
                .insert(Position { x, y })
                .insert(Label(label))
                .id()
        }
    }

    // ── Tests ────────────────────────────────────────────────────

    #[test]
    fn template_register_and_spawn() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Label>();

        world.register_template("test", TestTemplate { default_x: 10.0, default_y: 20.0 });

        let entity = world.spawn_from_template("test", &TemplateParams::new()).unwrap();
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

        world.register_template("test", TestTemplate { default_x: 10.0, default_y: 20.0 });

        let entity = world.spawn_from_template("test", &TemplateParams::new()
            .with("x", 99.0f32)
            .with("label", "custom".to_string())
        ).unwrap();

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.x, 99.0);    // override
        assert_eq!(pos.y, 20.0);    // default
        let label = world.get::<Label>(entity).unwrap();
        assert_eq!(label.0, "custom"); // override
    }

    #[test]
    fn template_default_params() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Label>();

        world.register_template("test", TestTemplate { default_x: 10.0, default_y: 20.0 });

        let entity = world.spawn_template("test").unwrap();
        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.x, 10.0);
        assert_eq!(pos.y, 20.0);
    }

    #[test]
    fn template_not_found() {
        let mut world = World::new();
        let result = world.spawn_from_template("nonexistent", &TemplateParams::new());
        assert!(result.is_none());
    }

    #[test]
    fn template_registry_has() {
        let mut world = World::new();
        world.register_template("a", TestTemplate { default_x: 1.0, default_y: 2.0 });
        world.register_template("b", TestTemplate { default_x: 3.0, default_y: 4.0 });

        assert!(world.template_registry().has("a"));
        assert!(world.template_registry().has("b"));
        assert!(!world.template_registry().has("c"));
        assert_eq!(world.template_registry().len(), 2);
    }

    #[test]
    fn template_macro_works() {
        struct MyTemplate { value: i32 }

        impl_entity_template!(MyTemplate, |this, world, params| {
            let val = params.get::<i32>("val").copied().unwrap_or(this.value);
            world.spawn().insert(MyTemplate { value: val }).id()
        });

        let mut world = World::new();
        world.register_component::<MyTemplate>();
        world.register_template("my", MyTemplate { value: 42 });

        let entity = world.spawn_template("my").unwrap();
        let v = world.get::<MyTemplate>(entity).unwrap();
        assert_eq!(v.value, 42);

        let entity2 = world.spawn_from_template("my", &TemplateParams::new()
            .with("val", 100i32)
        ).unwrap();
        let v2 = world.get::<MyTemplate>(entity2).unwrap();
        assert_eq!(v2.value, 100);
    }
}
