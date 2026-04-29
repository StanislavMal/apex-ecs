use super::ffi;
use super::wrapper::FlecsWorld;
use cgmath::{Matrix4, Vector3};

pub struct FlecsSimpleInsert;

impl FlecsSimpleInsert {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = FlecsWorld::new();
        let transform = world.register_component::<Matrix4<f32>>();
        let position = world.register_component::<Vector3<f32>>();
        let rotation = world.register_component::<Vector3<f32>>();
        let velocity = world.register_component::<Vector3<f32>>();

        let t = Matrix4::from_scale(1.0);
        let v: Vector3<f32> = Vector3::unit_x();

        for _ in 0..10_000 {
            let e = unsafe { ffi::ecs_new_w_id(world.raw(), transform) };
            unsafe {
                ffi::ecs_set_id(
                    world.raw(), e, position, 12,
                    &v as *const _ as *const std::ffi::c_void,
                );
                ffi::ecs_set_id(
                    world.raw(), e, rotation, 12,
                    &v as *const _ as *const std::ffi::c_void,
                );
                ffi::ecs_set_id(
                    world.raw(), e, velocity, 12,
                    &v as *const _ as *const std::ffi::c_void,
                );
            }
        }
    }
}
