use apex_core::prelude::*;
use apex_macros::Component;

// CommandsInsert — структурное изменение через ОТЛОЖЕННЫЙ буфер Commands (insert компонента + apply),
// а не прямой world.insert. Путь Command::Insert + insert-group apply. Реальные системы добавляют
// компоненты через Commands из параллельного контекста. iter_batched: setup (мир с 10k A) вне
// измерения; run — записать 10k insert-команд B + apply (10k archetype-move'ов).
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
