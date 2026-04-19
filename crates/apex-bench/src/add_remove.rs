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
        
        // Spawn entities with Position only
        for i in 0..10_000 {
            world.spawn_bundle((Position { x: i as f32, y: 0.0 },));
        }
        
        // Add Velocity to all entities
        let mut entities = Vec::new();
        apex_core::query::Query::<apex_core::query::Read<Position>>::new(&world)
            .for_each(|entity, _| entities.push(entity));
        
        for &entity in &entities {
            world.insert(entity, Velocity { x: 1.0, y: 0.0 });
        }
        
        // Remove Velocity from all entities
        for &entity in &entities {
            world.remove::<Velocity>(entity);
        }
    }
}
