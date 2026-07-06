//! Apex ECS — Stages & Tags Example
//!
//! Demonstrates grouping systems by stage (`StageLabel::tag`) through the single
//! `add_systems(label, …)` entry point and automatic reordering of
//! parallel systems before sequential ones within a single stage.
//!
//! Problem: two plugins must guarantee execution order
//! without knowing each other's system names.
//!
//! Solution: each plugin registers its systems in its own stage,
//! and the application defines the stage order in a single line.
//!
//! Additionally: sequential systems can be registered interleaved
//! with parallel ones — the scheduler builds the correct order itself.
//!
//! cargo run -p apex-examples --example stages --release
//! cargo test --workspace
//!
//! Expected output:
//!   [Input] read_keyboard  ← Plugin A
//!   [Sim]   physics        ← Plugin B (in parallel with ai)
//!   [Sim]   ai             ← Plugin B
//!   [SimSeq] print_stats   ← Sequential (runs after parallel within sim)
//!   [Render] draw          ← Plugin A
//!   [Update] particles     ← stage "update"
//!   [UpdateSeq] finalize   ← Sequential (runs after particles)

use apex_core::access_desc;
use apex_core::prelude::*;
use apex_scheduler::{par, par_access, seq, Scheduler, StageLabel};

// ── Components ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)] struct FrameInput  { keys: u32 }

// ── Plugin A ──────────────────────────────────────────────────
// Registers systems in the "input" and "render" stages.

fn plugin_a(sched: &mut Scheduler) {
    sched.add_systems(
        StageLabel::tag("input"),
        par_access("read_keyboard", access_desc!(write<FrameInput>), |ctx| {
            // Raw par path: access is declared above via `access_desc!`, so the
            // `_unchecked` mutable accessor is the correct (validated) escape (F3.2).
            ctx.resource_mut_unchecked::<FrameInput>().keys = 42;
            println!("  [Input] read_keyboard");
        }),
    );
    sched.add_systems(
        StageLabel::tag("render"),
        par_access("draw", access_desc!(read<FrameInput>), |ctx| {
            let input = ctx.resource::<FrameInput>();
            println!("  [Render] draw (pressed keys: {})", input.keys);
        }),
    );
}

// ── Plugin B ──────────────────────────────────────────────────
// Registers systems in the "sim" stage.
// Unaware of Plugin A's existence — uses only its own stage.

fn plugin_b(sched: &mut Scheduler) {
    sched.add_systems(StageLabel::tag("sim"), (
        // the sequential system is registered BEFORE the parallel ones
        // (checks automatic reordering: the sequential one is moved
        //  to the end of the sim stage after compile)
        seq("print_stats", |_w: &mut World| {
            println!("  [SimSeq] print_stats");
        }),
        par("physics", |_| {
            println!("  [Sim]   physics");
        }),
        par("ai", |_| {
            println!("  [Sim]   ai");
        }),
    ));
}

// ── main ──────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Stages & Tags ===\n");

    let mut world = World::new();
    world.insert_resource(FrameInput { keys: 0 });

    let mut sched = Scheduler::new();

    // Plugins register their systems
    plugin_a(&mut sched);
    plugin_b(&mut sched);

    // Application systems — in their own "update" stage; sequential and parallel
    // can be registered in any order (the same reordering check).
    sched.add_systems(StageLabel::tag("update"), (
        seq("finalize", |_w: &mut World| {
            println!("  [UpdateSeq] finalize");
        }),
        par("particles", |_| {
            println!("  [Update] particles");
        }),
    ));

    // Define the stage order — a single line.
    // input → sim → render → update (the rest go to the end)
    sched.configure_stages(vec![
        StageLabel::tag("input"),
        StageLabel::tag("sim"),
        StageLabel::tag("render"),
    ]);

    sched.compile_with_world(&world).unwrap();

    println!("\n--- Execution plan ---\n{}", sched.debug_plan());

    println!("\n--- Run ---\n");
    sched.run(&mut world);

    println!("\nAll systems ran in order: input → sim → render → update.");
    println!("Within 'sim', physics and ai run in parallel (no conflicts).");
}
