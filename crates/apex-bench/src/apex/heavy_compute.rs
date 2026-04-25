use apex_core::prelude::*;
use cgmath::*;
// Не импортируем cgmath::Transform, чтобы не конфликтовать с компонентом
use crate::{Transform as ApexTransform, Position, Velocity};

pub struct HeavyCompute {
    world: Box<World>,
}

impl HeavyCompute {
    pub fn new() -> Self {
        let mut world = Box::new(World::new());
        world.register_component::<ApexTransform>();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        // 1000 сущностей для честности
        world.spawn_many(1000, |_| (
            ApexTransform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(0.0, 0.0, 0.0)),
            Velocity(Vector3::new(1.0, 0.0, 0.0)),
        ));

        Self { world }
    }

    pub fn run(&mut self) {
        // Берём cached-запрос
        let query = self.world.query_typed::<(Write<ApexTransform>, Read<Position>, Write<Velocity>)>();
        query.par_for_each_component(|(mut transform, pos, mut vel)| {
            let mut m = transform.0;
            for _ in 0..100 {
                m = m.invert().unwrap();
            }
            transform.0 = m;
            // Вызов метода трейта через полный путь
            vel.0 = cgmath::Transform::transform_vector(&m, pos.0);
        });
    }
}