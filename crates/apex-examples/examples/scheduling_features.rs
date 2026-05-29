//! Apex ECS — Run Conditions + Apply Deferred Demo
//!
//! Демонстрирует новые возможности планировщика (v0.2):
//!
//! 1. **Run Conditions** — системы запускаются только когда condition = true
//!    - Пауза игры (1 строка на всю игровую логику)
//!    - Фазовые переходы (Loading → Playing → Paused)
//!    - Избирательное включение систем (debug overlay)
//!    - Интеграция с `system!` и `sequential_system!` макросами
//!
//! 2. **Apply Deferred** — Commands + Events применяются в том же кадре
//!    - Spawn → use immediately (без лага в 1 кадр)
//!    - Цепочки систем (sys_a → apply → sys_b)
//!    - Последовательная Startup-инициализация
//!
//! cargo run -p apex-examples --example scheduling_features

use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::{Scheduler, StageLabel};

// ── Состояния игры ───────────────────────────────────────────

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

// ── Компоненты ───────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)] struct Position { x: f32, y: f32 }
#[derive(Component, Clone, Copy, Debug)] struct Velocity { x: f32, y: f32 }
#[derive(Component, Clone, Copy, Debug)] struct Player;
#[derive(Component, Clone, Copy, Debug)] struct Enemy;
#[derive(Component, Clone, Copy, Debug)] struct Projectile;

// ── Run Conditions (fn-переиспользование) ────────────────────

fn is_playing(w: &World) -> bool {
    w.resource::<GameState>().phase == GamePhase::Playing
}

#[allow(dead_code)]
fn is_loading(w: &World) -> bool {
    w.resource::<GameState>().phase == GamePhase::Loading
}

#[allow(dead_code)]
fn is_paused(w: &World) -> bool {
    w.resource::<GameState>().phase == GamePhase::Paused
}

fn debug_on(w: &World) -> bool {
    w.resource::<GameState>().debug_overlay
}

// ── Параллельные системы (через system! макрос) ─────────────

system! {
    fn movement_system(
        q: (Read<Velocity>, Write<Position>),
    ) {
        q.for_each(|_, (vel, pos)| {
            pos.x += vel.x;
            pos.y += vel.y;
        });
    }
}

system! {
    fn debug_overlay_system(
        q: Read<Position>,
    ) {
        let count = q.iter().count();
        println!("[DEBUG] Entities with Position: {}", count);
    }
}

system! {
    fn ai_system(
        q: (Read<Position>, Write<Velocity>, Read<Enemy>),
    ) {
        q.for_each(|_, (pos, vel, _enemy)| {
            vel.x = -pos.x.signum() * 0.5;
            vel.y = -pos.y.signum() * 0.5;
        });
    }
}

system! {
    fn damage_system(
        q: Read<Player>,
    ) {
        let _ = q.iter().count(); // В реальной игре — нанесение урона
    }
}

// ── Sequential системы (через sequential_system! макрос) ────

sequential_system! {
    fn load_assets_system(world: &mut World, cmd: Cmd) {
        println!("[Startup/0] Loading assets...");
        world.insert_resource(GameState {
            phase: GamePhase::Loading,
            debug_overlay: false,
            frame: 0,
        });
    }
}

sequential_system! {
    fn spawn_entities_system(world: &mut World, cmd: Cmd) {
        println!("[Startup/1] Spawning entities (GamePhase={:?})...",
                 world.resource::<GameState>().phase);
        world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 0.0, y: 0.0 }, Player));
        world.spawn((Position { x: 5.0, y: 3.0 }, Velocity { x: 0.0, y: 0.0 }, Enemy));
    }
}

