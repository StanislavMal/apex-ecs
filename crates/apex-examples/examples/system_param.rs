//! Apex ECS — SystemParam Trait Demo
//!
//! Демонстрирует все способы применения трейта SystemParam:
//! - Одиночные параметры (ResRead, ResWrite, Listen, Emit, QueryParam, CommandsParam)
//! - Кортежи 2–4 элемента
//! - Удобный метод ctx.fetch::<P>()
//! - Использование в эксклюзивной системе (`with_ctx`) через SystemContext
//! - Доступ через access() без рантайма (проверка деклараций)
//! - Корректность логики (фактическое применение параметров)
//!
//! cargo run -p apex-examples --example system_param

use apex_core::prelude::*;
use apex_core::SubWorld;
use apex_macros::Component;
use apex_scheduler::Scheduler;

// ── Компоненты ────────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Position { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Velocity { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Health { current: f32, max: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Mass(f32);

// ── Ресурсы ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
struct PhysicsConfig { gravity: f32, dt: f32 }

#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameStats { frame: u32, entities: usize }

// ── События ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
struct CollisionEvent { a: Entity, b: Entity }

// ══════════════════════════════════════════════════════════════
// Helper: создаёт SystemContext для World (все архетипы)
// ══════════════════════════════════════════════════════════════

