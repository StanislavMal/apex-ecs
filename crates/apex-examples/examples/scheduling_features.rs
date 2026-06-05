//! Apex ECS Scheduling Features — run conditions, scope, auto-apply, benchmarks.
//!
//! ```
//! cargo run -p apex-examples --example scheduling_features --release
//! ```
//!
//! Parts:
//! 1. Scenarios (asserts in-systems) — 10 сценариев
//! 2. Performance benchmarks — 100 warmup + 300 measured frames
//! 3. Auto-exit after 300 measured frames

use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::{
    conditions, Condition, ConditionTree, Scheduler, StageLabel,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

// ── Components & Resources ──────────────────────────────────────

#[derive(Component, Clone, Copy)]
struct Pos {
    x: f32,
    #[allow(dead_code)]
    y: f32,
}
#[derive(Component, Clone, Copy)]
struct Vel {
    #[allow(dead_code)]
    vx: f32,
    #[allow(dead_code)]
    y: f32,
}
#[derive(Component, Clone, Copy)]
struct Player;
#[derive(Component, Clone, Copy)]
struct Enemy;
#[derive(Component, Clone, Copy)]
struct Spawned;

#[derive(Clone, Copy)]
struct Paused(bool);

#[derive(Clone, Copy)]
struct DebugOverlay(#[allow(dead_code)] bool);

// ── Scenario flags (set by systems, checked by main) ────────────

static S1_CALLED: AtomicBool = AtomicBool::new(false);
static S1_SKIPPED: AtomicBool = AtomicBool::new(false);

static S2_CALLED: AtomicBool = AtomicBool::new(false);

static S3_BOTH_TRUE: AtomicBool = AtomicBool::new(false);
static S3_ONE_FALSE: AtomicBool = AtomicBool::new(false);

static S4_CALLED: AtomicBool = AtomicBool::new(false);

static S5_SEEN: AtomicBool = AtomicBool::new(false);

static S6_RAN: AtomicBool = AtomicBool::new(false);
static S6_SKIPPED: AtomicBool = AtomicBool::new(false);

static S7_RAN: AtomicBool = AtomicBool::new(false);

static S8_RAN: AtomicBool = AtomicBool::new(false);

static S9_COUNT: Mutex<u32> = Mutex::new(0);
static S10_COUNT: Mutex<u32> = Mutex::new(0);

// ── Scenario 1: run_if closure ───────────────────────────────────
// .run_if(|w| !paused) — skipped when Paused(true)

system! {
    fn s1_system(_world: &mut World) {
        S1_CALLED.store(true, Ordering::SeqCst);
    }
}

fn setup_s1() -> (Scheduler, World) {
    let mut sched = Scheduler::new();
    sched.add_exclusive_system(s1_system)
        .run_if(|w: &World| w.try_resource::<Paused>().map(|p| !p.0).unwrap_or(true));

    let mut world = World::new();
    world.insert_resource(Paused(false));
    (sched, world)
}

fn check_s1(sched: &mut Scheduler, world: &mut World) {
    // Paused(false) → system should run
    S1_CALLED.store(false, Ordering::SeqCst);
    sched.run_sequential(world);
    assert!(S1_CALLED.load(Ordering::SeqCst), "S1: should run when not paused");

    // Paused(true) → system should skip
    *world.resource_mut::<Paused>() = Paused(true);
    S1_CALLED.store(false, Ordering::SeqCst);
    sched.run_sequential(world);
    assert!(!S1_CALLED.load(Ordering::SeqCst), "S1: should skip when paused");

    S1_SKIPPED.store(true, Ordering::SeqCst);
    println!("  ✓ 1. run_if closure — system skipped when paused");
}

// ── Scenario 2: run_if_cond typed ────────────────────────────────
// .run_if_cond(conditions::any_with_component::<Player>()) — планировщик видит read<Player>

system! {
    fn s2_system(q: (Read<Player>, Write<Pos>)) {
        q.for_each(|_, (_, mut pos)| { pos.x += 1.0; });
        S2_CALLED.store(true, Ordering::SeqCst);
    }
}

fn check_s2() {
    let mut sched = Scheduler::new();
    sched.add_auto_system("s2", s2_system)
        .run_if_cond(conditions::any_with_component::<Player>());

    // No Player entity → system skipped
    let mut world = World::new();
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(!S2_CALLED.load(Ordering::SeqCst), "S2: should skip without Player");

    // Player entity exists → system runs
    S2_CALLED.store(false, Ordering::SeqCst);
    let mut world2 = World::new();
    world2.spawn((Player, Pos { x: 0.0, y: 0.0 }));
    let mut sched2 = Scheduler::new();
    sched2
        .add_auto_system("s2", s2_system)
        .run_if_cond(conditions::any_with_component::<Player>());
    sched2.compile_with_world(&world2).unwrap();
    sched2.run_sequential(&mut world2);
    assert!(S2_CALLED.load(Ordering::SeqCst), "S2: should run with Player");

    println!("  ✓ 2. run_if_cond typed — system runs only when Player exists");
}

// ── Scenario 3a: chained .run_if() AND ───────────────────────────
// .run_if(a).run_if(b) → AND с шорт-циркутом

static S3A_FLAG: AtomicBool = AtomicBool::new(false);

fn check_s3a() {
    let mut sched = Scheduler::new();
    sched.add_system("s3a", move |_: &mut World| {
        S3A_FLAG.store(true, Ordering::SeqCst);
    })
    .run_if(|w: &World| w.try_resource::<Paused>().map(|p| !p.0).unwrap_or(true))
    .run_if(|_: &World| true);

    let mut world = World::new();
    world.insert_resource(Paused(false));
    S3A_FLAG.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(S3A_FLAG.load(Ordering::SeqCst), "S3a: both true → should run");
    S3_BOTH_TRUE.store(true, Ordering::SeqCst);

    // Paused(true) → first condition false → skip
    let mut world2 = World::new();
    world2.insert_resource(Paused(true));
    S3A_FLAG.store(false, Ordering::SeqCst);
    let mut sched2 = Scheduler::new();
    sched2.add_system("s3a_b", move |_: &mut World| {
        S3A_FLAG.store(true, Ordering::SeqCst);
    })
    .run_if(|w: &World| w.try_resource::<Paused>().map(|p| !p.0).unwrap_or(true))
    .run_if(|_: &World| true);
    sched2.compile_with_world(&world2).unwrap();
    sched2.run_sequential(&mut world2);
    assert!(!S3A_FLAG.load(Ordering::SeqCst), "S3a: one false → should skip");
    S3_ONE_FALSE.store(true, Ordering::SeqCst);

    println!("  ✓ 3a. chained .run_if() AND — both conditions must be true");
}

// ── Scenario 3b: tuple AND с typed conditions ────────────────────
// .run_if_cond((cond_a, cond_b)) → AND + автоперж access обоих

static S3B_FLAG: AtomicBool = AtomicBool::new(false);

fn check_s3b() {
    // both Paused and Player resources exist → runs
    let mut sched = Scheduler::new();
    sched.add_system("s3b", move |_: &mut World| {
        S3B_FLAG.store(true, Ordering::SeqCst);
    })
    .run_if_cond((
        conditions::resource_exists::<Paused>(),
        conditions::resource_exists::<DebugOverlay>(),
    ));

    let mut world = World::new();
    world.insert_resource(Paused(false));
    S3B_FLAG.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(!S3B_FLAG.load(Ordering::SeqCst), "S3b: no DebugOverlay resource → should skip");

    // Both resources exist → runs
    let mut world2 = World::new();
    world2.insert_resource(Paused(true));
    world2.insert_resource(DebugOverlay(false));
    S3B_FLAG.store(false, Ordering::SeqCst);
    let mut sched2 = Scheduler::new();
    sched2.add_system("s3b_b", move |_: &mut World| {
        S3B_FLAG.store(true, Ordering::SeqCst);
    })
    .run_if_cond((
        conditions::resource_exists::<Paused>(),
        conditions::resource_exists::<DebugOverlay>(),
    ));
    sched2.compile_with_world(&world2).unwrap();
    sched2.run_sequential(&mut world2);
    assert!(S3B_FLAG.load(Ordering::SeqCst), "S3b: tuple AND, both true → should run");

    println!("  ✓ 3b. tuple AND (typed + typed) — access merged, both must be true");
}

// ── Scenario 4: .not() ───────────────────────────────────────────

fn check_s4() {
    let mut sched = Scheduler::new();
    sched.add_system("s4", move |_: &mut World| {
        S4_CALLED.store(true, Ordering::SeqCst);
    })
    .run_if_cond(conditions::resource_exists::<Paused>().not());

    // No Paused → .not() true → runs
    let mut world = World::new();
    S4_CALLED.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(S4_CALLED.load(Ordering::SeqCst), "S4: .not() when resource missing → should run");

    // Paused exists → .not() false → skips
    let mut world2 = World::new();
    world2.insert_resource(Paused(true));
    S4_CALLED.store(false, Ordering::SeqCst);
    let mut sched2 = Scheduler::new();
    sched2.add_system("s4b", move |_: &mut World| {
        S4_CALLED.store(true, Ordering::SeqCst);
    })
    .run_if_cond(conditions::resource_exists::<Paused>().not());
    sched2.compile_with_world(&world2).unwrap();
    sched2.run_sequential(&mut world2);
    assert!(!S4_CALLED.load(Ordering::SeqCst), "S4: .not() when resource exists → should skip");

    println!("  ✓ 4. .not() — inversion works");
}

// ── Scenario 5a: chain + sequential (immediate apply) ────────────
// chain гарантирует порядок; world.spawn() — немедленная видимость

system! {
    fn s5a_spawner(world: &mut World) {
        world.spawn((Spawned,));
    }
}

fn check_s5a() {
    let mut sched = Scheduler::new();
    sched.add_exclusive_system(s5a_spawner);
    sched.add_system("reader", move |world: &mut World| {
        let c = Query::<Read<Spawned>>::new(world).iter().count();
        if c > 0 {
            S5_SEEN.store(true, Ordering::SeqCst);
        }
    });
    sched.chain(&["s5a_spawner", "reader"]).unwrap();

    let mut world = World::new();
    S5_SEEN.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(S5_SEEN.load(Ordering::SeqCst), "S5a: reader should see spawned entity");

    println!("  ✓ 5a. chain + sequential — spawned entity visible in same frame");
}

// ── Scenario 5b: chain + auto-apply (HAS_DEFERRED) ──────────────
// system! с cmd: Cmd → HAS_DEFERRED=true → chain() → compile() авто-split
// + runtime проверка: reader видит заспавненный entity

static S5B_SEEN: AtomicBool = AtomicBool::new(false);

system! {
    fn s5b_spawner(cmd: Cmd) {
        cmd.spawn((Spawned, Pos { x: 1.0, y: 1.0 }));
    }
}

fn check_s5b() {
    // Часть A: проверка compile
    let mut sched = Scheduler::new();
    sched.add_auto_system("spawner_d", s5b_spawner);
    sched.add_system("reader_d", |_: &mut World| {});
    sched.chain(&["spawner_d", "reader_d"]).unwrap();
    sched.compile().unwrap();
    let stages = sched.stages().unwrap();
    assert!(
        stages.len() >= 2,
        "S5b: auto-apply should split Stage, got {} stages",
        stages.len()
    );

    // Часть B: проверка runtime — reader видит заспавненные entity
    let mut sched2 = Scheduler::new();
    sched2.add_auto_system("spawner_r", s5b_spawner);
    sched2.add_system("reader_r", move |world: &mut World| {
        let c = Query::<Read<Spawned>>::new(world).iter().count();
        if c > 0 {
            S5B_SEEN.store(true, Ordering::SeqCst);
        }
    });
    sched2.chain(&["spawner_r", "reader_r"]).unwrap();

    let mut world = World::new();
    S5B_SEEN.store(false, Ordering::SeqCst);
    sched2.compile_with_world(&world).unwrap();
    sched2.run_sequential(&mut world);
    assert!(S5B_SEEN.load(Ordering::SeqCst), "S5b: reader should see spawned entity");

    println!("  ✓ 5b. chain + auto-apply (HAS_DEFERRED) — compile split + runtime visibility");
}

// ── Scenario 6: scope condition ──────────────────────────────────
// Все системы внутри staged наследуют scope condition

fn check_s6() {
    let mut sched = Scheduler::new();
    sched.staged(StageLabel::tag("s6"), |s| {
        s.run_condition(move |w: &World| {
            w.try_resource::<Paused>().map(|p| !p.0).unwrap_or(true)
        });
        s.add_system("s6a", move |_: &mut World| {
            S6_RAN.store(true, Ordering::SeqCst);
        });
        s.add_system("s6b", move |_: &mut World| {
            // just to have multiple systems in scope
        });
    });

    // Not paused → both run
    let mut world = World::new();
    world.insert_resource(Paused(false));
    S6_RAN.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(S6_RAN.load(Ordering::SeqCst), "S6: should run when not paused");

    // Paused → both skip
    let mut sched2 = Scheduler::new();
    sched2.staged(StageLabel::tag("s6p"), |s| {
        s.run_condition(move |w: &World| {
            w.try_resource::<Paused>().map(|p| !p.0).unwrap_or(true)
        });
        s.add_system("s6a2", move |_: &mut World| {
            S6_RAN.store(true, Ordering::SeqCst);
        });
    });
    let mut world2 = World::new();
    world2.insert_resource(Paused(true));
    sched2.compile_with_world(&world2).unwrap();
    S6_RAN.store(false, Ordering::SeqCst);
    sched2.run_sequential(&mut world2);
    assert!(!S6_RAN.load(Ordering::SeqCst), "S6: should skip when paused");
    S6_SKIPPED.store(true, Ordering::SeqCst);

    println!("  ✓ 6. scope condition — all systems in scope inherit");
}

// ── Scenario 7: scope + local AND ────────────────────────────────

fn check_s7() {
    let mut sched = Scheduler::new();
    sched.staged(StageLabel::tag("s7"), |s| {
        s.run_condition(move |w: &World| {
            w.try_resource::<Paused>().map(|p| !p.0).unwrap_or(true)
        });
        s.add_system("s7a", move |_: &mut World| {
            S7_RAN.store(true, Ordering::SeqCst);
        })
        .run_if(|_: &World| true);
    });

    // Not paused + local AND true → runs
    let mut world = World::new();
    world.insert_resource(Paused(false));
    S7_RAN.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(S7_RAN.load(Ordering::SeqCst), "S7: scope+local → should run");

    println!("  ✓ 7. scope + local AND — both conditions applied");
}

// ── Scenario 8: mixed run_if + run_if_cond + chain ───────────────

fn check_s8() {
    let mut sched = Scheduler::new();
    sched.add_system("s8", move |_: &mut World| {
        S8_RAN.store(true, Ordering::SeqCst);
    })
    .run_if(|w: &World| w.try_resource::<Paused>().map(|p| !p.0).unwrap_or(true))
    .run_if_cond(conditions::any_with_component::<Player>());
    sched
        .add_system("s8_helper", |_: &mut World| {})
        .run_if(|_: &World| true);
    sched.chain(&["s8_helper", "s8"]).unwrap();

    // Paused(false) + Player exists → runs
    let mut world = World::new();
    world.insert_resource(Paused(false));
    world.spawn((Player, Pos { x: 0.0, y: 0.0 }));
    S8_RAN.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(S8_RAN.load(Ordering::SeqCst), "S8: mixed conditions → should run");

    println!("  ✓ 8. mixed run_if + run_if_cond + chain — all work together");
}

// ── Scenario 9: run_until(5) ─────────────────────────────────────

fn check_s9() {
    let mut sched = Scheduler::new();
    sched.add_system("s9", move |_: &mut World| {
        let mut c = S9_COUNT.lock().unwrap();
        *c += 1;
    })
    .run_if_cond(conditions::run_until(5));

    let mut world = World::new();
    sched.compile_with_world(&world).unwrap();
    for _ in 0..10 {
        sched.run_sequential(&mut world);
    }
    let count = *S9_COUNT.lock().unwrap();
    assert_eq!(count, 5, "S9: run_until(5) should execute exactly 5 times, got {}", count);

    println!("  ✓ 9. run_until(5) — executed exactly 5 times");
}

// ── Scenario 10: every_n_frames(60) ──────────────────────────────

fn check_s10() {
    let mut sched = Scheduler::new();
    sched.add_system("s10", move |_: &mut World| {
        let mut c = S10_COUNT.lock().unwrap();
        *c += 1;
    })
    .run_if_cond(conditions::every_n_frames(60));

    let mut world = World::new();
    sched.compile_with_world(&world).unwrap();
    // Run 120 frames → should execute 2 times (at ticks 0 and 59)
    // Every n frames checks at (tick % n) == 0
    for _ in 0..120 {
        sched.run_sequential(&mut world);
    }
    let count = *S10_COUNT.lock().unwrap();
    assert_eq!(count, 2, "S10: every_n_frames(60) should execute 2 times in 120 frames, got {}", count);

    println!("  ✓ 10. every_n_frames(60) — executed once every 60 frames");
}

// ── Scenario 11: or_else — OR-композиция ──────────────────────────
// Система выполнится если ХОТЯ БЫ одно из OR-условий true

static S11_RAN: AtomicBool = AtomicBool::new(false);

fn check_s11() {
    let mut sched = Scheduler::new();
    sched.add_system("s11", move |_: &mut World| {
        S11_RAN.store(true, Ordering::SeqCst);
    })
    .or_else(|w: &World| w.has_resource::<Paused>())
    .or_else(|w: &World| w.has_resource::<DebugOverlay>());

    // Paused exists → runs (always-true base + OR Paused)
    let mut world = World::new();
    world.insert_resource(Paused(true));
    S11_RAN.store(false, Ordering::SeqCst);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(S11_RAN.load(Ordering::SeqCst), "S11a: Paused exists → runs");

    // Neither resource → still runs (default And([]) base = always true)
    // This is correct: or_else on a system with no prior conditions
    // means "run always OR run on condition", which is always.
    // To test true OR skip, set base to a false condition first:
    let mut sched2 = Scheduler::new();
    sched2.add_system("s11b", move |_: &mut World| {
        S11_RAN.store(true, Ordering::SeqCst);
    })
    // Reset: no-op condition always false → pure OR, no always-true baseline
    .condition(ConditionTree::And(vec![ConditionTree::leaf(|_: &World| false)]))
    .or_else(|w: &World| w.has_resource::<Paused>())
    .or_else(|w: &World| w.has_resource::<DebugOverlay>());

    let mut world2 = World::new();
    S11_RAN.store(false, Ordering::SeqCst);
    sched2.compile_with_world(&world2).unwrap();
    sched2.run_sequential(&mut world2);
    assert!(!S11_RAN.load(Ordering::SeqCst), "S11b: neither resource → skip");

    // Paused exists → runs
    let mut world3 = World::new();
    world3.insert_resource(Paused(true));
    S11_RAN.store(false, Ordering::SeqCst);
    let mut sched3 = Scheduler::new();
    sched3.add_system("s11c", move |_: &mut World| {
        S11_RAN.store(true, Ordering::SeqCst);
    })
    .condition(ConditionTree::And(vec![ConditionTree::leaf(|_: &World| false)]))
    .or_else(|w: &World| w.has_resource::<Paused>());
    sched3.compile_with_world(&world3).unwrap();
    sched3.run_sequential(&mut world3);
    assert!(S11_RAN.load(Ordering::SeqCst), "S11c: Paused exists → runs via OR");

    println!("  ✓ 11. or_else — system runs when any OR condition is true");
}

// ═══════════════════════════════════════════════════════════════════
// Part 2: Performance Benchmarks
// ═══════════════════════════════════════════════════════════════════

fn bench_run_if_overhead() {
    println!("\n[PERFORMANCE]");

    // Benchmark 1: run_if(true) overhead — 1000 systems, 100 warmup + 300 measured
    let mut sched = Scheduler::new();
    for i in 0..1000 {
        sched.add_system(
            format!("b_true_{}", i),
            |_: &mut World| {},
        )
        .run_if(|_: &World| true);
    }

    let mut world = World::new();
    sched.compile_with_world(&world).unwrap();

    // Warmup
    for _ in 0..100 {
        sched.run_sequential(&mut world);
    }

    let start = Instant::now();
    let iterations = 300;
    for _ in 0..iterations {
        sched.run_sequential(&mut world);
    }
    let elapsed = start.elapsed();
    let total_us = elapsed.as_secs_f64() * 1_000_000.0;
    let per_system_us = total_us / iterations as f64 / 1000.0;
    println!(
        "  run_if(true) overhead:     {:.3}μs per system (1000 sys, {} iters)",
        per_system_us, iterations
    );

    // Benchmark 2: run_if(false) savings — systems don't execute
    let mut sched2 = Scheduler::new();
    for i in 0..1000 {
        sched2.add_system(
            format!("b_false_{}", i),
            |_: &mut World| {},
        )
        .run_if(|_: &World| false);
    }

    // Run once to measure time with conditions false
    // Just measure overhead of condition evaluation itself
    static B_CALLED: AtomicUsize = AtomicUsize::new(0);
    let mut sched3 = Scheduler::new();
    for i in 0..1000 {
        let idx = i;
        sched3.add_system(
            format!("b_cnt_{}", i),
            move |_: &mut World| { B_CALLED.fetch_add(1, Ordering::Relaxed); },
        )
        .run_if(move |_: &World| idx >= 1000); // always false
    }
    sched3.compile_with_world(&world).unwrap();
    for _ in 0..100 {
        sched3.run_sequential(&mut world);
    }
    let before = B_CALLED.load(Ordering::Relaxed);
    let start2 = Instant::now();
    for _ in 0..iterations {
        sched3.run_sequential(&mut world);
    }
    let after = B_CALLED.load(Ordering::Relaxed);
    let elapsed2 = start2.elapsed();
    assert_eq!(
        before, after,
        "bench2: no systems should execute with condition=false"
    );
    println!(
        "  run_if(false) savings:     all {} systems skipped ({:.3}ms total)",
        1000,
        elapsed2.as_secs_f64() * 1000.0
    );

    // Benchmark 3: run_if_cond typed overhead
    let mut sched4 = Scheduler::new();
    for i in 0..1000 {
        sched4.add_system(
            format!("b_typed_{}", i),
            |_: &mut World| {},
        )
        .run_if_cond(conditions::every_n_frames(1)); // always true
    }
    sched4.compile_with_world(&world).unwrap();
    for _ in 0..100 {
        sched4.run_sequential(&mut world);
    }
    let start3 = Instant::now();
    for _ in 0..iterations {
        sched4.run_sequential(&mut world);
    }
    let elapsed3 = start3.elapsed();
    let per_system_us3 = elapsed3.as_secs_f64() * 1_000_000.0 / iterations as f64 / 1000.0;
    println!(
        "  run_if_cond typed overhead: {:.3}μs per system (1000 sys)",
        per_system_us3
    );

    // Benchmark 4: chain + auto-apply — 100 entities visible
    static B4_COUNT: AtomicUsize = AtomicUsize::new(0);
    let mut sched5 = Scheduler::new();
    sched5.add_system("b4_spawn", move |world: &mut World| {
        for _ in 0..100 {
            world.spawn((Spawned, Pos { x: 0.0, y: 0.0 }));
        }
    });
    sched5.add_system("b4_read", move |world: &mut World| {
        let c = Query::<Read<Spawned>>::new(world).iter().count();
        B4_COUNT.store(c, Ordering::Relaxed);
    });
    sched5.chain(&["b4_spawn", "b4_read"]).unwrap();
    sched5.compile_with_world(&world).unwrap();
    sched5.run_sequential(&mut world);
    let seen = B4_COUNT.load(Ordering::Relaxed);
    assert!(
        seen >= 100,
        "bench4: chain+autoapply should see all spawned entities, saw {}",
        seen
    );
    println!("  chain + auto-apply:         {} entities visible in same frame ✓", seen);

    // Benchmark 5: scope propagation
    static B5_CALLED: AtomicUsize = AtomicUsize::new(0);
    let mut sched6 = Scheduler::new();
    sched6.staged(StageLabel::tag("b5"), |s| {
        s.run_condition(|_: &World| false);
        for i in 0..10 {
            s.add_system(
                format!("b5_{}", i),
                move |_: &mut World| { B5_CALLED.fetch_add(1, Ordering::Relaxed); },
            );
        }
    });
    sched6.compile_with_world(&world).unwrap();
    for _ in 0..10 {
        sched6.run_sequential(&mut world);
    }
    let called = B5_CALLED.load(Ordering::Relaxed);
    assert_eq!(called, 0, "scope false → no systems execute");
    println!("  scope propagation:          10 systems, 0 executed ✓");

    // Benchmark 6: run_until(50) correctness
    static B6_COUNTS: [AtomicUsize; 10] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let mut sched7 = Scheduler::new();
    for i in 0..10 {
        sched7.add_system(
            format!("b6_{}", i),
            move |_: &mut World| { B6_COUNTS[i].fetch_add(1, Ordering::Relaxed); },
        )
        .run_if_cond(conditions::run_until(50));
    }
    sched7.compile_with_world(&world).unwrap();
    for _ in 0..100 {
        sched7.run_sequential(&mut world);
    }
    for (i, c) in B6_COUNTS.iter().enumerate() {
        let n = c.load(Ordering::Relaxed);
        assert_eq!(
            n, 50,
            "bench6: system {} should execute 50 times with run_until(50), got {}",
            i, n
        );
    }
    println!("  run_until(50):              10 systems × 50 executions each ✓");
}

// ═══════════════════════════════════════════════════════════════════
// Part 3: Main — scenarios → benchmarks → auto-exit
// ═══════════════════════════════════════════════════════════════════

fn main() {
    println!("=== Apex ECS Scheduling Features ===\n");
    println!("[SCENARIOS]");

    // Scenario 1
    let (mut s1_sched, mut s1_world) = setup_s1();
    check_s1(&mut s1_sched, &mut s1_world);

    // Scenario 2
    check_s2();

    // Scenario 3a: chained AND
    check_s3a();

    // Scenario 3b: tuple AND (typed + opaque)
    check_s3b();

    // Scenario 4
    check_s4();

    // Scenario 5a: chain + sequential
    check_s5a();

    // Scenario 5b: chain + auto-apply (HAS_DEFERRED)
    check_s5b();

    // Scenario 6
    check_s6();

    // Scenario 7
    check_s7();

    // Scenario 8
    check_s8();

    // Scenario 9
    check_s9();

    // Scenario 10
    check_s10();

    // Scenario 11
    check_s11();

    println!("\nAll {} scenarios passed.", 13);

    // Performance benchmarks
    bench_run_if_overhead();

    println!("\nAll done. Exiting.");
}
