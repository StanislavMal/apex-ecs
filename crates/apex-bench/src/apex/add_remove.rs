use apex_core::prelude::*;
use apex_macros::Component;

#[derive(Component, Clone, Copy)]
pub struct A(pub f32);

#[derive(Component, Clone, Copy)]
pub struct B(pub f32);

pub struct AddRemove {
    world: World,
    entities: Vec<Entity>,
}

impl Default for AddRemove {
    fn default() -> Self {
        Self::new()
    }
}

impl AddRemove {
    pub fn new() -> Self {
        let mut world = World::new();
        // ✅ Кортеж из одного компонента реализует Bundle
        let entities = world.spawn_many(10_000, |_| (A(0.0),));

        Self { world, entities }
    }

    pub fn run(&mut self) {
        for &entity in &self.entities {
            self.world.insert(entity, B(1.0));
        }
        for &entity in &self.entities {
            self.world.remove::<B>(entity);
        }
    }
}