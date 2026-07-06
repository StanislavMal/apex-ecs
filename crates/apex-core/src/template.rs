//! EntityTemplate — programmatic entity templates.
//!
//! Lets you register named templates in a [`World`] and spawn an
//! entity with a predefined set of components, supporting field overrides
//! via [`TemplateParams`].
//!
//! # Example
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

/// Marker trait for typed template parameters.
///
/// Each parameter is identified by a marker type implementing `TemplateParam`.
/// This lets us use `TypeId` as the key instead of strings.
///
/// To have a parameter automatically converted into overrides for PrefabManifest,
/// override [`component_type_name()`](TemplateParam::component_type_name).
///
/// # Example
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
    /// The parameter's value type.
    type Value: Send + Sync + 'static + Serialize;

    /// The fully qualified name of the component type this parameter drives.
    ///
    /// Override this method to enable automatic conversion of parameters
    /// into overrides when spawning via `PrefabManifest`.
    ///
    /// # Example
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

/// Template parameters — values for overriding fields at spawn time.
///
/// Stores `HashMap<TypeId, Box<dyn Any + Send + Sync>>`. Accessed via
/// [`set::<P>()`](TemplateParams::set) and [`get::<P>()`](TemplateParams::get)
/// by the marker type `P: TemplateParam`.
///
/// Also stores the reverse mapping TypeId → component type name
/// and pre-serialized JSON values for automatic
/// conversion into overrides when spawning via `PrefabManifest`.
#[derive(Default)]
pub struct TemplateParams {
    params: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    /// TypeId → component type name (for PrefabManifest overrides)
    type_names: HashMap<TypeId, String>,
    /// TypeId → pre-serialized JSON value
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

    /// Set a parameter by its marker type.
    ///
    /// If [`TemplateParam::component_type_name()`] is overridden,
    /// the value is serialized to JSON and stored for automatic
    /// conversion into overrides when spawning via `PrefabManifest`.
    ///
    /// # Example
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

    /// Get a parameter's value by its marker type.
    pub fn get<P: TemplateParam>(&self) -> Option<&P::Value> {
        self.params
            .get(&TypeId::of::<P>())
            .and_then(|b| b.downcast_ref::<P::Value>())
    }

    /// Are there any overrides?
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    /// Iterator over all (component_type_name, JSON value) pairs for overrides.
    ///
    /// Used by `PrefabManifest::spawn()` for automatic
    /// conversion of parameters into component overrides.
    pub fn json_overrides_iter(&self) -> impl Iterator<Item = (&str, &serde_json::Value)> {
        self.type_names.iter().filter_map(|(tid, name)| {
            self.json_overrides
                .get(tid)
                .map(|json| (name.as_str(), json))
        })
    }

    /// Is there at least one override with a component type name?
    pub fn has_json_overrides(&self) -> bool {
        !self.json_overrides.is_empty()
    }
}

// ── EntityTemplate trait ─────────────────────────────────────────

/// Trait for entity templates.
///
/// Lets you spawn an entity with a predefined set of components,
/// supporting field overrides via [`TemplateParams`].
///
/// # Implementation
///
/// 1. Implement `EntityTemplate` for your struct
/// 2. Register it via [`World::register_template`]
/// 3. Spawn entities via [`World::spawn_from_template`]
///
/// `Send + Sync` is required for storage in `TemplateRegistry`
/// (access from parallel systems).
pub trait EntityTemplate: Send + Sync {
    /// Spawn an entity in the given world with the given parameters.
    ///
    /// `params` holds the field overrides supplied by the user
    /// when calling `spawn_from_template`. If a field is not overridden,
    /// use the template's default values.
    fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity;

    /// Optional parent for the spawned entity.
    ///
    /// If this returns `Some(parent_entity)`, a `ChildOf(parent)` relation
    /// is automatically established after spawning.
    /// Returns `None` by default (no parent).
    fn parent(&self) -> Option<Entity> {
        None
    }
}

// ── TemplateRegistry ─────────────────────────────────────────────

/// Registry of named templates.
///
/// Stores `HashMap<String, Arc<dyn EntityTemplate>>`. `Arc` (not `Box`)
/// lets us clone the template handle and release the registry borrow BEFORE
/// calling `spawn` — otherwise a template that re-registers itself from its own
/// `spawn` would free the `Box` out from under a live pointer (UAF, B4).
/// Each template can be invoked by name via [`World::spawn_from_template`].
pub struct TemplateRegistry {
    templates: HashMap<String, Arc<dyn EntityTemplate>>,
}

impl TemplateRegistry {
    pub fn new() -> Self {
        Self {
            templates: HashMap::default(),
        }
    }

    /// Register a named template.
    pub fn register(&mut self, name: &str, template: impl EntityTemplate + 'static) {
        self.templates.insert(name.to_string(), Arc::new(template));
    }

    /// Spawn an entity from a registered template.
    ///
    /// If the template returns `Some(parent)` from [`EntityTemplate::parent()`],
    /// a `ChildOf(parent)` relation is automatically established after spawning.
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

    /// Clone the template handle by name. The `Arc` clone detaches the template
    /// from the registry borrow, so the caller can hold `&mut World` during
    /// `spawn` without risk of UAF, even if the template re-registers itself (B4).
    pub(crate) fn get_arc(&self, name: &str) -> Option<Arc<dyn EntityTemplate>> {
        self.templates.get(name).cloned()
    }

    /// Check whether a template is registered.
    pub fn has(&self, name: &str) -> bool {
        self.templates.contains_key(name)
    }

    /// The number of registered templates.
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

/// Macro for conveniently implementing `EntityTemplate` with a closure.
///
/// # Example
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

    // ── Marker types for typed parameters ────────────────────────

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

        // There should be exactly one entity with Position
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
                // Will be set by external code via a closure or by storing the parent in the struct.
                // In this test we exercise the mechanism through registration.
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

        // Verify that child has the ChildOf(parent) relation
        assert!(world.has_relation(child, ChildOf, parent));
    }
}
