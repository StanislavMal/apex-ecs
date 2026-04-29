use super::ffi;
use super::wrapper::{FlecsFilter, FlecsWorld};

pub struct FlecsFragIter {
    query: FlecsFilter,
    world: FlecsWorld,
}

impl FlecsFragIter {
    pub fn new() -> Self {
        let mut world = FlecsWorld::new();
        let mut marker_ids = Vec::new();
        let data = world.register_component::<f32>();

        for _ in 0..26 {
            let id = world.register_component_raw(1, 1);
            marker_ids.push(id);
        }

        for &mid in &marker_ids {
            for _ in 0..20 {
                let e = unsafe { ffi::ecs_new_w_id(world.raw(), mid) };
                unsafe {
                    ffi::ecs_set_id(
                        world.raw(), e, data, 4,
                        &1.0f32 as *const _ as *const std::ffi::c_void,
                    );
                }
            }
        }

        let query = FlecsFilter::new(
            &world,
            &[data],
            &[ffi::EcsInOutOut],
        );

        Self { query, world }
    }

    pub fn run(&self) {
        let mut it = self.query.iter(&self.world);
        while it.next() {
            let data_slice = it.field_mut::<f32>(0).unwrap();
            for val in data_slice.iter_mut() {
                *val *= 2.0;
            }
        }
    }
}
