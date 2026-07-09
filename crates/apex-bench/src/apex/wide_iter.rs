use apex_core::prelude::*;
use apex_macros::Component;

// WideIter — iteration of a WIDE query (5 components: 4 read + 1 write) over 10k. Real systems
// read many components, not 2 (like simple_iter). Measures the fetch of a wide tuple + per-row
// construction. Persistent QueryState (like bevy).
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);
#[derive(Component, Clone, Copy)]
pub struct B(pub f32);
#[derive(Component, Clone, Copy)]
pub struct C(pub f32);
#[derive(Component, Clone, Copy)]
pub struct D(pub f32);
#[derive(Component, Clone, Copy)]
pub struct E(pub f32);

pub struct WideIter {
    world: World,
    state: QueryState<(&'static B, &'static C, &'static D, &'static E, &'static mut A)>,
}

impl Default for WideIter {
    fn default() -> Self {
        Self::new()
    }
}

impl WideIter {
    pub fn new() -> Self {
        let mut world = World::new();
        world.spawn_many(10_000, |i| {
            (
                A(0.0),
                B(i as f32),
                C(1.0),
                D(2.0),
                E(3.0),
            )
        });
        Self {
            world,
            state: QueryState::new(),
        }
    }

    pub fn run(&mut self) {
        self.state
            .query_mut(&mut self.world)
            .for_each_mut(|_, (b, c, d, e, mut a)| {
                a.0 += b.0 + c.0 + d.0 + e.0;
            });
    }
}
