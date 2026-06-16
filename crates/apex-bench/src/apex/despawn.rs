use apex_core::prelude::*;
use apex_macros::Component;

// Despawn — удаление 10k сущностей. Изолировано через criterion `iter_batched`:
// `setup` создаёт населённый мир (НЕ в измеряемой части), `run` только деспавнит.
// Путь despawn не покрыт другими бенчами; меряет swap-remove из колонок + освобождение слотов.
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

/// Создать населённый мир (вне измерения).
pub fn setup() -> (World, Vec<Entity>) {
    let mut world = World::new();
    let entities = world.spawn_many(10_000, |_| (A(0.0), B(0.0)));
    (world, entities)
}

/// Измеряемая часть: деспавн всех сущностей.
pub fn run((mut world, entities): (World, Vec<Entity>)) {
    for e in entities {
        world.despawn(e);
    }
}
