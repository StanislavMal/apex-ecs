use crate::{Position, Velocity, Mass, Renderable};

pub struct Benchmark;

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = apex_core::world::World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Mass>();
        world.register_component::<Renderable>();
        
        // Create fragmented archetypes
        for i in 0..1_000 {
            match i % 4 {
                0 => {
                    world.spawn_bundle((Position { x: i as f32, y: 0.0 },));
                }
                1 => {
                    world.spawn_bundle((
                        Position { x: i as f32, y: 0.0 },
                        Velocity { x: 1.0, y: 0.0 },
                    ));
                }
                2 => {
                    world.spawn_bundle((
                        Position { x: i as f32, y: 0.0 },
                        Mass(i as f32),
                    ));
                }
                3 => {
                    world.spawn_bundle((
                        Position { x: i as f32, y: 0.0 },
                        Renderable { mesh: i as u32, material: 0 },
                    ));
                }
                _ => unreachable!(),
            }
        }
        
        let mut sum = 0.0f32;
        
        apex_core::query::Query::<apex_core::query::Read<Position>>::new(&world)
            .for_each_component(|pos| {
                sum += pos.x;
            });
        
        std::hint::black_box(sum);
    }
}
