use apex_core::prelude::*;
use cgmath::{Matrix4, Vector3};
use crate::{Transform, Position, Rotation, Velocity};

// SimpleInsert — создание мира и спавн 10K сущностей с 4 компонентами
// Регистрация компонентов происходит автоматически через spawn_many (get_or_register)
//
// NOTE: не используем std::hint::black_box(world), чтобы быть наравне
//       с Bevy/Legion бенчмарками (они тоже не используют black_box).
pub struct SimpleInsert;

impl Default for SimpleInsert {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleInsert {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = World::new();

        // Пакетное создание 10 000 сущностей (регистрация компонентов — автоматическая)
        // Значения unit_x() — для единообразия с Bevy/Legion бенчмарками
        world.spawn_many(10_000, |_| (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::unit_x()),
            Rotation(Vector3::unit_x()),
            Velocity(Vector3::unit_x()),
        ));
    }
}