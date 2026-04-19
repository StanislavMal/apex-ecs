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
        
        // Spawn entities with both components
        for i in 0..10_000 {
            world.spawn_bundle((
                Position { x: i as f32, y: 0.0 },
                Velocity { x: 1.0, y: 0.0 },
            ));
        }
        
        let mut sum_x = 0.0f32;
        let mut sum_y = 0.0f32;
        
        apex_core::query::Query::<(apex_core::query::Read<Position>, apex_core::query::Read<Velocity>)>::new(&world)
            .for_each_component(|(pos, vel)| {
                sum_x += pos.x + vel.x;
                sum_y += pos.y + vel.y;
            });
        
        std::hint::black_box(sum_x);
        std::hint::black_box(sum_y);
    }
}
