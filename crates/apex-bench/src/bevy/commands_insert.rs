use bevy_ecs::prelude::*;
use bevy_ecs::world::CommandQueue;

#[derive(Component)]
struct A(f32);

#[derive(Component)]
struct B(f32);

pub fn setup() -> (World, Vec<Entity>) {
    let mut world = World::new();
    let entities = world
        .spawn_batch((0..10_000).map(|_| (A(0.0),)))
        .collect::<Vec<_>>();
    (world, entities)
}

pub fn run((mut world, entities): (World, Vec<Entity>)) {
    let mut queue = CommandQueue::default();
    {
        let mut commands = Commands::new(&mut queue, &world);
        for &e in &entities {
            commands.entity(e).insert(B(1.0));
        }
    }
    queue.apply(&mut world);
}
