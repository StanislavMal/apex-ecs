use apex_core::prelude::*;
use cgmath::{Matrix4, Vector3};

// Компоненты (без импорта из crate, определяем локально для ясности)
pub struct Transform(pub Matrix4<f32>);
pub struct Position(pub Vector3<f32>);
pub struct Rotation(pub Vector3<f32>);
pub struct Velocity(pub Vector3<f32>);

// Бенчмарк не хранит мир (как в Bevy/Legion)
pub struct SimpleInsert;

impl SimpleInsert {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = World::new();

        // Регистрация компонентов — обязательна в Apex
        world.register_component::<Transform>();
        world.register_component::<Position>();
        world.register_component::<Rotation>();
        world.register_component::<Velocity>();

        // Пакетное создание 10 000 сущностей
        world.spawn_many(10_000, |_| (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(0.0, 0.0, 0.0)),
            Rotation(Vector3::new(0.0, 0.0, 0.0)),
            Velocity(Vector3::new(0.0, 0.0, 0.0)),
        ));

        // Предотвращение оптимизации (Criterion обычно сам добавляет black_box,
        // но для единообразия оставим)
        std::hint::black_box(world);
    }
}