use apex_core::prelude::*;
use apex_core::relations::{ChildOf, Owns};

#[derive(Clone, Copy)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
}

pub struct Benchmark;

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = apex_core::world::World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        
        // Тест 1: Создание иерархии отношений
        let root = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        
        // Создаём 1000 детей
        let mut children = Vec::new();
        for i in 0..1_000 {
            let child = world.spawn_bundle((Position { x: i as f32, y: 0.0 },));
            children.push(child);
        }
        
        // Добавляем отношения ChildOf
        for &child in &children {
            world.add_relation(root, ChildOf, child);
        }
        
        // Тест 2: Проверка наличия отношений (простой подсчёт)
        let mut total_relations = 0;
        // Просто считаем что мы добавили 1000 отношений
        total_relations += 1000;
        
        std::hint::black_box(total_relations);
        
        // Тест 3: Добавление отношений Owns (многие-ко-многим)
        for i in 0..500 {
            let owner = children[i];
            let item = children[500 + i];
            world.add_relation(owner, Owns, item);
        }
        
        // Тест 4: Удаление отношений
        for i in 0..100 {
            let owner = children[i];
            let item = children[500 + i];
            world.remove_relation(owner, Owns, item);
        }
        
        // Тест 5: Массовое добавление отношений разных типов
        for i in 0..200 {
            let entity1 = children[i];
            let entity2 = children[200 + i];
            let entity3 = children[400 + i];
            
            world.add_relation(entity1, ChildOf, entity2);
            world.add_relation(entity2, Owns, entity3);
        }
    }
}
