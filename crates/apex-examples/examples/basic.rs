//! Apex ECS — Basic Example
//!
//! Демонстрирует все ключевые возможности движка:
//! - Resources, Events
//! - spawn_many (batch API)
//! - Hybrid Scheduler (ParSystem + FnParSystem + Sequential)
//! - Relations (ChildOf иерархии)
//! - Commands

use apex_core::prelude::*;
use apex_scheduler::{Scheduler, ParSystem, SystemContext};
use apex_core::access::AccessDescriptor;

// ── Компоненты ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)] struct Position  { x: f32, y: f32 }
#[derive(Clone, Copy, Debug)] struct Velocity  { x: f32, y: f32 }
#[derive(Clone, Copy, Debug)] struct Health    { current: f32, max: f32 }
#[derive(Clone, Copy, Debug)] struct Mass(f32);
#[derive(Clone, Copy, Debug)] struct Player;
#[derive(Clone, Copy, Debug)] struct Enemy;
#[derive(Clone, Copy, Debug)] struct Name(pub &'static str);

// ── Ресурсы ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Clone, Copy, Debug)]
struct DeltaTime(f32);

#[derive(Debug, Default, Clone, Copy)]
struct FrameStats { frame: u32, total_entities_processed: usize }

// ── События ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct DamageEvent { target: Entity, amount: f32 }

#[derive(Clone, Copy, Debug)]
struct DeathEvent { entity: Entity }

// ── ParSystem: Physics ─────────────────────────────────────────

struct PhysicsSystem;

impl ParSystem for PhysicsSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new()
            .read::<PhysicsConfig>()
            .read::<Mass>()
            .write::<Velocity>()
            .write::<Position>()
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        let cfg  = ctx.resource::<PhysicsConfig>();
        let dt   = cfg.dt;
        let g    = cfg.gravity;

        let count = ctx.query::<(Read<Mass>, Write<Velocity>, Write<Position>)>().len();

        ctx.for_each_component::<(Read<Mass>, Write<Velocity>, Write<Position>), _>(
            |(mass, vel, pos)| {
                vel.y  -= g * mass.0 * dt;
                pos.x  += vel.x * dt;
                pos.y  += vel.y * dt;
            }
        );

        println!("  [PhysicsSystem] {} entities", count);
    }
}

// ── ParSystem: HealthClamp ─────────────────────────────────────

struct HealthClampSystem;

impl ParSystem for HealthClampSystem {
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().write::<Health>()
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        let mut clamped = 0usize;
        ctx.for_each_component::<Write<Health>, _>(|hp| {
            let prev = hp.current;
            hp.current = hp.current.clamp(0.0, hp.max);
            if (hp.current - prev).abs() > f32::EPSILON { clamped += 1; }
        });
        println!("  [HealthClampSystem] clamped={}", clamped);
    }
}

// ── Sequential системы ────────────────────────────────────────

fn damage_apply(world: &mut World) {
    let events: Vec<DamageEvent> = world
        .events::<DamageEvent>()
        .iter_previous()
        .copied()
        .collect();

    let mut deaths = Vec::new();
    for ev in &events {
        if let Some(hp) = world.get_mut::<Health>(ev.target) {
            hp.current -= ev.amount;
            if hp.current <= 0.0 { deaths.push(ev.target); }
        }
    }

    println!("  [damage_apply] events={} deaths={}", events.len(), deaths.len());
    for entity in deaths { world.send_event(DeathEvent { entity }); }
}

fn despawn_dead(world: &mut World) {
    let deaths: Vec<DeathEvent> = world
        .events::<DeathEvent>()
        .iter_current()
        .copied()
        .collect();

    if deaths.is_empty() { return; }

    let mut cmds = Commands::with_capacity(deaths.len());
    for ev in &deaths { cmds.despawn(ev.entity); }
    let count = cmds.len();
    cmds.apply(world);
    println!("  [despawn_dead] despawned={}", count);
}

fn stats_update(world: &mut World) {
    let entities = world.entity_count();
    let stats    = world.resource_mut::<FrameStats>();
    stats.frame += 1;
    stats.total_entities_processed += entities;
    println!("  [stats_update] frame={} entities={}", stats.frame, entities);
}

