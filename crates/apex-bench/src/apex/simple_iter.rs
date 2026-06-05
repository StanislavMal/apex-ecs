use apex_core::prelude::*;
use crate::{Transform, Position, Rotation, Velocity};
use cgmath::{Matrix4, Vector3};

// SimpleIter — итерация по 10K сущностей, Position += Velocity
// CachedQuery кешируется через QueryCache внутри query_typed()
pub struct SimpleIter {
    world: World,
}

impl SimpleIter {
    pub fn new() -> Self {
        let mut world = World::new();

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
            .for_each(|_, (vel, mut pos)| {
                pos.0 += vel.0;
            });
    }
}
