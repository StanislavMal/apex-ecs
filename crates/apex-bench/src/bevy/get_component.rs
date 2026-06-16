use bevy_ecs::prelude::*;

#[derive(Component)]
struct A(f32);

#[derive(Component)]
struct B(f32);

pub struct Benchmark {
    world: World,
    entities: Vec<Entity>,
}

impl Benchmark {
    pub fn new() -> Self {
        let mut world = World::new();
        let entities = world
            .spawn_batch((0..10_000).map(|i| (A(i as f32), B(0.0))))
            .collect::<Vec<_>>();
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
