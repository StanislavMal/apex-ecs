use apex_core::prelude::*;
use crate::{Transform, Position, Rotation, Velocity};
use cgmath::{Matrix4, Vector3};

// SimpleIter — итерация по 10K сущностей, Position += Velocity
// World хранится как owned, query_typed() создаётся на каждой итерации
pub struct SimpleIter {
    world: World,
}

impl SimpleIter {
    pub fn new() -> Self {
        let mut world = World::new();

        // Регистрация компонентов происходит автоматически через spawn_many
        world.spawn_many(10_000, |_| (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(0.0, 0.0, 0.0)),
            Rotation(Vector3::new(0.0, 0.0, 0.0)),
            Velocity(Vector3::new(1.0, 0.0, 0.0)),
        ));

        Self { world }
    }

    pub fn run(&self) {
        self.world.query_typed::<(Read<Velocity>, Write<Position>)>()
            .for_each_component(|(vel, pos)| {
                pos.0 += vel.0;
            });
    }
}