sequential_system! {
    fn start_game_system(world: &mut World, cmd: Cmd) {
        println!("[Startup/2] Starting game...");
        let mut gs = *world.resource::<GameState>();
        gs.phase = GamePhase::Playing;
        *world.resource_mut::<GameState>() = gs;
        println!("  {} entities spawned",
                 Query::<Read<Position>>::new(world).iter().count());
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
                gs.phase = GamePhase::Playing;
                gs.debug_overlay = true;
            }
            _ => {}
        }

        *world.resource_mut::<GameState>() = gs;
    }
}

sequential_system! {
    fn fire_missile_system(world: &mut World, cmd: Cmd) {
        println!("[Stage/0] Firing missile...");
        world.spawn((
            Position { x: 0.0, y: 10.0 },
            Velocity { x: 1.0, y: 0.0 },
            Projectile,
        ));
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
    println!("=== Apex ECS — Run Conditions + Apply Deferred Demo ===\n");

    // ── Фаза 1: Init main scheduler ──────────────────────────
    let mut main_sched = Scheduler::new();

    // ── Startup: загрузка → spawn → старт (с apply_deferred) ─
    main_sched.staged(StageLabel::Startup, |s| {
        s.add_system("load_assets", load_assets_system);
        s.apply_deferred();  // ★ GameState виден в spawn_entities
        s.add_system("spawn_entities", spawn_entities_system);
        s.apply_deferred();  // ★ entity видимы в start_game
        s.add_system("start_game", start_game_system);
    });

    // ── PreUpdate: фазовые переходы ──────────────────────────
    main_sched.staged(StageLabel::PreUpdate, |s| {
        s.add_system("phase_transitions", phase_transitions_system);
        s.apply_deferred();  // ★ game_state применён до Update
    });

    // ── Update: игровая логика с run conditions ──────────────
    main_sched.staged(StageLabel::Update, |s| {
        let _m = s.add_auto_system("movement", movement_system);
        s.set_run_if("movement", is_playing).unwrap();

        let _a = s.add_auto_system("ai", ai_system);
        s.set_run_if("ai", Box::new(|w: &World| is_playing(w) && {
            Query::<Read<Enemy>>::new(w).iter().count() > 0
        })).unwrap();

        let _d = s.add_auto_system("debug_overlay", debug_overlay_system);
        s.set_run_if("debug_overlay", debug_on).unwrap();

        let _dm = s.add_auto_system("damage", damage_system);
    });

    // Movement (Write<Pos>, Read<Vel>) и AI (Read<Pos>, Write<Vel>)
    // имеют BidirectionalWriteRead → явно задаём порядок
    main_sched.chain(&["movement", "ai"]).expect("chain movement → ai");

    // ── Фаза 2: run 5 frames ─────────────────────────────────
    let mut world = World::new();

    for _frame in 0..5 {
        main_sched.run_sequential(&mut world);

        let gs = *world.resource::<GameState>();
        let pos_count = Query::<Read<Position>>::new(&world).iter().count();
        println!("  → Phase={:?}  Frame={}  Entities={}  Debug={}",
                 gs.phase, gs.frame, pos_count, gs.debug_overlay);
    }

    // ── Фаза 3: Apply Deferred — spawn → use в том же кадре ──
    println!("\n--- Apply Deferred: Spawn → Use in same frame ---");
    let mut sched2 = Scheduler::new();

    sched2.staged(StageLabel::tag("missile_demo"), |s| {
        s.add_system("fire_missile", fire_missile_system);
        s.apply_deferred();  // ★ missile entity видима в track_missile
        s.add_system("track_missile", track_missile_system);
    });

    let mut world2 = World::new();
    sched2.run_sequential(&mut world2);

    println!("\n=== All examples passed ===\n");
    println!("Features demonstrated:");
    println!("  ✓ Run conditions + system! macro");
    println!("  ✓ Run conditions + sequential_system! macro");
    println!("  ✓ Apply Deferred + sequential_system! macro");
    println!("  ✓ Reusable run conditions (fn pointers)");
    println!("  ✓ Compile-time stage splitting (zero runtime cost)");
}
