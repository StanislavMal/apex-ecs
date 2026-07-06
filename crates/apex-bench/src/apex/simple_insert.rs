use apex_core::prelude::*;
use cgmath::{Matrix4, Vector3};
use crate::{Transform, Position, Rotation, Velocity};

// SimpleInsert — create a world and spawn 10K entities with 4 components
// Component registration happens automatically via spawn_many (get_or_register)
//
// NOTE: we do not use std::hint::black_box(world), to be on par
//       with the Bevy/Legion benchmarks (they also do not use black_box).
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

        // Batch creation of 10,000 entities (component registration is automatic)
        // unit_x() values — for uniformity with the Bevy/Legion benchmarks
        world.spawn_many(10_000, |_| (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::unit_x()),
            Rotation(Vector3::unit_x()),
            Velocity(Vector3::unit_x()),
        ));
    }
}