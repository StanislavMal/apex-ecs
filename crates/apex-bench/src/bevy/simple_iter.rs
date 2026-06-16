use bevy_ecs::prelude::*;
use cgmath::*;

#[derive(Component, Copy, Clone)]
struct Transform(Matrix4<f32>);

#[derive(Component, Copy, Clone)]
struct Position(Vector3<f32>);

#[derive(Component, Copy, Clone)]
struct Rotation(Vector3<f32>);

#[derive(Component, Copy, Clone)]
struct Velocity(Vector3<f32>);

pub struct Benchmark(World, QueryState<(&'static Velocity, &'static mut Position)>);

impl Benchmark {
    pub fn new() -> Self {
        let mut world = World::new();
        world.spawn_batch((0..10_000).map(|_| {
            (
                Transform(Matrix4::from_scale(1.0)),
                Position(Vector3::unit_x()),
                Rotation(Vector3::unit_x()),
                Velocity(Vector3::unit_x()),
            )
        }));

        let query = world.query::<(&Velocity, &mut Position)>();
        Self(world, query)
    }

    pub fn run(&mut self) {
        self.1
            .iter_mut(&mut self.0)
            .for_each(|(velocity, mut position)| {
                position.0 += velocity.0;
            });
    }
}
