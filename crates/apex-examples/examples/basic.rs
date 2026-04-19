use apex_core::prelude::*;
use apex_scheduler::{Scheduler, ParSystem, SystemContext, AccessDescriptor};

// ── Компоненты ────────────────────────────────────────────────
#[derive(Debug, Clone, Copy)] struct Position  { x: f32, y: f32 }
#[derive(Debug, Clone, Copy)] struct Velocity  { x: f32, y: f32 }
#[derive(Debug, Clone, Copy)] struct Health    { current: f32, max: f32 }
#[derive(Debug, Clone, Copy)] struct Mass(f32);
#[derive(Debug, Clone, Copy)] struct Player;
#[derive(Debug, Clone)]       struct Name(pub &'static str);

// ── Ресурсы ───────────────────────────────────────────────────
#[derive(Debug, Clone, Copy)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Debug, Default)]
struct FrameStats { frame: u32, entities_processed: usize }

// ── События ───────────────────────────────────────────────────
#[derive(Debug, Clone, Copy)]
struct DamageEvent { target: Entity, amount: f32 }

#[derive(Debug, Clone, Copy)]
struct DeathEvent  { entity: Entity }

// ── ParSystem реализации ──────────────────────────────────────

/// Система физики: применяет гравитацию и интегрирует позиции.
/// Читает PhysicsConfig из ресурсов.
/// Читает Mass, пишет Velocity и Position.
struct PhysicsSystem;

impl ParSystem for PhysicsSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new()
            .read::<Mass>()
            .write::<Velocity>()
            .write::<Position>()
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        let cfg = ctx.resource::<PhysicsConfig>();
        let dt      = cfg.dt;
        let gravity = cfg.gravity;

        ctx.query::<(Read<Mass>, Write<Velocity>, Write<Position>)>()
           .for_each_component(|(mass, vel, pos)| {
               // Упрощённая физика: гравитация пропорциональна массе
               vel.y -= gravity * mass.0 * dt;
               pos.x += vel.x * dt;
               pos.y += vel.y * dt;
           });

        println!("  [PhysicsSystem] processed {} entities",
            ctx.query::<Read<Position>>().len()
        );
    }
}

/// Система здоровья: клампит HP в [0, max].
/// Не конфликтует с PhysicsSystem → параллельны.
struct HealthClampSystem;

impl ParSystem for HealthClampSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().write::<Health>()
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        let mut clamped = 0usize;
        ctx.query::<Write<Health>>().for_each_component(|hp| {
            let before = hp.current;
            hp.current = hp.current.clamp(0.0, hp.max);
            if (hp.current - before).abs() > 1e-6 { clamped += 1; }
        });
        println!("  [HealthClampSystem] clamped {} entities", clamped);
    }
}

/// Система обновления статистики.
/// Читает всё но не пишет в компоненты — только в ресурс FrameStats.
/// Sequential потому что пишет в несколько ресурсов сразу.
fn stats_system(world: &mut World) {
    let count = world.entity_count();
    let stats = world.resource_mut::<FrameStats>();
    stats.frame += 1;
    stats.entities_processed += count;
    println!("  [stats_system] frame={} total_processed={}",
        stats.frame, stats.entities_processed
    );
}

/// Sequential система: применяет события урона + отправляет DeathEvent.
/// Требует structural access (send_event) → Sequential.
fn damage_apply_system(world: &mut World) {
    let damage_events: Vec<DamageEvent> = world
        .events::<DamageEvent>()
        .iter_previous()
        .copied()
        .collect();

    let mut deaths = Vec::new();
    for ev in &damage_events {
        if let Some(hp) = world.get_mut::<Health>(ev.target) {
            hp.current -= ev.amount;
            if hp.current <= 0.0 {
                deaths.push(ev.target);
            }
        }
    }

    println!("  [damage_apply] processed {} damage events, {} deaths",
        damage_events.len(), deaths.len()
    );

    for entity in deaths {
        world.send_event(DeathEvent { entity });
    }
}

