use apex_core::prelude::*;
use apex_macros::Component;

// Relations — OUR feature (legion has none, bevy 0.16+ has it as relationships). We build a parent + 10k
// children via `add_relation(ChildOf)`, then iterate `children_of`. Measures relation insertion (O(1)
// two-way index) + child traversal. Must be AAA-fast — a feature without performance is not AAA.
// Returns the number of children (honesty guard: both implementations must see 10000).
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

pub struct Relations;

impl Default for Relations {
    fn default() -> Self {
        Self::new()
    }
}

impl Relations {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) -> u64 {
        let mut world = World::new();
        let parent = world.spawn((A(0.0),));
        for i in 0..10_000 {
            let c = world.spawn((A(i as f32),));
            world.add_relation(c, ChildOf, parent);
        }
        let mut count = 0u64;
        for _c in world.targets_of(ChildOf, parent) {
            count += 1;
        }
        count
    }
}
