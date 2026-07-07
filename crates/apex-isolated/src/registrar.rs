//! `WorldRegistrar` — a replayable world-schema recipe (В4 / B5).
//!
//! Multiple worlds that exchange data (an [`crate::IsolatedWorld`] fork, a render
//! world, an editor play-world) must agree on their **schema**: the same
//! components registered for serde, the same event types, the same relation
//! kinds. Before this, each consumer duplicated those registrations by hand —
//! the renderer re-registers ~35 components into its render world line by line,
//! the editor re-registers its vocabulary for every Play fork. A drift between
//! two such lists is a silent data-loss bug (a component that serializes in one
//! world and is unknown in the other).
//!
//! A `WorldRegistrar` records the registration steps ONCE and replays them onto
//! any world, so linked worlds share one source of truth for their schema.
//!
//! ```ignore
//! let mut recipe = WorldRegistrar::new();
//! recipe
//!     .register_component_serde_json::<Position>()
//!     .register_component_serde_json::<Velocity>()
//!     .add_event::<Damage>();
//! let doc   = recipe.new_world();      // fresh world with the schema
//! let mut fork = recipe.new_world();   // an identically-schema'd fork
//! recipe.apply(iso.world_mut());       // or onto an existing world
//! ```

use apex_core::World;

/// One replayable registration step.
type Step = Box<dyn Fn(&mut World) + Send + Sync>;

/// A recorded set of world registrations that can be replayed onto any [`World`].
///
/// `Send + Sync` (the steps are), so it can be shared across the threads that own
/// linked worlds and used to (re)build their schema identically.
#[derive(Default)]
pub struct WorldRegistrar {
    steps: Vec<Step>,
}

impl WorldRegistrar {
    /// An empty recipe.
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Record `register_component_serde_json::<T>()` (JSON serde — the editor/
    /// snapshot default). Chainable.
    pub fn register_component_serde_json<T>(&mut self) -> &mut Self
    where
        T: apex_core::component::Serializable + 'static,
    {
        self.steps.push(Box::new(|w: &mut World| {
            w.register_component_serde_json::<T>();
        }));
        self
    }

    /// Record `register_component_serde::<T>()` (binary serde). Chainable.
    pub fn register_component_serde<T>(&mut self) -> &mut Self
    where
        T: apex_core::component::Serializable + 'static,
    {
        self.steps.push(Box::new(|w: &mut World| {
            w.register_component_serde::<T>();
        }));
        self
    }

    /// Record `add_event::<T>()`. Chainable.
    pub fn add_event<T>(&mut self) -> &mut Self
    where
        T: Send + Sync + 'static,
    {
        self.steps.push(Box::new(|w: &mut World| {
            w.add_event::<T>();
        }));
        self
    }

    /// Record registration of a relation KIND `R` by name, so relations of that
    /// kind resolve on restore/transfer. Relation kinds are part of the schema:
    /// a world that receives a transferred subtree must know the kind by name or
    /// its relations are dropped (only `ChildOf` is auto-registered). Chainable.
    pub fn register_relation_kind<R>(&mut self) -> &mut Self
    where
        R: apex_core::relations::RelationKind + 'static,
    {
        self.steps.push(Box::new(|w: &mut World| {
            w.relation_registry_mut().get_or_register::<R>();
        }));
        self
    }

    /// Record an arbitrary registration step — the escape hatch for anything not
    /// covered above (relation kinds, resources, custom setup). The closure must
    /// be `Send + Sync` so the recipe stays shareable. Chainable.
    pub fn with<F>(&mut self, step: F) -> &mut Self
    where
        F: Fn(&mut World) + Send + Sync + 'static,
    {
        self.steps.push(Box::new(step));
        self
    }

    /// Replay every recorded step onto `world`, in insertion order.
    pub fn apply(&self, world: &mut World) {
        for step in &self.steps {
            step(world);
        }
    }

    /// Build a fresh world and apply the recipe to it.
    pub fn new_world(&self) -> World {
        let mut world = World::new();
        self.apply(&mut world);
        world
    }

    /// Number of recorded steps.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the recipe has no steps.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}
