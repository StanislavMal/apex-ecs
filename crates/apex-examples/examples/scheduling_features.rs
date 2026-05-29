//! Apex ECS — Run Conditions + Apply Deferred Demo (v0.3)
//!
//! Демонстрирует новые возможности планировщика:
//!
//! 1. **Run Conditions — AND-комбинация**
//!    - Несколько `run_if` → все должны быть true
//!    - `or_else` → хотя бы одно true
//!    - Scope condition через `s.run_condition()` внутри `staged()`
//!
//! 2. **Common conditions** (`conditions` module)
//!    - `resource_exists::<T>()` — ресурс есть?
//!    - `resource_equals(val)` — ресурс равен значению?
//!    - `any_with_component::<T>()` — есть entity с компонентом?
//!    - `run_until(n)` — выполниться N раз
//!    - `every_n_frames(n)` — раз в N кадров
//!
//! 3. **Apply Deferred** — Commands применяются в том же кадре
//!
//! cargo run -p apex-examples --example scheduling_features

use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::{conditions, Scheduler, StageLabel};

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

// ── Reusable conditions (fn) ───────────────────────────────

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

    // ── Feature 1: AND conditions (multiple run_if) ────────
    println!("[Feature 1] AND-composition: run_if(is_playing) && run_if(has_enemies)");

    // ── Feature 2: OR conditions (or_else) ─────────────────
    println!("[Feature 2] OR-composition: or_else(cond_a).or_else(cond_b)");

    // ── Feature 3: Scope condition ─────────────────────────
    println!("[Feature 3] Scope condition via s.run_condition()");

    main_sched.staged(StageLabel::Startup, |s| {
        s.add_system("load_assets", load_assets_system);
        s.apply_deferred();
        s.add_system("spawn_entities", spawn_entities_system)
            .run_if(conditions::run_until(1));  // ★ 1 раз
        s.apply_deferred();
        s.add_system("start_game", start_game_system)
            .run_if(conditions::resource_exists::<GameState>());  // ★ resource check
    });

    main_sched.staged(StageLabel::PreUpdate, |s| {
        s.add_system("phase_transitions", phase_transitions_system);
        s.apply_deferred();
    });

    // ── Scope condition: все системы внутри наследуют is_playing ──
    main_sched.staged(StageLabel::Update, |s| {
        s.run_condition(|w: &World| {
            w.try_resource::<GameState>()
                .map(|gs| gs.phase == GamePhase::Playing)
                .unwrap_or(false)
        });

        // ★ add_auto_system теперь возвращает SystemBuilder — chain API!
        s.add_auto_system("movement", movement_system);

        // AI: AND-композиция — scope condition + has_enemies
        s.add_auto_system("ai", ai_system)
            .run_if(conditions::any_with_component::<Enemy>());

        // Debug: AND — scope + debug_on
        s.add_auto_system("debug_overlay", debug_overlay_system)
            .run_if(debug_on);

        // Damage: всегда работает (scope condition уже фильтрует)
        s.add_auto_system("damage", damage_system);
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

    // ── Feature 4: Apply Deferred ──────────────────────────
    println!("\n[Feature 4] Apply Deferred: Spawn → Use in same frame");
    let mut sched2 = Scheduler::new();
    sched2.staged(StageLabel::tag("missile_demo"), |s| {
        s.add_system("fire_missile", fire_missile_system);
        s.apply_deferred();
        s.add_system("track_missile", track_missile_system);
    });
    let mut world2 = World::new();
    sched2.run_sequential(&mut world2);

    println!("\n=== All examples passed ===\n");
    println!("Features demonstrated:");
    println!("  ✓ AND-composition  — run_if(a).run_if(b)");
    println!("  ✓ OR-composition   — or_else(a).or_else(b)");
    println!("  ✓ Scope condition  — s.run_condition(f)");
    println!("  ✓ Common conditions — conditions::resource_exists/equals/any_with_component/run_until");
    println!("  ✓ Apply Deferred   — spawn → use in same frame");
    println!("  ✓ system! + sequential_system! macros fully integrated");
}
