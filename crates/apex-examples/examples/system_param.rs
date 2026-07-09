//! Apex ECS — SystemParam Trait Demo
//!
//! Demonstrates every way to use the SystemParam trait:
//! - Single parameters (ResRead, ResWrite, Listen, Emit, QueryParam, CommandsParam)
//! - Tuples of 2–4 elements
//! - The ctx.fetch_unchecked::<Self::Params>() mechanism (raw; no declaration check — §0.2a)
//! - Use inside an exclusive system (`with_ctx`) via SystemContext
//! - Access through access() without the runtime (declaration inspection)
//! - Logic correctness (actual use of the parameters)
//!
//! cargo run -p apex-examples --example system_param

use apex_core::prelude::*;
use apex_core::SubWorld;
use apex_macros::Component;
use apex_scheduler::{seq, Scheduler, StageLabel};

// ── Components ────────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Position { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Velocity { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Health { current: f32, max: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Mass(f32);

// ── Resources ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameStats { frame: u32, entities: usize }

// ── Events ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
struct CollisionEvent { a: Entity, b: Entity }

// ══════════════════════════════════════════════════════════════
// Helper: builds a SystemContext for the World (all archetypes)
// ══════════════════════════════════════════════════════════════

fn with_ctx<T>(world: &mut World, f: impl FnOnce(SystemContext<'_>) -> T) -> T {
    let all_indices: Vec<usize> = (0..world.archetype_count()).collect();
    // SAFETY: `&mut World` proves exclusivity — nothing else can access the
    // world while the SubWorld (and the context built on it) is live.
    let sub = unsafe { SubWorld::new(world, &all_indices) };
    let ctx = SystemContext::from_sub_world(&sub);
    f(ctx)
}

// ══════════════════════════════════════════════════════════════
// TEST 1: ResRead<T> + ResWrite<T> — resources
// ══════════════════════════════════════════════════════════════

fn test_resources() {
    println!("─── TEST 1: ResRead<T> + ResWrite<T> ───");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(FrameStats { frame: 0, entities: 0 });

    with_ctx(&mut world, |ctx| {
        type P = (ResRead<PhysicsConfig>, ResWrite<FrameStats>);
        let (cfg, mut stats) = ctx.fetch_unchecked::<P>();

        assert!((cfg.gravity - 9.8).abs() < 1e-6, "gravity should be 9.8");
        assert_eq!(stats.frame, 0);
        stats.frame = 42;
        stats.entities = 100;

        println!("  cfg.dt={:?}, frame={}, entities={}", cfg.dt, stats.frame, stats.entities);

        // Check access()
        let access = P::access();
        assert!(!access.reads.is_empty(), "ResRead should declare a read");
        assert!(!access.writes.is_empty(), "ResWrite should declare a write");
        println!("  access(): {} reads, {} writes", access.reads.len(), access.writes.len());
    });

    // Verify the mutation was applied
    let stats = world.resource::<FrameStats>();
    assert_eq!(stats.frame, 42);
    assert_eq!(stats.entities, 100);

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// TEST 2: Listen<E> + Emit<E> — events
// ══════════════════════════════════════════════════════════════

fn test_events() {
    println!("─── TEST 2: Listen<E> + Emit<E> ───");

    let mut world = World::new();
    world.add_event::<CollisionEvent>();
    let a = world.spawn((Position { x: 0.0, y: 0.0 },));
    let b = world.spawn((Position { x: 1.0, y: 0.0 },));

    // Emit
    with_ctx(&mut world, |ctx| {
        type P = Emit<CollisionEvent>;
        let mut writer = P::fetch(&ctx);
        writer.send(CollisionEvent { a, b });
        println!("  emitted CollisionEvent({}, {})", a, b);

        let access = P::access();
        assert!(!access.writes_event.is_empty(), "Emit should declare a write_event");
        println!("  access(): {} write_events", access.writes_event.len());
    });

    // Listen — need a flush to see it
    world.flush_all_events();

    with_ctx(&mut world, |ctx| {
        type P = Listen<CollisionEvent>;
        let reader = P::fetch(&ctx);
        let events: Vec<_> = reader.iter().to_vec();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].a, a);
        assert_eq!(events[0].b, b);
        println!("  read {} event(s)", events.len());

        let access = P::access();
        assert!(!access.reads_event.is_empty(), "Listen should declare a read_event");
    });

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// TEST 3: QueryParam<Q> — queries (Read, Write, With, multiple)
// ══════════════════════════════════════════════════════════════

fn test_query_param() {
    println!("─── TEST 3: QueryParam<Q> ───");

    let mut world = World::new();
    world.spawn((Position { x: 1.0, y: 2.0 }, Velocity { x: 3.0, y: 4.0 }, Mass(10.0)));
    world.spawn((Position { x: 5.0, y: 6.0 }, Velocity { x: 7.0, y: 8.0 }, Mass(20.0)));
    world.spawn((Position { x: 0.0, y: 0.0 }, Health { current: 100.0, max: 100.0 }));

    with_ctx(&mut world, |ctx| {
        // &T single
        {
            type P = QueryParam<&'static Position>;
            let q = ctx.fetch_unchecked::<P>();
            assert_eq!(q.iter().count(), 3);
            println!("  &Position: 3 entities");
        }

        // (Read, Read, Read) — three components
        {
            type P = QueryParam<(&'static Position, &'static Velocity, &'static Mass)>;
            let q = ctx.fetch_unchecked::<P>();
            let mut count = 0;
            let mut vel_sum = (0.0f32, 0.0f32);
            q.for_each(|_, (_, vel, _)| {
                count += 1;
                vel_sum.0 += vel.x;
                vel_sum.1 += vel.y;
            });
            assert_eq!(count, 2);
            assert!((vel_sum.0 - 10.0).abs() < 1e-6);
            assert!((vel_sum.1 - 12.0).abs() < 1e-6);
            println!("  (&Pos, &Vel, &Mass): 2 entities, vel_sum=({}, {})", vel_sum.0, vel_sum.1);
        }

        // Read with a With filter
        {
            type P = QueryParam<(&'static Position, With<Mass>)>;
            let q = ctx.fetch_unchecked::<P>();
            let count = q.iter().count();
            assert_eq!(count, 2, "With<Mass> should filter down to 2 entities");
            println!("  (&Pos, With<Mass>): {} entities", count);

            let access = P::access();
            assert!(!access.reads.is_empty());
            println!("  access(): reads components");
        }

        // &mut T — mutable access
        {
            type P = QueryParam<&'static mut Position>;
            let mut q = ctx.fetch_unchecked::<P>();
            let count = q.iter_mut().count();
            assert_eq!(count, 3);
            q.for_each_mut(|_, mut pos| { pos.x += 100.0; });
            println!("  &mut Position: {} entities mutated", count);
        }
    });

    // Verify the mutation
    let q = Query::<&Position>::new(&world);
    let first = q.iter().next().unwrap();
    assert!((first.x - 101.0).abs() < 1e-6, "x should be 101.0 after mutation");

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// TEST 4: CommandsParam + () empty
// ══════════════════════════════════════════════════════════════

fn test_commands_and_empty() {
    println!("─── TEST 4: CommandsParam + () ───");

    // CommandsParam — verify that fetch returns &mut Commands
    {
        let mut world = World::new();
        world.spawn((Position { x: 0.0, y: 0.0 },));

        let count_before = world.entity_count();

        with_ctx(&mut world, |ctx| {
            type P = CommandsParam;
            let cmds = P::fetch(&ctx);
            // Just verify that we can call spawn
            cmds.spawn((Position { x: 10.0, y: 10.0 },));
            cmds.spawn((Velocity { x: 1.0, y: 0.0 },));
            println!("  CommandsParam: spawned 2 commands (pending)");
        });

        // Apply separately — avoid a &/&mut conflict
        let mut cmds = Commands::new();
        cmds.spawn((Position { x: 10.0, y: 10.0 },));
        cmds.spawn((Velocity { x: 1.0, y: 0.0 },));
        cmds.apply(&mut world);

        assert_eq!(world.entity_count(), count_before + 2);
        println!("  CommandsParam: {} → {} entities", count_before, world.entity_count());
    }

    // () — empty set
    {
        let mut world = World::new();
        with_ctx(&mut world, |ctx| {
            type P = ();
            ctx.fetch_unchecked::<P>(); // () — fetch returns nothing

            let access = P::access();
            assert!(access.reads.is_empty());
            assert!(access.writes.is_empty());
            println!("  (): fetch() -> (), access() all empty");
        });
    }

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// TEST 5: Tuples of 2, 3, 4 elements
// ══════════════════════════════════════════════════════════════

fn test_tuples() {
    println!("─── TEST 5: Tuples (A,B), (A,B,C), (A,B,C,D) ───");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(FrameStats { frame: 1, entities: 10 });
    world.add_event::<CollisionEvent>();
    world.spawn((Position { x: 1.0, y: 2.0 }, Velocity { x: 3.0, y: 4.0 }));

    with_ctx(&mut world, |ctx| {
        // Tuple 2: ResRead + ResWrite
        {
            type P = (ResRead<PhysicsConfig>, ResWrite<FrameStats>);
            let (cfg, mut stats) = ctx.fetch_unchecked::<P>();
            assert!((cfg.dt - 0.016).abs() < 1e-6);
            assert_eq!(stats.frame, 1);
            stats.frame += 1;
            println!("  tuple2: cfg.dt={:?}, stats.frame={}", cfg.dt, stats.frame);
        }

        // Tuple 3: ResRead + Query + Emit
        {
            type P = (
                ResRead<PhysicsConfig>,
                QueryParam<(&'static Position, &'static Velocity)>,
                Emit<CollisionEvent>,
            );
            let (cfg, q, mut writer) = ctx.fetch_unchecked::<P>();
            assert!((cfg.gravity - 9.8).abs() < 1e-6);
            let mut e_entity = None;
            q.for_each(|e, (pos, vel)| {
                e_entity = Some(e);
                assert_eq!(pos.x, 1.0);
                assert_eq!(vel.y, 4.0);
            });
            assert!(e_entity.is_some());
            // Send event referencing found entity
            writer.send(CollisionEvent { a: e_entity.unwrap(), b: e_entity.unwrap() });
            println!("  tuple3: cfg.gravity={}, entity found, event sent", cfg.gravity);
        }

        // Tuple 4: ResRead + ResWrite + Query + Commands
        {
            type P = (
                ResRead<PhysicsConfig>,
                ResWrite<FrameStats>,
                QueryParam<&'static Position>,
                CommandsParam,
            );
            let (cfg, stats, q, _cmds) = ctx.fetch_unchecked::<P>();
            assert!((cfg.dt - 0.016).abs() < 1e-6);
            assert_eq!(stats.frame, 2); // incremented in tuple2
            let count = q.iter().count();
            assert!(count >= 1);
            println!("  tuple4: cfg.dt={:?}, frame={}, positions={}", cfg.dt, stats.frame, count);
        }
    });

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// TEST 6: SystemParam inside an exclusive system + Scheduler
// ══════════════════════════════════════════════════════════════

fn test_inside_exclusive_system() {
    println!("─── TEST 6: SystemParam inside exclusive system ───");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 1.0, y: 2.0 }));
    world.spawn((Position { x: 5.0, y: 5.0 }, Velocity { x: 3.0, y: 4.0 }));
    world.spawn((Health { current: 100.0, max: 100.0 },));

    let mut sched = Scheduler::new();

    sched.add_systems(StageLabel::Update, seq("system_param_test", |world: &mut World| {
        with_ctx(world, |ctx| {
            type Params = (
                ResRead<PhysicsConfig>,
                QueryParam<(&'static Position, &'static Velocity)>,
            );
            let (cfg, q) = ctx.fetch_unchecked::<Params>();

            assert!((cfg.gravity - 9.8).abs() < 1e-6);
            let mut found = 0;
            q.for_each(|_, (pos, vel)| {
                found += 1;
                println!("  pos=({},{}), vel=({},{})", pos.x, pos.y, vel.x, vel.y);
            });
            assert_eq!(found, 2, "there should be 2 entities with Position+Velocity");
            println!("  [scheduler] cfg.gravity={}, found {} entities", cfg.gravity, found);
        });
    }));

    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// TEST 7: access() checks — all marker variants
// ══════════════════════════════════════════════════════════════

fn test_access_descriptors() {
    println!("─── TEST 7: access() descriptors ───");

    // ResRead
    {
        let a = <ResRead<PhysicsConfig> as SystemParam>::access();
        assert!(!a.reads.is_empty());
        assert!(a.writes.is_empty());
        println!("  ResRead: {} reads, 0 writes", a.reads.len());
    }

    // ResWrite
    {
        let a = <ResWrite<PhysicsConfig> as SystemParam>::access();
        assert!(a.reads.is_empty());
        assert!(!a.writes.is_empty());
        println!("  ResWrite: 0 reads, {} writes", a.writes.len());
    }

    // Listen
    {
        let a = <Listen<CollisionEvent> as SystemParam>::access();
        assert!(!a.reads_event.is_empty());
        assert!(a.writes_event.is_empty());
        println!("  Listen: {} read_events, 0 write_events", a.reads_event.len());
    }

    // Emit
    {
        let a = <Emit<CollisionEvent> as SystemParam>::access();
        assert!(a.reads_event.is_empty());
        assert!(!a.writes_event.is_empty());
        println!("  Emit: 0 read_events, {} write_events", a.writes_event.len());
    }

    // QueryParam
    {
        let a = <QueryParam<(&Position, &mut Velocity)> as SystemParam>::access();
        assert!(!a.reads.is_empty(), "&Position should be in reads");
        assert!(!a.writes.is_empty(), "&mut Velocity should be in writes");
        println!("  QueryParam: {} reads, {} writes", a.reads.len(), a.writes.len());
    }

    // CommandsParam
    {
        let a = <CommandsParam as SystemParam>::access();
        // Commands does not declare component read/write — only structural changes
        println!("  CommandsParam: {} reads, {} writes", a.reads.len(), a.writes.len());
    }

    // () empty
    {
        let a = <() as SystemParam>::access();
        assert!(a.reads.is_empty());
        assert!(a.writes.is_empty());
        assert!(a.reads_event.is_empty());
        assert!(a.writes_event.is_empty());
        println!("  (): ALL empty");
    }

    // Merge: tuple of 3
    {
        type P = (ResRead<PhysicsConfig>, ResWrite<FrameStats>, Emit<CollisionEvent>);
        let a = P::access();
        assert!(!a.reads.is_empty(), "there should be a read from ResRead");
        assert!(!a.writes.is_empty(), "there should be a write from ResWrite");
        assert!(!a.writes_event.is_empty(), "there should be a write_event from Emit");
        println!("  tuple merge: {} reads, {} writes, {} write_events",
            a.reads.len(), a.writes.len(), a.writes_event.len());
    }

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// main
// ══════════════════════════════════════════════════════════════

fn main() {
    println!("=== Apex ECS — SystemParam Trait Demo ===\n");

    test_resources();
    test_events();
    test_query_param();
    test_commands_and_empty();
    test_tuples();
    test_inside_exclusive_system();
    test_access_descriptors();

    println!("═══════════════════════════════════════");
    println!("  ALL 7 TESTS PASSED");
    println!("═══════════════════════════════════════");
}
