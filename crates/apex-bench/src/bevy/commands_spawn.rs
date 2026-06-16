use bevy_ecs::prelude::*;
use bevy_ecs::world::CommandQueue;

#[derive(Component)]
struct A(f32);

#[derive(Component)]
struct B(f32);

pub struct Benchmark;

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        {
            let mut commands = Commands::new(&mut queue, &world);
            for _ in 0..10_000 {
                commands.spawn((A(0.0), B(0.0)));
            }
        }
        queue.apply(&mut world);
    }
}
