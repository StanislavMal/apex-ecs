use apex_core::prelude::*;
use apex_macros::Component;

// Despawn — removal of 10k entities. Isolated via criterion `iter_batched`:
// `setup` creates a populated world (NOT in the measured part), `run` only despawns.
// The despawn path is not covered by other benches; measures swap-remove from columns + slot release.
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

/// Create a populated world (outside measurement).
pub fn setup() -> (World, Vec<Entity>) {
    let mut world = World::new();
    let entities = world.spawn_many(10_000, |_| (A(0.0), B(0.0)));
    (world, entities)
}

/// Measured part: despawn all entities.
pub fn run((mut world, entities): (World, Vec<Entity>)) {
    for e in entities {
        world.despawn(e);
    }
}
