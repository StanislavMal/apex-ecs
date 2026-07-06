use apex_core::prelude::*;
use cgmath::{Matrix4, Rad, Vector3, SquareMatrix, Transform as _};
use crate::{Position, Rotation, Velocity};

// HeavyCompute — heavy computation: matrix invert + transform_vector
// CachedQuery is cached via QueryCache inside query()
pub struct HeavyCompute {
    world: World,
}

impl Default for HeavyCompute {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn run(&mut self) {
        // The matrix is read (Read), not written back: otherwise the result degenerates over ~1M
        // inversions (random-walk FP drift → NaN/denormals → cgmath None / on x86 denormal
        // arithmetic ~100× slower → a nondeterministic death-spiral). Reading a healthy seed
        // on every iteration makes the bench deterministic and fair for all engines; the load
        // (100 inversions per entity + position write) is preserved. unwrap_or — a safeguard.
        self.world.query_mut::<(Read<Matrix4<f32>>, Write<Position>)>()
            .par_for_each_mut(|_, (mat, mut pos)| {
                let mut m = *mat;
                for _ in 0..100 {
                    m = m.invert().unwrap_or(m);
                }
                pos.0 = m.transform_vector(pos.0);
            });
    }
}
