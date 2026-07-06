use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::prelude::*;

#[derive(Component)]
struct A(f32);

pub fn setup() -> (World, Entity) {
    let mut world = World::new();
    let parent = world.spawn(A(0.0)).id();
    for i in 0..1000 {
        world.spawn((A(i as f32), ChildOf(parent)));
    }
    (world, parent)
}

pub fn run((mut world, parent): (World, Entity)) {
    // bevy 0.16+: despawn cascades to children via the ChildOf/Children relationship hooks.
    world.entity_mut(parent).despawn();
}