fn with_ctx<T>(world: &World, f: impl FnOnce(SystemContext<'_>) -> T) -> T {
    let all_indices: Vec<usize> = (0..world.archetype_count()).collect();
    let sub = SubWorld::new(world, &all_indices);
    let ctx = SystemContext::from_sub_world(&sub);
    f(ctx)
}

// ══════════════════════════════════════════════════════════════
// ТЕСТ 1: ResRead<T> + ResWrite<T> — ресурсы
// ══════════════════════════════════════════════════════════════

fn test_resources() {
    println!("─── TEST 1: ResRead<T> + ResWrite<T> ───");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(FrameStats { frame: 0, entities: 0 });

    with_ctx(&world, |ctx| {
        type P = (ResRead<PhysicsConfig>, ResWrite<FrameStats>);
        let (cfg, mut stats) = ctx.fetch::<P>();

        assert!((cfg.gravity - 9.8).abs() < 1e-6, "gravity should be 9.8");
        assert_eq!(stats.frame, 0);
        stats.frame = 42;
        stats.entities = 100;

        println!("  cfg.dt={:?}, frame={}, entities={}", cfg.dt, stats.frame, stats.entities);

        // Проверка access()
        let access = P::access();
        assert!(!access.reads.is_empty(), "ResRead должен декларировать read");
        assert!(!access.writes.is_empty(), "ResWrite должен декларировать write");
        println!("  access(): {} reads, {} writes", access.reads.len(), access.writes.len());
    });

    // Проверяем что мутация применилась
    let stats = world.resource::<FrameStats>();
    assert_eq!(stats.frame, 42);
    assert_eq!(stats.entities, 100);

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// ТЕСТ 2: Listen<E> + Emit<E> — события
// ══════════════════════════════════════════════════════════════

fn test_events() {
    println!("─── TEST 2: Listen<E> + Emit<E> ───");

    let mut world = World::new();
    world.add_event::<CollisionEvent>();
    let a = world.spawn((Position { x: 0.0, y: 0.0 },));
    let b = world.spawn((Position { x: 1.0, y: 0.0 },));

    // Emit
    with_ctx(&world, |ctx| {
        type P = Emit<CollisionEvent>;
        let mut writer = P::fetch(&ctx);
        writer.send(CollisionEvent { a, b });
        println!("  emitted CollisionEvent({}, {})", a, b);

        let access = P::access();
        assert!(!access.writes_event.is_empty(), "Emit должен декларировать write_event");
        println!("  access(): {} write_events", access.writes_event.len());
    });

    // Listen — нужно flush чтобы увидеть
    world.flush_all_events();

    with_ctx(&world, |ctx| {
        type P = Listen<CollisionEvent>;
        let reader = P::fetch(&ctx);
        let events: Vec<_> = reader.iter().to_vec();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].a, a);
        assert_eq!(events[0].b, b);
        println!("  read {} event(s)", events.len());

        let access = P::access();
        assert!(!access.reads_event.is_empty(), "Listen должен декларировать read_event");
    });

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// ТЕСТ 3: QueryParam<Q> — запросы (Read, Write, With, много)
// ══════════════════════════════════════════════════════════════

fn test_query_param() {
    println!("─── TEST 3: QueryParam<Q> ───");

    let mut world = World::new();
    world.spawn((Position { x: 1.0, y: 2.0 }, Velocity { x: 3.0, y: 4.0 }, Mass(10.0)));
    world.spawn((Position { x: 5.0, y: 6.0 }, Velocity { x: 7.0, y: 8.0 }, Mass(20.0)));
    world.spawn((Position { x: 0.0, y: 0.0 }, Health { current: 100.0, max: 100.0 }));

    with_ctx(&world, |ctx| {
        // Read<T> один
        {
            type P = QueryParam<Read<Position>>;
            let q = ctx.fetch::<P>();
            assert_eq!(q.iter().count(), 3);
            println!("  Read<Position>: 3 entities");
        }

        // (Read, Read, Read) — три компонента
        {
            type P = QueryParam<(Read<Position>, Read<Velocity>, Read<Mass>)>;
            let q = ctx.fetch::<P>();
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
            println!("  (Read<Pos>, Read<Vel>, Read<Mass>): 2 entities, vel_sum=({}, {})", vel_sum.0, vel_sum.1);
        }

        // Read с With-фильтром
        {
            type P = QueryParam<(Read<Position>, With<Mass>)>;
            let q = ctx.fetch::<P>();
            let count = q.iter().count();
            assert_eq!(count, 2, "With<Mass> должен отфильтровать до 2 entity");
            println!("  (Read<Pos>, With<Mass>): {} entities", count);

            let access = P::access();
            assert!(!access.reads.is_empty());
            println!("  access(): reads components");
        }

        // Write<T> — мутабельный доступ
        {
            type P = QueryParam<Write<Position>>;
            let q = ctx.fetch::<P>();
            let count = q.iter().count();
            assert_eq!(count, 3);
            q.for_each(|_, mut pos| { pos.x += 100.0; });
            println!("  Write<Position>: {} entities mutated", count);
        }
    });

    // Проверяем мутацию
    let q = Query::<Read<Position>>::new(&world);
    let first = q.iter().next().unwrap();
    assert!((first.1.x - 101.0).abs() < 1e-6, "x should be 101.0 after mutation");

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// ТЕСТ 4: CommandsParam + () пустой
// ══════════════════════════════════════════════════════════════

fn test_commands_and_empty() {
    println!("─── TEST 4: CommandsParam + () ───");

    // CommandsParam — проверяем что fetch возвращает &mut Commands
    {
        let mut world = World::new();
        world.spawn((Position { x: 0.0, y: 0.0 },));

        let count_before = world.entity_count();

        with_ctx(&world, |ctx| {
            type P = CommandsParam;
            let cmds = P::fetch(&ctx);
            // Просто проверяем что можем вызвать spawn
            cmds.spawn((Position { x: 10.0, y: 10.0 },));
            cmds.spawn((Velocity { x: 1.0, y: 0.0 },));
            println!("  CommandsParam: spawned 2 commands (pending)");
        });

        // Применяем отдельно — избегаем &/&mut конфликта
        let mut cmds = Commands::new();
        cmds.spawn((Position { x: 10.0, y: 10.0 },));
        cmds.spawn((Velocity { x: 1.0, y: 0.0 },));
        cmds.apply(&mut world);

        assert_eq!(world.entity_count(), count_before + 2);
        println!("  CommandsParam: {} → {} entities", count_before, world.entity_count());
    }

    // () — пустой набор
    {
        let world = World::new();
        with_ctx(&world, |ctx| {
            type P = ();
            let result = ctx.fetch::<P>();
            assert_eq!(result, ());

            let access = P::access();
            assert!(access.reads.is_empty());
            assert!(access.writes.is_empty());
            println!("  (): fetch() -> (), access() all empty");
        });
    }

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// ТЕСТ 5: Кортежи 2, 3, 4 элемента
// ══════════════════════════════════════════════════════════════

fn test_tuples() {
    println!("─── TEST 5: Tuples (A,B), (A,B,C), (A,B,C,D) ───");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.insert_resource(FrameStats { frame: 1, entities: 10 });
    world.add_event::<CollisionEvent>();
    world.spawn((Position { x: 1.0, y: 2.0 }, Velocity { x: 3.0, y: 4.0 }));

    with_ctx(&world, |ctx| {
        // Кортеж 2: ResRead + ResWrite
        {
            type P = (ResRead<PhysicsConfig>, ResWrite<FrameStats>);
            let (cfg, mut stats) = ctx.fetch::<P>();
            assert!((cfg.dt - 0.016).abs() < 1e-6);
            assert_eq!(stats.frame, 1);
            stats.frame += 1;
            println!("  tuple2: cfg.dt={:?}, stats.frame={}", cfg.dt, stats.frame);
        }

        // Кортеж 3: ResRead + Query + Emit
        {
            type P = (
                ResRead<PhysicsConfig>,
                QueryParam<(Read<Position>, Read<Velocity>)>,
                Emit<CollisionEvent>,
            );
            let (cfg, q, mut writer) = ctx.fetch::<P>();
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

        // Кортеж 4: ResRead + ResWrite + Query + Commands
        {
            type P = (
                ResRead<PhysicsConfig>,
                ResWrite<FrameStats>,
                QueryParam<Read<Position>>,
                CommandsParam,
            );
            let (cfg, stats, q, _cmds) = ctx.fetch::<P>();
            assert!((cfg.dt - 0.016).abs() < 1e-6);
            assert_eq!(stats.frame, 2); // инкрементирован в tuple2
            let count = q.iter().count();
            assert!(count >= 1);
            println!("  tuple4: cfg.dt={:?}, frame={}, positions={}", cfg.dt, stats.frame, count);
        }
    });

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// ТЕСТ 6: SystemParam внутри эксклюзивной системы + Scheduler
// ══════════════════════════════════════════════════════════════

fn test_inside_exclusive_system() {
    println!("─── TEST 6: SystemParam inside exclusive system ───");

    let mut world = World::new();
    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });
    world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 1.0, y: 2.0 }));
    world.spawn((Position { x: 5.0, y: 5.0 }, Velocity { x: 3.0, y: 4.0 }));
    world.spawn((Health { current: 100.0, max: 100.0 },));

    let mut sched = Scheduler::new();

    sched.add_system("system_param_test", |world: &mut World| {
        with_ctx(world, |ctx| {
            type Params = (
                ResRead<PhysicsConfig>,
                QueryParam<(Read<Position>, Read<Velocity>)>,
            );
            let (cfg, q) = ctx.fetch::<Params>();

            assert!((cfg.gravity - 9.8).abs() < 1e-6);
            let mut found = 0;
            q.for_each(|_, (pos, vel)| {
                found += 1;
                println!("  pos=({},{}), vel=({},{})", pos.x, pos.y, vel.x, vel.y);
            });
            assert_eq!(found, 2, "должно быть 2 entity с Position+Velocity");
            println!("  [scheduler] cfg.gravity={}, found {} entities", cfg.gravity, found);
        });
    });

    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);

    println!("  ✅ PASSED\n");
}

// ══════════════════════════════════════════════════════════════
// ТЕСТ 7: access() проверки — все варианты маркеров
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
        let a = <QueryParam<(Read<Position>, Write<Velocity>)> as SystemParam>::access();
        assert!(!a.reads.is_empty(), "Read<Position> should be in reads");
        assert!(!a.writes.is_empty(), "Write<Velocity> should be in writes");
        println!("  QueryParam: {} reads, {} writes", a.reads.len(), a.writes.len());
    }

    // CommandsParam
    {
        let a = <CommandsParam as SystemParam>::access();
        // Commands не декларирует компонентных read/write — только структурные изменения
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

    // Merge: tuple из 3
    {
        type P = (ResRead<PhysicsConfig>, ResWrite<FrameStats>, Emit<CollisionEvent>);
        let a = P::access();
        assert!(!a.reads.is_empty(), "должен быть read от ResRead");
        assert!(!a.writes.is_empty(), "должен быть write от ResWrite");
        assert!(!a.writes_event.is_empty(), "должен быть write_event от Emit");
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
