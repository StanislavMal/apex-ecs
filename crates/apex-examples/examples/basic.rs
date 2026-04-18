use apex_core::prelude::*;
use apex_scheduler::Scheduler;

// ── Компоненты ────────────────────────────────────────────────
#[derive(Debug, Clone)] struct Position  { x: f32, y: f32 }
#[derive(Debug, Clone)] struct Velocity  { x: f32, y: f32 }
#[derive(Debug, Clone)] struct Health    { current: f32, max: f32 }
#[derive(Debug, Clone)] struct Player;
#[derive(Debug, Clone)] struct Name(pub &'static str);

// ── Ресурсы ───────────────────────────────────────────────────
#[derive(Debug)]
struct GameConfig {
    gravity:    f32,
    time_scale: f32,
}

#[derive(Debug, Default)]
struct FrameStats {
    frame:        u32,
    entities_max: usize,
}

// ── События ───────────────────────────────────────────────────
#[derive(Debug, Clone)]
struct DamageEvent {
    target:  Entity,
    amount:  f32,
    source:  &'static str,
}

#[derive(Debug, Clone)]
struct DeathEvent {
    entity: Entity,
    name:   &'static str,
}

fn main() {
    println!("=== Apex ECS - Basic Example ===\n");

    let mut world = World::new();

    // Регистрируем компоненты
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Player>();
    world.register_component::<Name>();

    // ── [NEW] Resources ───────────────────────────────────────
    println!("=== Resources ===");

    world.insert_resource(GameConfig {
        gravity:    9.8,
        time_scale: 1.0,
    });
    world.insert_resource(FrameStats::default());

    // Читаем ресурс
    let cfg = world.resource::<GameConfig>();
    println!("GameConfig: gravity={}, time_scale={}", cfg.gravity, cfg.time_scale);

    // Изменяем ресурс
    world.resource_mut::<GameConfig>().time_scale = 0.5;
    println!(
        "After slow-mo: time_scale={}",
        world.resource::<GameConfig>().time_scale
    );

    // try_resource — безопасная версия без паники
    if let Some(stats) = world.try_resource::<FrameStats>() {
        println!("FrameStats: frame={}", stats.frame);
    }

    // has_resource
    println!("has GameConfig: {}", world.has_resource::<GameConfig>());
    println!("has Score:      {}\n", world.has_resource::<u32>());

    // ── [NEW] Events ──────────────────────────────────────────
    println!("=== Events ===");

    world.add_event::<DamageEvent>();
    world.add_event::<DeathEvent>();

    // Спавним entity для тестирования событий
    let player = world.spawn_bundle((
        Position { x: 0.0, y: 0.0 },
        Velocity { x: 1.0, y: 0.5 },
        Health   { current: 100.0, max: 100.0 },
        Player,
        Name("Hero"),
    ));
    let goblin = world.spawn_bundle((
        Position  { x: 10.0, y: 5.0 },
        Velocity  { x: -0.5, y: 0.0 },
        Health    { current: 50.0, max: 50.0 },
        Name("Goblin"),
    ));
    let orc = world.spawn_bundle((
        Position { x: -5.0, y: 3.0 },
        Velocity { x: 0.3, y: -0.2 },
        Health   { current: 75.0, max: 75.0 },
        Name("Orc"),
    ));

    println!("Spawned {} entities", world.entity_count());

    // Отправляем события в текущем тике
    world.send_event(DamageEvent { target: goblin, amount: 30.0, source: "sword" });
    world.send_event(DamageEvent { target: orc,    amount: 15.0, source: "arrow" });
    world.send_event(DamageEvent { target: player, amount:  5.0, source: "trap"  });

    // Читаем события ТЕКУЩЕГО тика (отправитель и читатель в одной фазе)
    println!("\nEvents in current tick:");
    let damage_events: Vec<DamageEvent> = world
        .events::<DamageEvent>()
        .iter_current()
        .cloned()
        .collect();

    for ev in &damage_events {
        println!("  DamageEvent: {:?} took {:.0} dmg from {}", ev.target, ev.amount, ev.source);
    }

    // Применяем урон вручную
    for ev in &damage_events {
        if let Some(hp) = world.get_mut::<Health>(ev.target) {
            hp.current -= ev.amount;
            println!(
                "  Applied: {:?} hp now {:.0}",
                ev.target, hp.current
            );
        }
    }

    // world.tick() — события переходят в previous, current очищается
    world.tick();
    println!("\nAfter world.tick():");
    println!(
        "  current events: {}",
        world.events::<DamageEvent>().len_current()
    );
    println!(
        "  previous events: {}",
        world.events::<DamageEvent>().len_previous()
    );

    // Теперь читаем из previous (стандартный режим для систем)
    println!("\nReading previous tick's damage events:");
    for ev in world.events::<DamageEvent>().iter_previous() {
        println!("  (prev) {:?} took {:.0} from {}", ev.target, ev.amount, ev.source);
    }

    // Отправляем DeathEvent для тех у кого HP <= 0
    // (имитируем систему death_check)
    {
        // Собираем deaths через Query + отправляем события
        let dead: Vec<(Entity, &'static str)> = {
            let mut result = Vec::new();
            Query::<(Read<Health>, Read<Name>)>::new(&world)
                .for_each(|entity, (hp, name)| {
                    if hp.current <= 0.0 {
                        result.push((entity, name.0));
                    }
                });
            result
        };
        for (entity, name) in dead {
            world.send_event(DeathEvent { entity, name });
        }
    }

    println!(
        "\nDeath events sent: {}",
        world.events::<DeathEvent>().len_current()
    );
    for ev in world.events::<DeathEvent>().iter_current() {
        println!("  {:?} ('{}') has died!", ev.entity, ev.name);
    }

    // Ещё один тик — death events уходят в previous
    world.tick();
    println!(
        "\nAfter second tick — death events in previous: {}",
        world.events::<DeathEvent>().len_previous()
    );
    // После третьего тика события исчезнут полностью
    world.tick();
    println!(
        "After third tick  — death events in previous: {} (gone)",
        world.events::<DeathEvent>().len_previous()
    );

    // ── Resources + Events в Scheduler ───────────────────────
    println!("\n=== Scheduler with Resources & Events ===");

    // Обновляем stats ресурс
    world.resource_mut::<FrameStats>().entities_max = world.entity_count();

    let mut scheduler = Scheduler::new();

    // Система: обновление позиций с учётом time_scale из ресурса
    let movement_id = scheduler.add_system("movement", |world: &mut World| {
        let time_scale = world.resource::<GameConfig>().time_scale;
        Query::<(Read<Velocity>, Write<Position>)>::new(world)
            .for_each_component(|(vel, pos)| {
                pos.x += vel.x * time_scale;
                pos.y += vel.y * time_scale;
            });
        println!("  [movement] applied (time_scale={time_scale})");
    }).id();

    // Система: health check — читает Health, пишет DeathEvent через world
    let health_id = scheduler.add_system("health_check", |world: &mut World| {
        let dead: Vec<(Entity, f32)> = {
            let mut result = Vec::new();
            Query::<Read<Health>>::new(world).for_each(|e, hp| {
                if hp.current <= 0.0 {
                    result.push((e, hp.current));
                }
            });
            result
        };
        if dead.is_empty() {
            println!("  [health_check] all alive");
        } else {
            println!("  [health_check] {} dead entities", dead.len());
        }
    }).id();

    // Система: frame stats update
    let stats_id = scheduler.add_system("stats_update", |world: &mut World| {
        let count = world.entity_count();
        let stats = world.resource_mut::<FrameStats>();
        stats.frame += 1;
        stats.entities_max = stats.entities_max.max(count);
        println!(
            "  [stats_update] frame={} entities={}",
            stats.frame, count
        );
    }).id();

    // Система: event processor — читает damage события из previous
    let _event_id = scheduler.add_system("event_processor", |world: &mut World| {
        let count = world.events::<DamageEvent>().len_previous();
        println!("  [event_processor] processing {count} previous damage events");
    }).id();

    // Зависимости: health_check после movement, stats после health_check
    scheduler.add_dependency(health_id, movement_id);
    scheduler.add_dependency(stats_id,  health_id);

    scheduler.compile().unwrap();
    println!("Systems: {}", scheduler.system_count());
    println!("Plan:\n{}", scheduler.debug_plan());

    println!("Running scheduler:");
    scheduler.run(&mut world);

    let stats = world.resource::<FrameStats>();
    println!("\nFinal FrameStats: frame={}, entities_max={}", stats.frame, stats.entities_max);

    // ── Relations ─────────────────────────────────────────────
    println!("\n=== Relations: Scene Hierarchy ===");

    let scene_root = world.spawn_bundle((Name("SceneRoot"),));
    let node_a     = world.spawn_bundle((Name("NodeA"), Position { x: 1.0, y: 0.0 }));
    let node_b     = world.spawn_bundle((Name("NodeB"), Position { x: 2.0, y: 0.0 }));
    let node_c     = world.spawn_bundle((Name("NodeC"), Position { x: 3.0, y: 0.0 }));
    let leaf       = world.spawn_bundle((Name("Leaf"),  Position { x: 4.0, y: 0.0 }));

    world.add_relation(node_a, ChildOf, scene_root);
    world.add_relation(node_b, ChildOf, scene_root);
    world.add_relation(node_c, ChildOf, node_a);
    world.add_relation(leaf,   ChildOf, node_c);

    println!("Archetypes after relations: {}", world.archetype_count());

    println!("\nDirect children of SceneRoot:");
    for child in world.children_of(ChildOf, scene_root) {
        if let Some(name) = world.get::<Name>(child) {
            println!("  {child}: {}", name.0);
        }
    }

    println!("\nhas_relation checks:");
    println!("  node_a ChildOf scene_root: {}", world.has_relation(node_a, ChildOf, scene_root));
    println!("  leaf   ChildOf node_c:     {}", world.has_relation(leaf,   ChildOf, node_c));
    println!("  node_b ChildOf node_a:     {}", world.has_relation(node_b, ChildOf, node_a));

    // get_relation_target
    if let Some(target) = world.get_relation_target(node_c, ChildOf) {
        if let Some(name) = world.get::<Name>(target) {
            println!("\nnode_c's parent is: {} ({})", target, name.0);
        }
    }

    // Despawn recursive
    println!("\nBefore despawn_recursive(scene_root): {} entities", world.entity_count());
    world.despawn_recursive(ChildOf, scene_root);
    println!("After  despawn_recursive(scene_root): {} entities", world.entity_count());

    // ── Commands ──────────────────────────────────────────────
    println!("\n=== Commands ===");

    // Спавним ещё врагов для теста commands
    for i in 0..5 {
        world.spawn_bundle((
            Position { x: i as f32 * 2.0, y: 0.0 },
            Health   { current: if i % 2 == 0 { 0.0 } else { 100.0 }, max: 100.0 },
            Name("Enemy"),
        ));
    }
    println!("Spawned 5 enemies, entities: {}", world.entity_count());

    let mut cmds = Commands::new();

    // Commands::insert — добавляем Velocity тем у кого её нет
    Query::<(Read<Health>, Read<Position>)>::new(&world)
        .for_each(|entity, (hp, _)| {
            if hp.current > 0.0 && world.get::<Velocity>(entity).is_none() {
                cmds.insert(entity, Velocity { x: 0.5, y: 0.0 });
            }
        });

    // Commands::despawn — убираем мёртвых
    let mut despawn_count = 0usize;
    Query::<Read<Health>>::new(&world).for_each(|entity, hp| {
        if hp.current <= 0.0 {
            cmds.despawn(entity);
            despawn_count += 1;
        }
    });
    println!("Queued: {} despawns in Commands", despawn_count);
    println!("Commands queue len: {}", cmds.len());

    cmds.apply(&mut world);
    println!("After apply: {} entities remaining", world.entity_count());

    // ── Component remove / despawn ─────────────────────────────
    println!("\n=== Component Remove / Despawn ===");
    println!("Player alive: {}", world.is_alive(player));

    world.remove::<Player>(player);
    println!(
        "After remove<Player>: has Player = {}",
        world.get::<Player>(player).is_some()
    );

    world.despawn(player);
    println!("After despawn:  alive = {}", world.is_alive(player));
    println!("Final entity count: {}", world.entity_count());

    // ── Resource remove ───────────────────────────────────────
    println!("\n=== Resource Remove ===");
    let removed = world.remove_resource::<GameConfig>();
    println!("Removed GameConfig: {}", removed.is_some());
    println!(
        "has GameConfig after remove: {}",
        world.has_resource::<GameConfig>()
    );

    println!("\n=== Done ===");
}