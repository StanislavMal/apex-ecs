use apex_core::prelude::*;
use apex_macros::Component;

// Relations — НАША фича (у legion нет, у bevy 0.16+ есть как relationships). Строим родитель + 10k
// детей через `add_relation(ChildOf)`, затем итерируем `children_of`. Меряет вставку связи (O(1)
// двусторонний индекс) + обход детей. Должна быть AAA-быстрой — фича без производительности не AAA.
// Возвращает число детей (страж честности: обе реализации обязаны увидеть 10000).
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
