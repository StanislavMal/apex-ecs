use apex_core::prelude::*;
use crate::{Transform, Position, Rotation, Velocity};
use cgmath::{Matrix4, Vector3};

pub struct SimpleIter {
    // Только для удержания памяти, query использует внутреннюю мутабельность
    _world: &'static World,
    query: CachedQuery<'static, (Read<Velocity>, Write<Position>)>,
}

impl SimpleIter {
    pub fn new() -> Self {
        let mut world = Box::new(World::new());

        // Обязательная регистрация компонентов
        world.register_component::<Transform>();
        world.register_component::<Position>();
        world.register_component::<Rotation>();
        world.register_component::<Velocity>();

        world.spawn_many(10_000, |_| (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(0.0, 0.0, 0.0)),
            Rotation(Vector3::new(0.0, 0.0, 0.0)),
            Velocity(Vector3::new(1.0, 0.0, 0.0)),
        ));

        // Безопасное получение 'static времени жизни
        let world = Box::leak(world);
        let query = CachedQuery::<(Read<Velocity>, Write<Position>)>::new(world, Tick::ZERO);

        Self { _world: world, query }
    }

    pub fn run(&self) {
        self.query.for_each_component(|(vel, pos)| {
            pos.0 += vel.0;
        });
    }
}