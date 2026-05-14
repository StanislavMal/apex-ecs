use apex_core::prelude::*;
use cgmath::{Matrix4, Rad, Vector3, SquareMatrix, Transform as _};
use crate::{Position, Rotation, Velocity};

// HeavyCompute — тяжёлые вычисления: invert матрицы + transform_vector
// CachedQuery кешируется через QueryCache внутри query_typed()
pub struct HeavyCompute {
    world: World,
}

impl HeavyCompute {
    pub fn new() -> Self {
        let mut world = World::new();

        world.spawn_many(1000, |_| (
            Matrix4::<f32>::from_angle_x(Rad(1.2)),
            Position(Vector3::unit_x()),
            Rotation(Vector3::unit_x()),
            Velocity(Vector3::unit_x()),
        ));

        Self { world }
    }

    pub fn run(&self) {
        self.world.query_typed::<(Write<Matrix4<f32>>, Write<Position>)>()
            .par_for_each(|_, (mat, pos)| {
                let mut m = *mat;
                for _ in 0..100 {
                    m = m.invert().unwrap();
                }
                *mat = m;
                pos.0 = m.transform_vector(pos.0);
            });
    }
}
