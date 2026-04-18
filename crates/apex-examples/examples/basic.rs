use apex_core::prelude::*;
use apex_scheduler::Scheduler;

#[derive(Debug, Clone)] struct Position { x: f32, y: f32 }
#[derive(Debug, Clone)] struct Velocity { x: f32, y: f32 }
#[derive(Debug, Clone)] struct Health   { current: f32, max: f32 }
#[derive(Debug, Clone)] struct Player;
#[derive(Debug, Clone)] struct Name(pub &'static str);

fn main() {
    println!("=== Apex ECS - Basic Example ===\n");

    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Player>();
    world.register_component::<Name>();

    let player = world.spawn_bundle((
        Position { x: 0.0, y: 0.0 },
        Velocity { x: 1.0, y: 0.5 },
        Health { current: 100.0, max: 100.0 },
        Player,
        Name("Hero"),
    ));

    let _enemy1 = world.spawn_bundle((
        Position { x: 10.0, y: 5.0 },
        Velocity { x: -0.5, y: 0.0 },
        Health { current: 50.0, max: 50.0 },
        Name("Goblin"),
    ));

    let _enemy2 = world.spawn_bundle((
        Position { x: -5.0, y: 3.0 },
        Velocity { x: 0.3, y: -0.2 },
        Health { current: 75.0, max: 75.0 },
        Name("Orc"),
    ));

    println!("Created {} entities", world.entity_count());
    println!("Archetypes: {}\n", world.archetype_count());

    // ── get / get_mut ──────────────────────────────────────────
    let pos = world.get::<Position>(player).unwrap();
    println!("Player position: ({}, {})", pos.x, pos.y);

    if let Some(pos) = world.get_mut::<Position>(player) {
        pos.x += 5.0;
        pos.y += 2.0;
    }
    let pos = world.get::<Position>(player).unwrap();
    println!("Player moved to: ({}, {})\n", pos.x, pos.y);

    // ── CachedQuery ────────────────────────────────────────────
    println!("=== CachedQuery Demo ===");
    world.query_typed::<(Read<Name>, Read<Health>)>()
        .for_each(|entity, (name, hp)| {
            println!("  {entity}: {} — HP {}/{}", name.0, hp.current, hp.max);
        });

    // ── Relations: ChildOf иерархия ────────────────────────────
    println!("\n=== Relations: Scene Hierarchy ===");

    let scene_root = world.spawn_bundle((Name("SceneRoot"),));
    let node_a    = world.spawn_bundle((Name("NodeA"), Position { x: 1.0, y: 0.0 }));
    let node_b    = world.spawn_bundle((Name("NodeB"), Position { x: 2.0, y: 0.0 }));
    let node_c    = world.spawn_bundle((Name("NodeC"), Position { x: 3.0, y: 0.0 }));
    let leaf      = world.spawn_bundle((Name("Leaf"),  Position { x: 4.0, y: 0.0 }));

    // Строим иерархию: root → A, B; A → C; C → leaf
    world.add_relation(node_a, ChildOf, scene_root);
    world.add_relation(node_b, ChildOf, scene_root);
    world.add_relation(node_c, ChildOf, node_a);
    world.add_relation(leaf,   ChildOf, node_c);

    println!("Archetypes after relations: {}", world.archetype_count());

    // Прямые дети scene_root
    println!("\nDirect children of SceneRoot:");
    for child in world.children_of(ChildOf, scene_root) {
        if let Some(name) = world.get::<Name>(child) {
            println!("  {child}: {}", name.0);
        }
    }

    // Query: все entity у которых есть ChildOf(scene_root) + Position
    println!("\nChildren of SceneRoot with Position:");
    for (entity, pos) in world.query_relation::<ChildOf, Read<Position>>(ChildOf, scene_root) {
        if let Some(name) = world.get::<Name>(entity) {
            println!("  {entity} ({}): pos=({:.1}, {:.1})", name.0, pos.x, pos.y);
        }
    }

    // has_relation проверка
    println!("\nhas_relation checks:");
    println!("  node_a ChildOf scene_root: {}", world.has_relation(node_a, ChildOf, scene_root));
    println!("  node_b ChildOf node_a:     {}", world.has_relation(node_b, ChildOf, node_a));
    println!("  leaf   ChildOf node_c:     {}", world.has_relation(leaf,   ChildOf, node_c));

    // Удаляем relation
    world.remove_relation(node_b, ChildOf, scene_root);
    println!("\nAfter remove_relation(node_b, ChildOf, scene_root):");
    println!("  node_b ChildOf scene_root: {}", world.has_relation(node_b, ChildOf, scene_root));
    println!("  Direct children of SceneRoot now:");
    for child in world.children_of(ChildOf, scene_root) {
        if let Some(name) = world.get::<Name>(child) {
            println!("    {child}: {}", name.0);
        }
    }

    // ── Commands ───────────────────────────────────────────────
    println!("\n=== Commands Demo ===");
    let mut cmds = Commands::new();

    // Убиваем всех с HP <= 50 через Commands (безопасно во время итерации)
    Query::<(Read<Health>, Read<Name>)>::new(&world).for_each(|entity, (hp, name)| {
        if hp.current <= 50.0 {
            println!("  Queuing despawn for {} (HP {})", name.0, hp.current);
            cmds.despawn(entity);
        }
    });
    cmds.apply(&mut world);
    println!("  Entities after despawn: {}", world.entity_count());

    // ── Scheduler ──────────────────────────────────────────────
    println!("\n=== Scheduler ===");
    let mut scheduler = Scheduler::new();
    let movement_id = scheduler.add_system("movement",     |_| println!("  [movement] running..."));
    let health_id   = scheduler.add_system("health_check", |_| println!("  [health_check] running..."));
    let render_id   = scheduler.add_system("render",       |_| println!("  [render] running..."));
    scheduler.add_dependency(health_id, movement_id);
    scheduler.add_dependency(render_id, health_id);
    scheduler.compile().unwrap();
    println!("Running {} systems:", scheduler.system_count());
    scheduler.run(&mut world);

    // ── Component remove / despawn ─────────────────────────────
    println!("\n=== Component Remove / Despawn ===");
    println!("Player alive: {}", world.is_alive(player));
    world.remove::<Player>(player);
    println!("After remove<Player>: has Player tag = {}", world.get::<Player>(player).is_some());
    world.despawn(player);
    println!("After despawn: alive = {}", world.is_alive(player));
    println!("Remaining entities: {}", world.entity_count());
}
