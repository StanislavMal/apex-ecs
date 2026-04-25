use apex_core::prelude::*;
use cgmath::*;
use crate::{Position, Rotation, Velocity};

// HeavyCompute — тяжёлые вычисления: invert матрицы + transform_vector
// Компоненты: Matrix4<f32> (без обёртки, как в Bevy/Legion), Position, Rotation, Velocity
// Идентично Bevy и Legion по набору компонентов и операциям
pub struct HeavyCompute {
    world: World,
}

impl HeavyCompute {
    pub fn new() -> Self {
        let mut world = World::new();

        // 1000 сущностей с 4 компонентами (как в Bevy и Legion)
        world.spawn_many(1000, |_| (
            Matrix4::<f32>::from_angle_x(Rad(1.2)),
            Position(Vector3::unit_x()),
            Rotation(Vector3::unit_x()),
            Velocity(Vector3::unit_x()),
        ));

        Self { world }
    }

    pub fn run(&mut self) {
        // Запрос: изменяем Matrix4<f32> и Position (как в Bevy heavy_compute)
        let query = self.world.query_typed::<(Write<Matrix4<f32>>, Write<Position>)>();
        query.par_for_each_component(|(mat, pos)| {
            let mut m = *mat;
            for _ in 0..100 {
                m = m.invert().unwrap();
            }
            *mat = m;
            pos.0 = m.transform_vector(pos.0);
        });
    }
}