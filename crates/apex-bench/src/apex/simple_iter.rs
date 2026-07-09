use apex_core::prelude::*;
use crate::{Transform, Position, Rotation, Velocity};
use cgmath::{Matrix4, Vector3};

// SimpleIter — iteration over 10K entities, Position += Velocity
// CachedQuery is cached via QueryCache inside query()
pub struct SimpleIter {
    world: World,
    // W2-0/W2-0.5: per-state query for the chunked variant (Bevy QueryState model)
    state: QueryState<(&'static Velocity, &'static mut Position)>,
}

impl Default for SimpleIter {
    fn default() -> Self {
        Self::new()
    }
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

        Self { world, state: QueryState::new() }
    }

    /// Per-element iteration via a PERSISTENT `QueryState` — the idiomatic
    /// fast apex path, a direct analog of bevy `QueryState::iter_mut` (the bevy bench
    /// also keeps state between calls). The previous variant called
    /// `world.query()` on every iteration (rebuilding the archetype match) — this was
    /// an unfair handicap of apex against the caching bevy.
    pub fn run(&mut self) {
        self.state.query_mut(&mut self.world).for_each_mut(|_, (vel, mut pos)| {
            pos.0 += vel.0;
        });
    }

    /// W2-0.5: dense chunk iteration (column slices + stamp_range).
    pub fn run_chunked(&mut self) {
        self.state.query_mut(&mut self.world).for_each_chunk_mut(|_, (vel, pos)| {
            for (p, v) in pos.iter_mut().zip(vel) {
                p.0 += v.0;
            }
        });
    }
}
