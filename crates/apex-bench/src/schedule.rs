use crate::{Position, Velocity};
use apex_scheduler::{Scheduler, ParSystem};
use apex_core::world::SystemContext;
use apex_core::access::AccessDescriptor;

pub struct Benchmark;

struct MoveSystem;
impl ParSystem for MoveSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().read::<Velocity>().write::<Position>()
    }
    fn run(&mut self, ctx: SystemContext<'_>) {
        ctx.query::<(apex_core::query::Read<Velocity>, apex_core::query::Write<Position>)>()
            .for_each_component(|(v, p)| {
                p.x += v.x;
                p.y += v.y;
            });
    }
}

struct GravitySystem;
impl ParSystem for GravitySystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().write::<Position>()
    }
    fn run(&mut self, ctx: SystemContext<'_>) {
        ctx.query::<apex_core::query::Write<Position>>()
            .for_each_component(|p| {
                p.y -= 9.8 * 0.016;
            });
    }
}

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = apex_core::world::World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        
        // Spawn entities
        for i in 0..10_000 {
            world.spawn_bundle((
                Position { x: i as f32, y: 0.0 },
                Velocity { x: 1.0, y: 0.0 },
            ));
        }
        
        let mut scheduler = Scheduler::new();
        scheduler.add_par_system("move", MoveSystem);
        scheduler.add_par_system("gravity", GravitySystem);
        
        scheduler.compile().unwrap();
        scheduler.run_sequential(&mut world);
    }
}
