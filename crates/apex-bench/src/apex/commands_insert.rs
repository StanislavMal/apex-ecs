use apex_core::prelude::*;
use apex_macros::Component;

// CommandsInsert — a structural change through the DEFERRED Commands buffer (component insert + apply),
// rather than a direct world.insert. Path: Command::Insert + insert-group apply. Real systems add
// components through Commands from a parallel context. iter_batched: setup (a world with 10k A) is
// outside the measurement; run — record 10k insert commands for B + apply (10k archetype moves).
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

pub fn setup() -> (World, Vec<Entity>) {
    let mut world = World::new();
    let entities = world.spawn_many(10_000, |_| (A(0.0),));
    (world, entities)
}

pub fn run((mut world, entities): (World, Vec<Entity>)) {
    let mut cmds = Commands::new();
    for &e in &entities {
        cmds.insert(e, B(1.0));
    }
    cmds.apply(&mut world);
}
