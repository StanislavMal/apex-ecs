use bevy_ecs::prelude::*;
use cgmath::*;

// Bevy counterpart of apex `SpawnWide`: 10k INDIVIDUAL spawns of the same four-component
// bundle (`world.spawn`, not `spawn_batch`). Bevy resolves a bundle once into a `BundleInfo`
// keyed by `TypeId::of::<B>()`, so its cost per spawn is flat in the bundle's width — that is
// the bar this cell holds apex to.

#[allow(dead_code)]
#[derive(Component, Copy, Clone)]
struct Transform(Matrix4<f32>);

#[allow(dead_code)]
#[derive(Component, Copy, Clone)]
struct Position(Vector3<f32>);

#[allow(dead_code)]
#[derive(Component, Copy, Clone)]
struct Rotation(Vector3<f32>);

#[allow(dead_code)]
#[derive(Component, Copy, Clone)]
struct Velocity(Vector3<f32>);

pub struct Benchmark;

impl Default for Benchmark {
    fn default() -> Self {
        Self::new()
    }
}

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = World::new();
        for _ in 0..10_000 {
            world.spawn((
                Transform(Matrix4::from_scale(1.0)),
                Position(Vector3::unit_x()),
                Rotation(Vector3::unit_x()),
                Velocity(Vector3::unit_x()),
            ));
        }
    }
}
