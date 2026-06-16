use apex_core::prelude::*;
use apex_macros::Component;

// DespawnRecursive — каскадный despawn иерархии (наша фича + bevy 0.16+). Родитель + 1000 детей
// через ChildOf; `despawn_recursive` сносит всё поддерево. Изолировано `iter_batched` (setup строит
// иерархию вне измерения). Не покрыто; реальный путь выгрузки сцен/смерти агрегатов.
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

pub fn setup() -> (World, Entity) {
    let mut world = World::new();
    let parent = world.spawn((A(0.0),));
    for i in 0..1000 {
        let c = world.spawn((A(i as f32),));
        world.add_relation(c, ChildOf, parent);
    }
    (world, parent)
}

pub fn run((mut world, parent): (World, Entity)) {
    world.despawn_recursive(ChildOf, parent);
}
