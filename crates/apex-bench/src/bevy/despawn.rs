use bevy_ecs::prelude::*;

#[derive(Component)]
struct A(f32);

#[derive(Component)]
struct B(f32);

pub fn setup() -> (World, Vec<Entity>) {
    let mut world = World::new();
    let entities = world
        .spawn_batch((0..10_000).map(|_| (A(0.0), B(0.0))))
        .collect::<Vec<_>>();
    (world, entities)
}

pub fn run((mut world, entities): (World, Vec<Entity>)) {
    for e in entities {
        world.despawn(e);
    }
}