// ── main ──────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Basic Example ===\n");

    let mut world = World::new();

    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Mass>();
    world.register_component::<Player>();
    world.register_component::<Enemy>();
    world.register_component::<Name>();

    // ── Resources ─────────────────────────────────────────────
    println!("=== Resources ===");

    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(DeltaTime(0.016));
    world.insert_resource(FrameStats::default());

    println!("PhysicsConfig: {:?}", world.resource::<PhysicsConfig>());
    world.resource_mut::<PhysicsConfig>().gravity = 1.62;
    println!("Moon gravity:  {:.2}", world.resource::<PhysicsConfig>().gravity);
    world.resource_mut::<PhysicsConfig>().gravity = 9.8;

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

    let goblin = world.spawn_bundle((
        Position { x: 20.0, y:  5.0 },
        Velocity { x: -0.5, y:  0.0 },
        Health   { current: 30.0, max: 30.0 },
        Mass(60.0),
        Name("Goblin"),
    ));

    let _orc = world.spawn_bundle((
        Position { x: -5.0, y: 3.0 },
        Velocity { x:  0.3, y: 0.0 },
        Health   { current: 75.0, max: 75.0 },
        Mass(120.0),
        Name("Orc"),
    ));

    println!("Spawned {} entities, {} archetypes", world.entity_count(), world.archetype_count());

    // ── Batch Spawn ───────────────────────────────────────────
    println!("\n=== Batch Spawn ===");

    let soldiers = world.spawn_many(500, |i| (
        Position { x: i as f32 * 0.5, y: 0.0 },
        Velocity { x: 0.1, y: 0.0 },
        Health   { current: 50.0, max: 50.0 },
        Mass(70.0),
        Enemy,
    ));
    println!("spawn_many(500 soldiers): {} entities created", soldiers.len());

    world.spawn_many_silent(200, |i| (
        Position { x: -(i as f32), y: 2.0 },
        Velocity { x: 0.0, y: 0.0 },
        Health   { current: 20.0, max: 20.0 },
        Mass(30.0),
        Enemy,
    ));
    println!("spawn_many_silent(200 archers)");
    println!("Total entities: {}", world.entity_count());

    // ── Events pipeline ───────────────────────────────────────
    println!("\n=== Events Pipeline ===");

    // Наносим урон: goblin получает 35 (умрёт, у него 30 HP)
    world.send_event(DamageEvent { target: goblin, amount: 35.0 });
    world.send_event(DamageEvent { target: player, amount: 10.0 });

    println!("Sent 2 damage events (current tick)");
    world.tick(); // damage → previous
    println!("After tick: {} damage events in previous",
        world.events::<DamageEvent>().len_previous()
    );

    // ── Hybrid Scheduler ──────────────────────────────────────
    println!("\n=== Hybrid Scheduler ===\n");

    let mut sched = Scheduler::new();

    // Stage 0: Physics + HealthClamp — параллельны (нет конфликтов)
    sched.add_par_system("physics",      PhysicsSystem);
    sched.add_par_system("health_clamp", HealthClampSystem);

    // Stage 1: damage_apply (Sequential барьер)
    let damage_id = sched.add_system("damage_apply", damage_apply).id();

    // Stage 2: despawn_dead (Sequential)
    let despawn_id = sched.add_system("despawn_dead", despawn_dead).id();

    // Stage 3: stats (Sequential)
    let stats_id = sched.add_system("stats_update", stats_update).id();

    sched.add_dependency(despawn_id, damage_id);
    sched.add_dependency(stats_id,   despawn_id);

    // FnParSystem: velocity reset для Enemy — демо нового API
    let reset_id = sched.add_fn_par_system(
        "enemy_ai",
        |ctx: SystemContext<'_>| {
            let count = ctx.query::<(Read<Enemy>, Write<Velocity>)>().len();
            ctx.for_each_component::<(Read<Enemy>, Write<Velocity>), _>(|(_, vel)| {
                vel.x *= 0.99; // трение
                vel.y *= 0.99;
            });
            if count > 0 {
                println!("  [enemy_ai] updated {} enemies", count);
            }
        },
        AccessDescriptor::new()
            .read::<Enemy>()
            .write::<Velocity>(),
    );

    // enemy_ai после physics (physics тоже пишет Velocity — конфликт → auto-ordered)
    sched.add_dependency(reset_id, sched.stages().map(|_| apex_scheduler::SystemId(0)).unwrap_or(apex_scheduler::SystemId(0)));
    // Просто добавляем после damage для порядка
    sched.add_dependency(reset_id, damage_id);

    sched.compile().unwrap();

    println!("Compiled plan:\n{}", sched.debug_plan());
    println!("--- Running tick 1 ---\n");
    sched.run(&mut world);

    println!("\nAfter tick 1:");
    println!("  Entities:       {}", world.entity_count());
    println!("  goblin alive:   {}", world.is_alive(goblin));
    println!("  player alive:   {}", world.is_alive(player));
    if let Some(hp) = world.get::<Health>(player) {
        println!("  player HP:      {}/{}", hp.current, hp.max);
    }
    println!("  FrameStats:     {:?}", world.resource::<FrameStats>());

    // ── Tick 2 ────────────────────────────────────────────────
    println!("\n--- Running tick 2 ---\n");
    world.tick();
    sched.run(&mut world);

    println!("\nAfter tick 2:");
    println!("  Entities:   {}", world.entity_count());
    println!("  FrameStats: {:?}", world.resource::<FrameStats>());

    // ── Relations ─────────────────────────────────────────────
    println!("\n=== Relations ===");

    let root   = world.spawn_bundle((Name("Root"),));
    let child1 = world.spawn_bundle((Name("Child1"), Position { x: 1.0, y: 0.0 }));
    let child2 = world.spawn_bundle((Name("Child2"), Position { x: 2.0, y: 0.0 }));
    let leaf   = world.spawn_bundle((Name("Leaf"),   Position { x: 3.0, y: 0.0 }));

    world.add_relation(child1, ChildOf, root);
    world.add_relation(child2, ChildOf, root);
    world.add_relation(leaf,   ChildOf, child1);

    println!("has_relation(child1, ChildOf, root):  {}", world.has_relation(child1, ChildOf, root));
    println!("has_relation(leaf,   ChildOf, child1): {}", world.has_relation(leaf, ChildOf, child1));
    println!("has_relation(child2, ChildOf, child1): {}", world.has_relation(child2, ChildOf, child1));

    if let Some(target) = world.get_relation_target(leaf, ChildOf) {
        if let Some(name) = world.get::<Name>(target) {
            println!("leaf's parent: {} ({})", target, name.0);
        }
    }

    let children: Vec<Entity> = world.children_of(ChildOf, root).collect();
    println!("Direct children of root: {}", children.len());

    let before = world.entity_count();
    world.despawn_recursive(ChildOf, root);
    println!("despawn_recursive(root): {} → {} entities", before, world.entity_count());

    // ── Commands ──────────────────────────────────────────────
    println!("\n=== Commands ===");

    let before = world.entity_count();
    let mut cmds = Commands::new();

    Query::<Read<Health>>::new(&world).for_each(|e, hp| {
        if hp.current < 25.0 { cmds.despawn(e); }
    });

    println!("Queued {} despawns (HP < 25)", cmds.len());
    cmds.apply(&mut world);
    println!("Entities: {} → {}", before, world.entity_count());

    // ── Resource cleanup ──────────────────────────────────────
    println!("\n=== Resource ops ===");
    let removed = world.remove_resource::<PhysicsConfig>();
    println!("remove_resource::<PhysicsConfig>: {}", removed.is_some());
    println!("has_resource::<PhysicsConfig>:    {}", world.has_resource::<PhysicsConfig>());

    println!("\n=== Done ===");
    println!("Final entities:   {}", world.entity_count());
    println!("Final archetypes: {}", world.archetype_count());
    println!("Final resources:  {}", world.resource_count());
    println!("Final tick:       {:?}", world.current_tick());
}