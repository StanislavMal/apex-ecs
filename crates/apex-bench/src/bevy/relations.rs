use bevy_ecs::hierarchy::{ChildOf, Children};
use bevy_ecs::prelude::*;

#[derive(Component)]
struct A(f32);

pub struct Benchmark;

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) -> u64 {
        let mut world = World::new();
        let parent = world.spawn(A(0.0)).id();
        for i in 0..10_000 {
            world.spawn((A(i as f32), ChildOf(parent)));
        }
        let mut count = 0u64;
        if let Some(children) = world.entity(parent).get::<Children>() {
            count = children.iter().count() as u64;
        }
        count
    }
}
