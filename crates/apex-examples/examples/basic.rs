use apex_core::prelude::*;
use apex_scheduler::Scheduler;

#[derive(Debug, Clone)]
struct Position { x: f32, y: f32 }

#[derive(Debug, Clone)]
struct Velocity { x: f32, y: f32 }

#[derive(Debug, Clone)]
struct Health { current: f32, max: f32 }

#[derive(Debug, Clone)]
struct Player;

fn main() {
    println!("=== Apex ECS - Basic Example ===\n");

    let mut world = World::new();

    // Регистрируем компоненты
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Player>();

    // spawn_bundle — один архетипный переход, без промежуточных архетипов
    let player = world.spawn_bundle((
        Position { x: 0.0, y: 0.0 },
        Velocity { x: 1.0, y: 0.5 },
        Health { current: 100.0, max: 100.0 },
        Player,
    ));

    let _enemy1 = world.spawn_bundle((
        Position { x: 10.0, y: 5.0 },
        Velocity { x: -0.5, y: 0.0 },
        Health { current: 50.0, max: 50.0 },
    ));

    let _enemy2 = world.spawn_bundle((
        Position { x: -5.0, y: 3.0 },
        Velocity { x: 0.3, y: -0.2 },
        Health { current: 75.0, max: 75.0 },
    ));

    println!("Created {} entities", world.entity_count());
    // Теперь 3 архетипа: пустой + [Pos,Vel,Hp,Player] + [Pos,Vel,Hp]
    println!("Archetypes: {}\n", world.archetype_count());

    let pos = world.get::<Position>(player).unwrap();
    println!("Player position: ({}, {})", pos.x, pos.y);

    if let Some(pos) = world.get_mut::<Position>(player) {
        pos.x += 5.0;
        pos.y += 2.0;
    }

    let pos = world.get::<Position>(player).unwrap();
    println!("Player moved to: ({}, {})\n", pos.x, pos.y);

    // Планировщик
    let mut scheduler = Scheduler::new();

    let movement_id = scheduler.add_system("movement", |_world| {
        println!("  [movement system] running...");
    });
    let health_id = scheduler.add_system("health_check", |_world| {
        println!("  [health_check system] running...");
    });
    let render_id = scheduler.add_system("render", |_world| {
        println!("  [render system] running...");
    });

    scheduler.add_dependency(health_id, movement_id);
    scheduler.add_dependency(render_id, health_id);
    scheduler.compile().unwrap();

    println!("Running scheduler ({} systems):", scheduler.system_count());
    scheduler.run(&mut world);

    // Graph test
    println!("\n=== Graph Test ===");
    use apex_graph::Graph;

    let mut graph: Graph<&str, &str> = Graph::new();
    let physics   = graph.add_node("Physics");
    let collision = graph.add_node("Collision");
    let ai        = graph.add_node("AI");
    let render    = graph.add_node("Render");
    let audio     = graph.add_node("Audio");

    graph.add_edge(physics, collision, "needs");
    graph.add_edge(collision, render, "needs");
    graph.add_edge(ai, render, "needs");

    let levels = graph.parallel_levels().unwrap();
    println!("Parallel execution levels:");
    for (i, level) in levels.iter().enumerate() {
        let names: Vec<&str> = level
            .iter()
            .filter_map(|&idx| graph.node_data(idx).copied())
            .collect();
        println!("  Level {}: {:?}", i, names);
    }

    // Component remove / despawn
    println!("\n=== Component Remove Test ===");
    println!("Player alive: {}", world.is_alive(player));
    println!("Player has Player tag: {}", world.get::<Player>(player).is_some());

    world.remove::<Player>(player);
    println!("After remove - Player has Player tag: {}", world.get::<Player>(player).is_some());

    world.despawn(player);
    println!("After despawn - Player alive: {}", world.is_alive(player));
    println!("Remaining entities: {}", world.entity_count());
}