/// Sequential система: despawn мёртвых entity через Commands.
fn despawn_dead_system(world: &mut World) {
    let death_events: Vec<DeathEvent> = world
        .events::<DeathEvent>()
        .iter_current()
        .copied()
        .collect();

    if death_events.is_empty() { return; }

    let mut cmds = Commands::with_capacity(death_events.len());
    for ev in &death_events {
        cmds.despawn(ev.entity);
    }
    let count = cmds.len();
    cmds.apply(world);
    println!("  [despawn_dead] despawned {} entities", count);
}

fn main() {
    println!("=== Apex ECS — Basic Example ===\n");

    let mut world = World::new();

    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Player>();
    world.register_component::<Name>();

    // ── Resources ─────────────────────────────────────────────
    println!("=== Resources ===");

    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(FrameStats::default());

    println!("PhysicsConfig: gravity={}, dt={}",
        world.resource::<PhysicsConfig>().gravity,
        world.resource::<PhysicsConfig>().dt,
    );
    world.resource_mut::<PhysicsConfig>().gravity = 1.62; // лунная гравитация
    println!("Moon gravity: {}", world.resource::<PhysicsConfig>().gravity);
    world.resource_mut::<PhysicsConfig>().gravity = 9.8;  // возвращаем

    // ── Events ────────────────────────────────────────────────
    println!("\n=== Events ===");

    world.add_event::<DamageEvent>();
    world.add_event::<DeathEvent>();

    // ── Spawn ─────────────────────────────────────────────────
    println!("\n=== Spawn ===");

    let player = world.spawn_bundle((
        Position { x: 0.0,  y: 10.0 },
        Velocity { x: 1.0,  y: 0.0  },
        Health   { current: 100.0, max: 100.0 },
        Mass(80.0),
        Player,
        Name("Hero"),
    ));
    println!("Player:  {player}");

    let enemy1 = world.spawn_bundle((
        Position { x: 20.0, y: 5.0  },
        Velocity { x: -0.5, y: 0.0  },
        Health   { current: 30.0, max: 30.0 },
        Mass(60.0),
        Name("Goblin"),
    ));
    let _enemy2 = world.spawn_bundle((
        Position { x: -5.0, y: 3.0  },
        Velocity { x:  0.3, y: 0.0  },
        Health   { current: 75.0, max: 75.0 },
        Mass(120.0),
        Name("Orc"),
    ));

    println!("Entities: {}", world.entity_count());
    println!("Archetypes: {}", world.archetype_count());

    // ── Batch Spawn ───────────────────────────────────────────
    println!("\n=== Batch Spawn ===");

    let batch_entities = world.spawn_many(1000, |i| (
        Position { x: i as f32 * 0.1, y: 0.0 },
        Velocity { x: 0.0, y: 0.0 },
        Health   { current: 50.0, max: 50.0 },
        Mass(10.0),
    ));
    println!("spawn_many(1000): {} entities created", batch_entities.len());
    println!("Total entities: {}", world.entity_count());

    // spawn_many_silent — без Vec<Entity>
    world.spawn_many_silent(500, |i| (
        Position { x: -(i as f32), y: 5.0 },
        Velocity { x: 1.0, y: 0.0 },
        Health   { current: 25.0, max: 25.0 },
        Mass(5.0),
    ));
    println!("spawn_many_silent(500) done");
    println!("Total entities: {}", world.entity_count());

    // ── Events — отправка урона ───────────────────────────────
    println!("\n=== Events: Damage Pipeline ===");

    // Тик 1: отправляем события урона
    world.send_event(DamageEvent { target: enemy1, amount: 35.0 }); // убьёт goblin (30 HP)
    world.send_event(DamageEvent { target: player, amount: 10.0 });

    println!("Sent {} damage events (current tick)",
        world.events::<DamageEvent>().len_current()
    );

    world.tick(); // damage events → previous
    println!("After tick: {} in previous", world.events::<DamageEvent>().len_previous());

    // ── Hybrid Scheduler ──────────────────────────────────────
    println!("\n=== Hybrid Scheduler ===\n");

    let mut sched = Scheduler::new();

    // ── Parallel Stage 1: Physics + HealthClamp ───────────────
    // Не конфликтуют:
    //   Physics:     reads Mass, writes Vel, Pos
    //   HealthClamp: writes Health
    // → один Stage, параллельно
    sched.add_par_system("physics",      PhysicsSystem);
    sched.add_par_system("health_clamp", HealthClampSystem);

    // ── Sequential барьер: damage_apply ───────────────────────
    // Читает Events, пишет Health (get_mut), send_event(DeathEvent)
    // Требует sequential — structural access
    let damage_id = sched.add_system("damage_apply", damage_apply_system).id();

    // ── Sequential: despawn_dead ──────────────────────────────
    // Читает DeathEvent (current!), despawn через Commands
    let despawn_id = sched.add_system("despawn_dead", despawn_dead_system).id();

    // ── Sequential: stats ─────────────────────────────────────
    let stats_id = sched.add_system("stats", stats_system).id();

    // Зависимости: damage → despawn → stats
    sched.add_dependency(despawn_id, damage_id);
    sched.add_dependency(stats_id,   despawn_id);

    sched.compile().unwrap();

    println!("Compiled plan:\n{}", sched.debug_plan());
    println!("Running scheduler (tick 1):\n");
    sched.run(&mut world);

    println!("\nAfter tick 1:");
    println!("  Entities: {}", world.entity_count());
    println!("  FrameStats: frame={}", world.resource::<FrameStats>().frame);

    if let Some(hp) = world.get::<Health>(player) {
        println!("  Player HP: {}/{}", hp.current, hp.max);
    }

    // enemy1 должен быть despawned (30 HP - 35 dmg = dead)
    println!("  enemy1 alive: {}", world.is_alive(enemy1));

    // ── Ещё один тик ──────────────────────────────────────────
    println!("\n=== Tick 2 ===\n");
    world.tick();
    sched.run(&mut world);

    println!("\nAfter tick 2:");
    println!("  Entities: {}", world.entity_count());
    println!("  FrameStats: frame={}", world.resource::<FrameStats>().frame);

    // ── Relations ─────────────────────────────────────────────
    println!("\n=== Relations ===");

    let root  = world.spawn_bundle((Name("Root"),));
    let child = world.spawn_bundle((Name("Child"), Position { x: 1.0, y: 0.0 }));
    let leaf  = world.spawn_bundle((Name("Leaf"),  Position { x: 2.0, y: 0.0 }));

    world.add_relation(child, ChildOf, root);
    world.add_relation(leaf,  ChildOf, child);

    println!("has_relation(child, ChildOf, root): {}",
        world.has_relation(child, ChildOf, root)
    );
    println!("get_relation_target(leaf, ChildOf): {:?}",
        world.get_relation_target(leaf, ChildOf)
    );

    let children: Vec<Entity> = world.children_of(ChildOf, root).collect();
    println!("children_of(root): {} direct children", children.len());

    let before = world.entity_count();
    world.despawn_recursive(ChildOf, root);
    println!("despawn_recursive: {} → {} entities", before, world.entity_count());

    // ── Commands ──────────────────────────────────────────────
    println!("\n=== Commands ===");

    let mut cmds = Commands::new();
    let alive_count = world.entity_count();

    Query::<Read<Health>>::new(&world).for_each(|e, hp| {
        if hp.current < 30.0 {
            cmds.despawn(e);
        }
    });

    println!("Queued {} despawns (HP < 30)", cmds.len());
    cmds.apply(&mut world);
    println!("Entities: {} → {}", alive_count, world.entity_count());

    // ── Финал ─────────────────────────────────────────────────
    println!("\n=== Done ===");
    println!("Final entity count: {}", world.entity_count());
    println!("Final tick: {:?}", world.current_tick());
    println!("Resources: {}", world.resource_count());
}