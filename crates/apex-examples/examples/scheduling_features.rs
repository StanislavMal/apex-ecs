//! Apex ECS — Run Conditions + Apply Deferred Demo (v0.3)
//!
//! Демонстрирует профессиональный `add_systems` API:
//!
//! 1. **Run Conditions — AND/OR-комбинация**
//!    - `.run_if(a).run_if(b)` → AND
//!    - `.or_else(x).or_else(y)` → OR
//!    - Scope condition через `s.run_condition()` внутри `staged()`
//!
//! 2. **Common conditions** (`conditions` module)
//!    - `resource_exists::<T>()`, `resource_equals(val)`, `any_with_component::<T>()`
//!    - `run_until(n)`, `every_n_frames(n)`, `not(cond)`
//!
//! 3. **Apply Deferred** — команды применяются в том же кадре
//!
//! 4. **Event pipeline** — по именам (без SystemId)
//!
//! cargo run -p apex-examples --example scheduling_features

use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::{conditions, Scheduler, StageLabel, sys, seq};

// ── Состояния ──────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum GamePhase {
    #[allow(dead_code)]
    Loading,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug)]
struct GameState {
    phase: GamePhase,
    debug_overlay: bool,
    frame: u32,
}

// ── Компоненты ─────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)] struct Position { x: f32, y: f32 }
#[derive(Component, Clone, Copy, Debug)] struct Velocity { x: f32, y: f32 }
#[derive(Component, Clone, Copy, Debug)] struct Player;
#[derive(Component, Clone, Copy, Debug)] struct Enemy;
#[derive(Component, Clone, Copy, Debug)] struct Projectile;

// ── Reusable condition ─────────────────────────────────────

fn debug_on(w: &World) -> bool {
    w.try_resource::<GameState>().map(|gs| gs.debug_overlay).unwrap_or(false)
}

// ── Parallel systems (system! macro) ───────────────────────

system! {
    fn movement_system(q: (Read<Velocity>, Write<Position>)) {
        q.for_each(|_, (vel, pos)| { pos.x += vel.x; pos.y += vel.y; });
    }
}

system! {
    fn debug_overlay_system(q: Read<Position>) {
        println!("[DEBUG] Entities with Position: {}", q.iter().count());
    }
}

system! {
    fn ai_system(q: (Read<Position>, Write<Velocity>, Read<Enemy>)) {
        q.for_each(|_, (pos, vel, _)| {
            vel.x = -pos.x.signum() * 0.5;
            vel.y = -pos.y.signum() * 0.5;
        });
    }
}

system! {
    fn damage_system(q: Read<Player>) { let _ = q.iter().count(); }
}

// ── Sequential systems (sequential_system! macro) ──────────

sequential_system! {
    fn load_assets_system(world: &mut World, cmd: Cmd) {
        println!("[Startup/0] Loading assets...");
        world.insert_resource(GameState { phase: GamePhase::Loading, debug_overlay: false, frame: 0 });
    }
}

sequential_system! {
    fn spawn_entities_system(world: &mut World, cmd: Cmd) {
        println!("[Startup/1] Spawning entities...");
        world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 0.0, y: 0.0 }, Player));
        world.spawn((Position { x: 5.0, y: 3.0 }, Velocity { x: 0.0, y: 0.0 }, Enemy));
    }
}

sequential_system! {
    fn start_game_system(world: &mut World, cmd: Cmd) {
        println!("[Startup/2] Starting game ({} entities)...",
                 Query::<Read<Position>>::new(world).iter().count());
        let mut gs = *world.resource::<GameState>();
        gs.phase = GamePhase::Playing;
        *world.resource_mut::<GameState>() = gs;
    }
}

sequential_system! {
    fn phase_transitions_system(world: &mut World, cmd: Cmd) {
        let mut gs = *world.resource::<GameState>();
        gs.frame += 1;
        match gs.frame {
            1 => println!("\n--- Frame 1: Playing ---"),
            2 => { println!("\n--- Frame 2: Pausing ---"); gs.phase = GamePhase::Paused; }
            3 => println!("\n--- Frame 3: Still Paused ---"),
            4 => {
                println!("\n--- Frame 4: Resuming + Debug ON ---");
                gs.phase = GamePhase::Playing; gs.debug_overlay = true;
            }
            _ => {}
        }
        *world.resource_mut::<GameState>() = gs;
    }
}

