use super::ffi;
use super::wrapper::{FlecsFilter, FlecsWorld};
use cgmath::{Matrix4, Rad, SquareMatrix, Transform, Vector3};

pub struct FlecsHeavyCompute {
    query: FlecsFilter,
    world: FlecsWorld,
}

impl FlecsHeavyCompute {
    pub fn new() -> Self {
        let mut world = FlecsWorld::new();
        let mat = world.register_component::<Matrix4<f32>>();
        let position = world.register_component::<Vector3<f32>>();
        let _rotation = world.register_component::<Vector3<f32>>();
        let _velocity = world.register_component::<Vector3<f32>>();

        let m = Matrix4::from_angle_x(Rad(1.2f32));
        let v: Vector3<f32> = Vector3::unit_x();

        for _ in 0..1000 {
            let e = unsafe { ffi::ecs_new_w_id(world.raw(), mat) };
            unsafe {
                ffi::ecs_set_id(
                    world.raw(), e, position, 12,
                    &v as *const _ as *const std::ffi::c_void,
                );
                ffi::ecs_set_id(
                    world.raw(), e, _rotation, 12,
                    &v as *const _ as *const std::ffi::c_void,
                );
                ffi::ecs_set_id(
                    world.raw(), e, _velocity, 12,
                    &v as *const _ as *const std::ffi::c_void,
                );
            }
        }

        let query = FlecsFilter::new(
            &world,
            &[mat, position],
            &[ffi::EcsInOutOut, ffi::EcsInOutOut],
        );

        Self { query, world }
    }

    pub fn run(&self) {
        let mut it = self.query.iter(&self.world);
        while it.next() {
            let mat_slice = it.field_mut::<Matrix4<f32>>(0).unwrap();
            let pos_slice = it.field_mut::<Vector3<f32>>(1).unwrap();
            for i in 0..it.count() as usize {
                let mut m = mat_slice[i];
                for _ in 0..100 {
                    m = m.invert().unwrap_or(m);
                }
                mat_slice[i] = m;
                pos_slice[i] = m.transform_vector(pos_slice[i]);
            }
        }
    }
}
