use apex_core::prelude::*;
use apex_macros::Component;

// CommandsSpawn — 10k spawns via the DEFERRED `Commands` path (write to a buffer + apply),
// not the direct `world.spawn`. Real games drive structural changes through Commands from
// parallel systems — this path is not covered by simple_insert (which is direct). We measure command
// recording + their application (reservation + materialization).
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

pub struct CommandsSpawn;

impl Default for CommandsSpawn {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandsSpawn {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = World::new();
        let mut cmds = Commands::new();
        for _ in 0..10_000 {
            cmds.spawn((A(0.0), B(0.0)));
        }
        cmds.apply(&mut world);
    }
}
