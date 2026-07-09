//! Apex ECS — Basic Example
//!
//! Demonstrates all the key capabilities of the engine:
//! - Resources, Events
//! - spawn_many (batch API)
//! - Hybrid Scheduler (ParSystem + FnParSystem + Sequential)
//! - Bevy-like Stages (Startup, PreUpdate, Update, PostUpdate)
//! - Relations (ChildOf hierarchies)
//! - Commands
//!
//! Run: `cargo run -p apex-examples --example basic --release`

use apex_core::prelude::*;

use apex_macros::Component;
use apex_scheduler::{sys, Scheduler, StageLabel};

// ── Components ────────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)] struct Position  { x: f32, y: f32 }
#[derive(Component, Clone, Copy, Debug)] struct Velocity  { x: f32, y: f32 }
#[derive(Component, Clone, Copy, Debug)] struct Health    { current: f32, max: f32 }
#[derive(Component, Clone, Copy, Debug)] struct Mass(f32);
#[derive(Component, Clone, Copy, Debug)] struct Player;
#[derive(Component, Clone, Copy, Debug)] struct Enemy;
#[derive(Component, Clone, Copy, Debug)] struct Name(pub &'static str);

// ── Resources ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct DeltaTime(f32);

#[derive(Debug, Default, Clone, Copy)]
struct FrameStats { frame: u32, total_entities_processed: usize }

// ── Events ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct DamageEvent { target: Entity, amount: f32 }

#[derive(Clone, Copy, Debug)]
struct DeathEvent { entity: Entity }

// ── AutoSystem: Physics ────────────────────────────────────────

system! {
    fn physics_system(
        q: (&Mass, &mut Velocity, &mut Position),
        cfg: Res<PhysicsConfig>,
        _dt: Res<DeltaTime>,
    ) {
        let dt = cfg.dt;
        let g = cfg.gravity;
        let count = q.len();
        q.for_each_mut(|_, (mass, mut vel, mut pos)| {
            vel.y -= g * mass.0 * dt;
            pos.x += vel.x * dt;
            pos.y += vel.y * dt;
        });
        println!("  [PhysicsSystem] {} entities", count);
    }
}

// ── AutoSystem: HealthClamp ────────────────────────────────────

system! {
    fn health_clamp_system(
        q: (&mut Health,),
        cmd: Cmd,
    ) {
        let mut dead_entities: Vec<Entity> = Vec::new();
        q.for_each_mut(|entity, (mut hp,)| {
            hp.current = hp.current.clamp(0.0, hp.max);
            if hp.current <= 0.0 {
                dead_entities.push(entity);
            }
        });
        for e in &dead_entities {
            cmd.despawn(*e);
        }
        println!("  [HealthClampSystem] dead={}", dead_entities.len());
    }
}

// ── AutoSystem: Movement ──────────────────────────────────────

system! {
    fn movement_system(
        q: (&Velocity, &mut Position),
    ) {
        let count = q.len();
        q.for_each_mut(|_, (vel, mut pos)| {
            pos.x += vel.x * 0.016;
            pos.y += vel.y * 0.016;
        });
        println!("  [MovementSystem (AutoSystem)] updated {} entities", count);
    }
}

// ── Sequential systems ────────────────────────────────────────

system! {
    fn damage_apply(
        world: &mut World,
    ) {
        let events: Vec<DamageEvent> = {
            let mut reader = world.event_reader::<DamageEvent>();
            reader.read().into_iter().copied().collect()
        };

        let mut deaths = Vec::new();
        for ev in &events {
            if let Some(mut hp) = world.get_mut::<Health>(ev.target) {
                hp.current -= ev.amount;
                if hp.current <= 0.0 { deaths.push(ev.target); }
            }
        }

        println!("  [damage_apply] events={} deaths={}", events.len(), deaths.len());
        for entity in deaths { world.send_event(DeathEvent { entity }); }
    }
}

system! {
    fn despawn_dead(
        world: &mut World,
    ) {
        let deaths: Vec<DeathEvent> = {
            let mut reader = world.event_reader::<DeathEvent>();
            reader.read().into_iter().copied().collect()
        };

        if deaths.is_empty() { return; }

        let mut cmds = Commands::with_capacity(deaths.len());
        for ev in &deaths { cmds.despawn(ev.entity); }
        let count = cmds.len();
        cmds.apply(world);
        println!("  [despawn_dead] despawned={}", count);
    }
}

system! {
    fn stats_update(
        world: &mut World,
    ) {
        let entities = world.entity_count();
        let stats = world.resource_mut::<FrameStats>();
        stats.frame += 1;
        stats.total_entities_processed += entities;
        println!("  [stats_update] frame={} entities={}", stats.frame, stats.total_entities_processed);
    }
}

