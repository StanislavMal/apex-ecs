use apex_core::prelude::*;
use apex_macros::Component;

// GetComponent — random-access `world.get::<T>(entity)` over 10k entities. Measures the path
// entity → location (generational records) → archetype → column → row. Not covered by other
// benches (iteration goes by archetypes, not by entity-id). Hot gameplay path (access by handle).
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

pub struct GetComponent {
    world: World,
    entities: Vec<Entity>,
}

impl Default for GetComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl GetComponent {
    pub fn new() -> Self {
        let mut world = World::new();
        let entities = world.spawn_many(10_000, |i| (A(i as f32), B(0.0)));
        Self { world, entities }
    }

    pub fn run(&self) -> f32 {
        let mut sum = 0.0f32;
        for &e in &self.entities {
            sum += self.world.get::<A>(e).unwrap().0;
        }
        sum
    }
}