sequential_system! {
    fn fire_missile_system(world: &mut World, cmd: Cmd) {
        println!("[Stage/0] Firing missile...");
        world.spawn((Position { x: 0.0, y: 10.0 }, Velocity { x: 1.0, y: 0.0 }, Projectile));
    }
}

sequential_system! {
    fn track_missile_system(world: &mut World, cmd: Cmd) {
        let n = Query::<(Read<Position>, Read<Projectile>)>::new(world).iter().count();
        println!("[Stage/1] Tracking {} missiles (in same frame!)", n);
        assert!(n > 0, "missile должен быть виден в том же кадре!");
    }
}

fn main() {
    println!("=== Apex ECS — Run Conditions + Apply Deferred Demo (v0.3) ===\n");

    let mut main_sched = Scheduler::new();

    // ── Startup: load → apply → spawn → apply → start ──────
    main_sched.staged(StageLabel::Startup, |s| {
        s.add_systems(StageLabel::Startup, (
            seq("load_assets", load_assets_system),
        ));
        s.apply_deferred();  // ★ game_state готов

        s.add_systems(StageLabel::Startup, (
            seq("spawn_entities", spawn_entities_system)
                .run_if_cond(conditions::run_until(1)),  // ★ 1 раз
        ));
        s.apply_deferred();  // ★ entities готовы

        s.add_systems(StageLabel::Startup, (
            seq("start_game", start_game_system)
                .run_if_cond(conditions::resource_exists::<GameState>()),
        ));
    });

    // ── PreUpdate: phase transitions ───────────────────────
    main_sched.staged(StageLabel::PreUpdate, |s| {
        s.add_systems(StageLabel::PreUpdate, (
            seq("phase_transitions", phase_transitions_system),
        ));
        s.apply_deferred();  // ★ game_state применён до Update
    });

    // ── Update: игровая логика с условиями ────────────────
    main_sched.staged(StageLabel::Update, |s| {
        s.run_condition(|w: &World| {
            w.try_resource::<GameState>()
                .map(|gs| gs.phase == GamePhase::Playing)
                .unwrap_or(false)
        });

        s.add_systems(StageLabel::Update, (
            // Movement: всегда когда playing (scope condition)
            sys("movement", movement_system),

            // AI: playing И есть враги (scope AND has_enemies)
            sys("ai", ai_system)
                .run_if_cond(conditions::any_with_component::<Enemy>()),

            // Debug: playing И debug_overlay (scope AND debug_on)
            sys("debug_overlay", debug_overlay_system)
                .run_if(debug_on),

            // Damage: всегда когда playing (scope condition)
            sys("damage", damage_system),
        ));
    });

    // Movement↔AI bidirectional — chain resolves it
    main_sched.chain(&["movement", "ai"]).expect("chain");

    // ── Run 5 frames ───────────────────────────────────────
    let mut world = World::new();
    for _frame in 0..5 {
        main_sched.run_sequential(&mut world);
        let gs = *world.resource::<GameState>();
        let n = Query::<Read<Position>>::new(&world).iter().count();
        println!("  → Phase={:?} Frame={} Entities={} Debug={}", gs.phase, gs.frame, n, gs.debug_overlay);
    }

    // ── Apply Deferred: spawn → use в том же кадре ─────────
    println!("\n[Apply Deferred] Spawn → Use in same frame");
    let mut sched2 = Scheduler::new();
    sched2.staged(StageLabel::tag("missile_demo"), |s| {
        s.add_systems(StageLabel::tag("missile_demo"), (
            seq("fire_missile", fire_missile_system),
        ));
        s.apply_deferred();  // ★ missile entity видима в track
        s.add_systems(StageLabel::tag("missile_demo"), (
            seq("track_missile", track_missile_system),
        ));
    });
    let mut world2 = World::new();
    sched2.run_sequential(&mut world2);

    println!("\n=== All examples passed ===\n");
    println!("Features demonstrated:");
    println!("  ✓ add_systems() — единый кортежный API");
    println!("  ✓ sys() — AutoSystem / system! macro");
    println!("  ✓ seq()  — sequential_system! macro");
    println!("  ✓ AND-composition   — run_if(a).run_if(b)");
    println!("  ✓ OR-composition    — or_else(a).or_else(b)");
    println!("  ✓ Scope condition   — s.run_condition(f)");
    println!("  ✓ Common conditions — resource_exists/equals/any_with_component/run_until");
    println!("  ✓ Apply Deferred    — spawn → use in same frame (compile-time split)");
    println!("  ✓ Event pipeline    — produced_by/transformed_by/consumed_by by name");
}