// ── Startup systems ──────────────────────────────────────────
// Executed once on the first run().

system! {
    fn init_resources(
        world: &mut World,
    ) {
        world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
        world.insert_resource(DeltaTime(0.016));
        world.insert_resource(FrameStats::default());

        world.add_event::<DamageEvent>();
        world.add_event::<DeathEvent>();

        println!("  [Startup] init_resources: PhysicsConfig, DeltaTime, FrameStats, events");
    }
}

system! {
    fn spawn_player(
        world: &mut World,
    ) {
        world.spawn((
            Position { x: 0.0,  y: 10.0 },
            Velocity { x: 1.0,  y: 0.0  },
            Health   { current: 100.0, max: 100.0 },
            Mass(80.0),
            Player,
            Name("Hero"),
        ));

        world.spawn((
            Position { x: 20.0, y:  5.0 },
            Velocity { x: -0.5, y:  0.0 },
            Health   { current: 30.0, max: 30.0 },
            Mass(60.0),
            Name("Goblin"),
        ));

        world.spawn((
            Position { x: -5.0, y: 3.0 },
            Velocity { x:  0.3, y: 0.0 },
            Health   { current: 75.0, max: 75.0 },
            Mass(120.0),
            Name("Orc"),
        ));

        // Batch spawn
        world.spawn_many_silent(500, |i| (
            Position { x: i as f32 * 0.5, y: 0.0 },
            Velocity { x: 0.1, y: 0.0 },
            Health   { current: 50.0, max: 50.0 },
            Mass(70.0),
            Enemy,
        ));

        world.spawn_many_silent(200, |i| (
            Position { x: -(i as f32), y: 2.0 },
            Velocity { x: 0.0, y: 0.0 },
            Health   { current: 20.0, max: 20.0 },
            Mass(30.0),
            Enemy,
        ));

        println!("  [Startup] spawn_player: entities={}, archetypes={}",
            world.entity_count(), world.archetype_count());
    }
}

// ── AutoSystem: Enemy AI ─────────────────────────────────────

system! {
    fn enemy_ai_system(
        q: (&Enemy, &mut Velocity),
    ) {
        let count = q.len();
        q.for_each_mut(|_, (_, mut vel)| {
            vel.x *= 0.99;
            vel.y *= 0.99;
        });
        if count > 0 {
            println!("  [enemy_ai] updated {} enemies", count);
        }
    }
}

