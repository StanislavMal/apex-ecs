use super::ffi;
use super::wrapper::FlecsWorld;

#[allow(dead_code)]
pub struct FlecsAddRemove {
    world: FlecsWorld,
    entities: Vec<ffi::ecs_entity_t>,
    a_id: ffi::ecs_entity_t,
    b_id: ffi::ecs_entity_t,
}

impl FlecsAddRemove {
    pub fn new() -> Self {
        let mut world = FlecsWorld::new();
        let a_id = world.register_component::<f32>();
        let b_id = world.register_component::<f32>();

        let mut entities = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            let e = unsafe { ffi::ecs_new_w_id(world.raw(), a_id) };
            entities.push(e);
        }

        Self {
            world,
            entities,
            a_id,
            b_id,
        }
    }

    pub fn run(&mut self) {
        for &e in &self.entities {
            unsafe {
                ffi::ecs_add_id(self.world.raw(), e, self.b_id);
            }
        }
        for &e in &self.entities {
            unsafe {
                ffi::ecs_remove_id(self.world.raw(), e, self.b_id);
            }
        }
    }
}
