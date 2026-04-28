//! Apex ECS — Stages & Tags Example
//!
//! Демонстрирует группировку систем по этапам (StageLabel::tag),
//! скоуп-регистрацию staged() и автоматическое переупорядочивание
//! parallel-систем перед sequential в пределах одного этапа.
//!
//! Проблема: два плагина должны гарантировать порядок выполнения,
//! не зная имён систем друг друга.
//!
//! Решение: каждый плагин регистрирует свои системы в своём этапе,
//! а приложение задаёт порядок этапов одной строкой.
//!
//! Дополнительно: sequential-системы можно регистрировать вперемешку
//! с parallel — планировщик сам выстроит правильный порядок.
//!
//! cargo run -p apex-examples --example stages --release --features parallel
//! cargo test --workspace
//!
//! Ожидаемый вывод:
//!   [Input] read_keyboard  ← Plugin A
//!   [Sim]   physics        ← Plugin B (параллельно с ai)
//!   [Sim]   ai             ← Plugin B
//!   [SimSeq] print_stats   ← Sequential (выполняется после parallel внутри sim)
//!   [Render] draw          ← Plugin A
//!   [Update] particles     ← без явного этапа → default "update"
//!   [UpdateSeq] finalize   ← Sequential (выполняется после particles)

use apex_core::prelude::*;
use apex_core::access_desc;
use apex_scheduler::{Scheduler, StageLabel};

// ── Компоненты ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)] struct FrameInput  { keys: u32 }

// ── Plugin A ──────────────────────────────────────────────────
// Регистрирует системы в этапах "input" и "render".

fn plugin_a(sched: &mut Scheduler) {
    sched
        .staged(StageLabel::tag("input"), |s| {
            s.add_par_access(
                "read_keyboard",
                access_desc!(write<FrameInput>),
                |ctx| {
                    ctx.resource_mut::<FrameInput>().keys = 42;
                    println!("  [Input] read_keyboard");
                },
            );
        })
        .staged(StageLabel::tag("render"), |s| {
            s.add_par_access(
                "draw",
                access_desc!(read<FrameInput>),
                |ctx| {
                    let input = ctx.resource::<FrameInput>();
                    println!("  [Render] draw (pressed keys: {})", input.keys);
                },
            );
        });
}

// ── Plugin B ──────────────────────────────────────────────────
// Регистрирует системы в этапе "simulation".
// Не знает о существовании Plugin A — использует только свой этап.

fn plugin_b(sched: &mut Scheduler) {
    sched.staged(StageLabel::tag("sim"), |s| {
        // sequential-система регистрируется ПЕРЕД parallel
        // (проверка автоматического переупорядочивания: sequential будет
        //  вынесена в конец этапа sim после compile)
        s.add_system("print_stats", |_| {
            println!("  [SimSeq] print_stats");
        });

        s.add_par("physics", |_| {
            println!("  [Sim]   physics");
        });
        s.add_par("ai", |_| {
            println!("  [Sim]   ai");
        });
    });
}

// ── main ──────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Stages & Tags ===\n");

    let mut world = World::new();
    world.insert_resource(FrameInput { keys: 0 });

    let mut sched = Scheduler::new();

    // Меняем этап по умолчанию: системы без явного этапа
    // попадут не в стандартный Update, а в "update".
    sched.set_default_stage(StageLabel::tag("update"));

    // Плагины регистрируют свои системы
    plugin_a(&mut sched);
    plugin_b(&mut sched);

    // Sequential-система вне плагинов — попадёт в "update" (default_stage_label)
    sched.add_system("finalize", |_| {
        println!("  [UpdateSeq] finalize");
    });

    // Parallel-система зарегистрирована ПОСЛЕ sequential (та же проверка)
    sched.add_par("particles", |_| {
        println!("  [Update] particles");
    });

    // Задаём порядок этапов — одна строка.
    // input → sim → render → update (остальные в конец)
    sched.configure_stages(vec![
        StageLabel::tag("input"),
        StageLabel::tag("sim"),
        StageLabel::tag("render"),
    ]);

    sched.compile_with_world(&world).unwrap();

    println!("\n--- Execution plan ---\n{}", sched.debug_plan());

    println!("\n--- Run ---\n");
    sched.run(&mut world);

    println!("\nВсе системы отработали в порядке: input → sim → render → update.");
    println!("Внутри 'sim' physics и ai выполняются параллельно (нет конфликтов).");
}
