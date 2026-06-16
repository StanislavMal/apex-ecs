use apex_core::prelude::*;
use apex_macros::Component;

// CommandsSpawn — 10k спавнов через ОТЛОЖЕННЫЙ путь `Commands` (запись в буфер + apply),
// а не прямой `world.spawn`. Реальные игры гонят структурные изменения через Commands из
// параллельных систем — этот путь не покрыт simple_insert (тот прямой). Меряем запись команд
// + их применение (резервация + материализация).
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
