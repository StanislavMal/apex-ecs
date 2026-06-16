use apex_core::prelude::*;
use apex_macros::Component;

// GetComponent — random-access `world.get::<T>(entity)` по 10k сущностям. Меряет путь
// entity → location (generational records) → archetype → колонка → строка. Не покрыт другими
// бенчами (итерация идёт по архетипам, не по entity-id). Горячий путь геймплея (доступ по хэндлу).
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

pub struct GetComponent {
    world: World,
    entities: Vec<Entity>,
}

impl Default for GetComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl GetComponent {
    pub fn new() -> Self {
        let mut world = World::new();
        let entities = world.spawn_many(10_000, |i| (A(i as f32), B(0.0)));
        Self { world, entities }
    }

    pub fn run(&self) -> f32 {
        let mut sum = 0.0f32;
        for &e in &self.entities {
            sum += self.world.get::<A>(e).unwrap().0;
        }
        sum
    }
}
