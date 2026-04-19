use crate::{Position, Velocity};

pub struct Benchmark;

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = apex_core::world::World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        
        for i in 0..10_000 {
            world.spawn_bundle((Position { x: i as f32, y: 0.0 },));
            world.spawn_bundle((Velocity { x: 1.0, y: 0.0 },));
        }
    }
}
