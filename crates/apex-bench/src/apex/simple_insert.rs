use apex_core::prelude::*;
use cgmath::{Matrix4, Vector3};
use crate::{Transform, Position, Rotation, Velocity};

// SimpleInsert — создание мира и спавн 10K сущностей с 4 компонентами
// Регистрация компонентов происходит автоматически через spawn_many (get_or_register)
pub struct SimpleInsert;

impl SimpleInsert {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = World::new();

        // Пакетное создание 10 000 сущностей (регистрация компонентов — автоматическая)
        world.spawn_many(10_000, |_| (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(0.0, 0.0, 0.0)),
            Rotation(Vector3::new(0.0, 0.0, 0.0)),
            Velocity(Vector3::new(0.0, 0.0, 0.0)),
        ));

        // Предотвращение оптимизации компилятором
        std::hint::black_box(world);
    }
}