// ── main ──────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Basic Example (with Stages) ===\n");

    let mut world = World::new();

    // ── Scheduler with Stages ────────────────────────────────
    println!("=== Scheduler with Stages ===\n");

    let mut sched = Scheduler::new();

    // ╔══════════════════════════════════════════════════════════╗
    // ║  Startup — executed once on the first run()            ║
    // ╚══════════════════════════════════════════════════════════╝
    // Bare identifiers of exclusive system! — single add_systems entry point.
    sched.add_systems(StageLabel::Startup, (init_resources, spawn_player));

    // ╔══════════════════════════════════════════════════════════╗
    // ║  PreUpdate — AutoSystem (access auto-inference)        ║
    // ╚══════════════════════════════════════════════════════════╝
    sched.add_systems(StageLabel::PreUpdate, sys("movement", movement_system));
    println!("  [Stage:PreUpdate] MovementSystem registered as AutoSystem");

    // ╔══════════════════════════════════════════════════════════╗
    // ║  Update — AutoSystem (parallel)                        ║
    // ╚══════════════════════════════════════════════════════════╝
    sched.add_systems(StageLabel::Update, (
        sys("physics", physics_system),
        sys("health_clamp", health_clamp_system),
        sys("enemy_ai", enemy_ai_system),
    ));

    // ╔══════════════════════════════════════════════════════════╗
    // ║  PostUpdate — exclusive systems + explicit order       ║
    // ╚══════════════════════════════════════════════════════════╝
    sched.add_systems(StageLabel::PostUpdate, (damage_apply, despawn_dead, stats_update));
    sched.chain(&["damage_apply", "despawn_dead", "stats_update"]).unwrap();

    sched.compile_with_world(&world).unwrap();

    println!("\nCompiled plan:\n{}", sched.debug_plan());
    println!("\nVerbose diagnostics:\n{}", sched.debug_plan_verbose());

    // ── Tick 1 (Startup + all stages) ────────────────────────
    println!("\n--- Running tick 1 (Startup + PreUpdate + Update + PostUpdate) ---\n");
    sched.run(&mut world);

    // Find player and goblin via query (created in startup)
    let player = Query::<(Entity, &Player)>::new(&world)
        .iter().next().map(|(e, _)| e)
        .expect("Player entity not found");
    let goblin = Query::<(Entity, &Name)>::new(&world)
        .iter().find(|(_, n)| n.0 == "Goblin")
        .map(|(e, _)| e)
        .expect("Goblin entity not found");

    println!("\nAfter tick 1:");
    println!("  Entities:       {}", world.entity_count());
    println!("  Resources:      PhysicsConfig={:?}", world.resource::<PhysicsConfig>());
    if let Some(hp) = world.get::<Health>(player) {
        println!("  player HP:      {}/{}", hp.current, hp.max);
    }
    println!("  FrameStats:     {:?}", world.resource::<FrameStats>());

    // ── Tick 2 (without Startup) ─────────────────────────────
    // Send events via world.send_event BEFORE scheduler.run,
    // so they land in the previous buffer after tick()
    world.send_event(DamageEvent { target: goblin, amount: 35.0 });
    world.send_event(DamageEvent { target: player, amount: 10.0 });

    println!("\n--- Running tick 2 (without Startup — only PreUpdate → Update → PostUpdate run) ---\n");
    world.tick();
    sched.run(&mut world);

    println!("\nAfter tick 2:");
    println!("  Entities:   {}", world.entity_count());
    println!("  FrameStats: {:?}", world.resource::<FrameStats>());

    // ── Relations ─────────────────────────────────────────────
    println!("\n=== Relations ===");

    let root   = world.spawn((Name("Root"),));
    let child1 = world.spawn((Name("Child1"), Position { x: 1.0, y: 0.0 }));
    let child2 = world.spawn((Name("Child2"), Position { x: 2.0, y: 0.0 }));
    let leaf   = world.spawn((Name("Leaf"),   Position { x: 3.0, y: 0.0 }));

    world.add_relation(child1, ChildOf, root);
    world.add_relation(child2, ChildOf, root);
    world.add_relation(leaf,   ChildOf, child1);

    println!("has_relation(child1, ChildOf, root):  {}", world.has_relation(child1, ChildOf, root));
    println!("has_relation(leaf,   ChildOf, child1): {}", world.has_relation(leaf, ChildOf, child1));
    println!("has_relation(child2, ChildOf, child1): {}", world.has_relation(child2, ChildOf, child1));

    if let Some(target) = world.target_of(leaf, ChildOf) {
        if let Some(name) = world.get::<Name>(target) {
            println!("leaf's parent: {} ({})", target, name.0);
        }
    }

    let children: Vec<Entity> = world.targets_of(ChildOf, root).collect();
    println!("Direct children of root: {}", children.len());

    let before = world.entity_count();
    world.despawn_recursive(ChildOf, root);
    println!("despawn_recursive(root): {} → {} entities", before, world.entity_count());

    // ── Batch Relations ───────────────────────────────────────
    println!("\n=== Batch Relations (add_relation_batch) ===");

    // Create 100 child entities + 1 parent
    let parent = world.spawn((Name("BatchParent"),));
    let mut children: Vec<Entity> = Vec::with_capacity(100);
    for i in 0..100 {
        children.push(world.spawn((Name("BatchChild"), Position { x: i as f32, y: 0.0 })));
    }

    // add_relation_batch — one operation instead of 100 individual add_relation calls
    let _before = world.entity_count();
    world.add_relation_batch(&children, ChildOf, parent);
    println!(
        "add_relation_batch: {} children → parent={} (entities: {})",
        children.len(),
        parent,
        world.entity_count(),
    );

    // Verify that relations are set
    let mut found = 0;
    for &child in &children {
        if world.has_relation(child, ChildOf, parent) {
            found += 1;
        }
    }
    println!("Verified: {}/{} children have ChildOf relation to parent", found, children.len());

    // Check targets_of (subjects of the ChildOf relation to parent)
    let direct: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
    println!("targets_of(ChildOf, parent) count: {} (expected 100)", direct.len());

    // Clean up
    world.despawn_recursive(ChildOf, parent);
    println!("despawn_recursive(parent): {} → {} entities", children.len() + 1, world.entity_count());

    // ── Commands ──────────────────────────────────────────────
    println!("\n=== Commands ===");

    let before = world.entity_count();
    let mut cmds = Commands::new();

    Query::<&Health>::new(&world).for_each(|e, hp| {
        if hp.current < 25.0 { cmds.despawn(e); }
    });

    println!("Queued {} despawns (HP < 25)", cmds.len());
    cmds.apply(&mut world);
    println!("Entities: {} → {}", before, world.entity_count());

    // ── Resource ops ──────────────────────────────────────────
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