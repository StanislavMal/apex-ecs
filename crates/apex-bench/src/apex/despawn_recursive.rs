use apex_core::prelude::*;
use apex_macros::Component;

// DespawnRecursive — cascading despawn of a hierarchy (our feature + bevy 0.16+). A parent + 1000 children
// via ChildOf; `despawn_recursive` tears down the whole subtree. Isolated with `iter_batched` (setup builds
// the hierarchy outside measurement). Not covered; a real path for scene unloading / aggregate death.
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

pub fn setup() -> (World, Entity) {
    let mut world = World::new();
    let parent = world.spawn((A(0.0),));
    for i in 0..1000 {
        let c = world.spawn((A(i as f32),));
        world.add_relation(c, ChildOf, parent);
    }
    (world, parent)
}

pub fn run((mut world, parent): (World, Entity)) {
    world.despawn_recursive(ChildOf, parent);
}
