use bevy_ecs::prelude::*;
use bevy_tasks::{ComputeTaskPool, TaskPool};
use cgmath::*;

#[derive(Component, Copy, Clone)]
struct Mat(Matrix4<f32>);

#[derive(Component, Copy, Clone)]
struct Position(Vector3<f32>);

#[derive(Component, Copy, Clone)]
struct Rotation(Vector3<f32>);

#[derive(Component, Copy, Clone)]
struct Velocity(Vector3<f32>);

pub struct Benchmark(World, QueryState<(&'static Mat, &'static mut Position)>);

impl Benchmark {
    pub fn new() -> Self {
        ComputeTaskPool::get_or_init(TaskPool::default);

        let mut world = World::new();
        world.spawn_batch((0..1000).map(|_| {
            (
                Mat(Matrix4::<f32>::from_angle_x(Rad(1.2))),
                Position(Vector3::unit_x()),
                Rotation(Vector3::unit_x()),
                Velocity(Vector3::unit_x()),
            )
        }));

        let query = world.query::<(&Mat, &mut Position)>();
        Self(world, query)
    }

    pub fn run(&mut self) {
        self.1
            .par_iter_mut(&mut self.0)
            .for_each(|(mat, mut pos)| {
                let mut m = mat.0;
                for _ in 0..100 {
                    m = m.invert().unwrap_or(m);
                }
                pos.0 = m.transform_vector(pos.0);
            });
    }
}
