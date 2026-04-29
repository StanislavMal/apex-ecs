use super::ffi;
use super::wrapper::{FlecsFilter, FlecsWorld};
use cgmath::{Matrix4, Vector3};

pub struct FlecsSimpleIter {
    query: FlecsFilter,
    world: FlecsWorld,
}

impl FlecsSimpleIter {
    pub fn new() -> Self {
        let mut world = FlecsWorld::new();
        let _transform = world.register_component::<Matrix4<f32>>();
        let position = world.register_component::<Vector3<f32>>();
        let velocity = world.register_component::<Vector3<f32>>();
        let _rotation = world.register_component::<Vector3<f32>>();

        let px = Vector3::new(0.0, 0.0, 0.0);
        let vx: Vector3<f32> = Vector3::new(1.0, 0.0, 0.0);

        for _ in 0..10_000 {
            let e = unsafe { ffi::ecs_new_w_id(world.raw(), _transform) };
            unsafe {
                ffi::ecs_set_id(
                    world.raw(), e, position, 12,
                    &px as *const _ as *const std::ffi::c_void,
                );
                ffi::ecs_set_id(
                    world.raw(), e, _rotation, 12,
                    &px as *const _ as *const std::ffi::c_void,
                );
                ffi::ecs_set_id(
                    world.raw(), e, velocity, 12,
                    &vx as *const _ as *const std::ffi::c_void,
                );
            }
        }

        let query = FlecsFilter::new(
            &world,
            &[velocity, position],
            &[ffi::EcsInOutIn, ffi::EcsInOutOut],
        );

        Self { query, world }
    }

    pub fn run(&self) {
        let mut it = self.query.iter(&self.world);
        while it.next() {
            let pos_slice = it.field_mut::<Vector3<f32>>(1).unwrap();
            let vel_slice = it.field::<Vector3<f32>>(0).unwrap();
            for i in 0..it.count() as usize {
                pos_slice[i] += vel_slice[i];
            }
        }
    }
}
