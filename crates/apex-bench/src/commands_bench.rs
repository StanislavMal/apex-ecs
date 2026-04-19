use apex_core::prelude::*;

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

#[derive(Clone, Copy)]
pub struct Health {
    pub current: f32,
    pub max: f32,
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
        world.register_component::<Health>();
        
        // Тест 1: Отложенный спавн через Commands
        let mut commands = apex_core::commands::Commands::new();
        
        // Создаём 10k сущностей отложенно
        for i in 0..10_000 {
            commands.spawn_bundle((
                Position { x: i as f32, y: 0.0 },
                Velocity { x: 1.0, y: 0.0 },
            ));
        }
        
        // Применяем команды
        commands.apply(&mut world);
        
        // Тест 2: Команды во время итерации (без мутаций мира)
        let mut entities = Vec::new();
        apex_core::query::Query::<apex_core::query::Read<Position>>::new(&world)
            .for_each(|entity, _| entities.push(entity));
        
        let mut commands2 = apex_core::commands::Commands::new();
        
        // Добавляем Health компонент к каждой второй сущности
        for (i, &entity) in entities.iter().enumerate() {
            if i % 2 == 0 {
                commands2.insert(entity, Health { current: 100.0, max: 100.0 });
            }
        }
        
        // Применяем команды
        commands2.apply(&mut world);
        
        // Тест 3: Удаление компонентов через команды
        let mut commands3 = apex_core::commands::Commands::new();
        
        // Удаляем Velocity у каждой третьей сущности
        for (i, &entity) in entities.iter().enumerate() {
            if i % 3 == 0 {
                commands3.remove::<Velocity>(entity);
            }
        }
        
        // Применяем команды
        commands3.apply(&mut world);
        
        // Тест 4: Batch операции (спавн с callback)
        let mut commands4 = apex_core::commands::Commands::new();
        
        // Спавним ещё 5k сущностей с разными компонентами
        for i in 0..5_000 {
            if i % 2 == 0 {
                commands4.spawn_bundle((
                    Position { x: i as f32, y: 0.0 },
                    Velocity { x: 1.0, y: 0.0 },
                ));
            } else {
                commands4.spawn_bundle((Position { x: i as f32, y: 0.0 },));
            }
        }
        
        // Применяем команды
        commands4.apply(&mut world);
        
        // Проверяем результаты через запросы
        let with_position = apex_core::query::Query::<apex_core::query::Read<Position>>::new(&world).len();
        let with_velocity = apex_core::query::Query::<apex_core::query::Read<Velocity>>::new(&world).len();
        let with_health = apex_core::query::Query::<apex_core::query::Read<Health>>::new(&world).len();
        
        std::hint::black_box(with_position);
        std::hint::black_box(with_velocity);
        std::hint::black_box(with_health);
    }
}