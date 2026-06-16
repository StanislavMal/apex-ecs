use apex_core::prelude::*;
use cgmath::{Matrix4, Rad, Vector3, SquareMatrix, Transform as _};
use crate::{Position, Rotation, Velocity};

// HeavyCompute — тяжёлые вычисления: invert матрицы + transform_vector
// CachedQuery кешируется через QueryCache внутри query()
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

    pub fn run(&self) {
        // Матрица читается (Read), а не пишется обратно: иначе результат вырождается за ~1M
        // инверсий (random-walk FP-дрейф → NaN/денормалы → cgmath None / на x86 денормальная
        // арифметика ~100× медленнее → недетерминированный death-spiral). Чтение healthy-seed
        // каждую итерацию делает бенч детерминированным и честным для всех движков; нагрузка
        // (100 инверсий на сущность + запись позиции) сохранена. unwrap_or — страховка.
        self.world.query::<(Read<Matrix4<f32>>, Write<Position>)>()
            .par_for_each(|_, (mat, mut pos)| {
                let mut m = *mat;
                for _ in 0..100 {
                    m = m.invert().unwrap_or(m);
                }
                pos.0 = m.transform_vector(pos.0);
            });
    }
}
