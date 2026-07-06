//! W3-2 — events under load (plans/CORE_REFACTORING.md §8, apex-engine).
//!
//! The guide benches measured single-type throughput on a clean world. Here it is
//! the "engine under load" profile:
//!   1. send: 100k small events of a single type (baseline of the pending path);
//!   2. flush_all with 64 REGISTERED types, of which 2 are active
//!      (typical frame: many registered, only a few send) — the cost of sweeping
//!      empty queues;
//!   3. flush_events_by_type (the scheduler's targeted per-stage path) — the same
//!      64 types, flushing the 2 active ones;
//!   4. flush with a LAGGING reader (the update append path: moving old
//!      events + restoring capacity) — the allocation worst case;
//!   5. scheduler cycle: 16 systems × (writer→reader) over 8 event types,
//!      empty world — the overhead of event-ordering barriers and the per-stage flush.
//!
//! Run: `cargo run --release -p apex-bench --bin events_load`

use apex_core::prelude::*;
use std::hint::black_box;
use std::time::{Duration, Instant};

const SAMPLES: usize = 9;

fn median_of<F: FnMut() -> u64>(mut f: F) -> (Duration, u64) {
    let mut times: Vec<Duration> = Vec::with_capacity(SAMPLES);
    let mut sink = 0u64;
    for _ in 0..SAMPLES {
        let t = Instant::now();
        sink = black_box(f());
        times.push(t.elapsed());
    }
    times.sort();
    (times[SAMPLES / 2], sink)
}

fn print_row(name: &str, t: Duration, per: f64, unit: &str) {
    println!("  {:<52} {:>10.3?}   {:.1} {}", name, t, per, unit);
}

// 64 event types — generate u64 wrappers with a macro.
macro_rules! ev_types {
    ($($name:ident),+) => {
        $( #[allow(dead_code)] #[derive(Clone, Copy)] struct $name(u64); )+
        fn register_all(world: &mut World) {
            $( world.add_event::<$name>(); )+
        }
    };
}

ev_types!(
    E00, E01, E02, E03, E04, E05, E06, E07, E08, E09, E10, E11, E12, E13, E14, E15, E16, E17,
    E18, E19, E20, E21, E22, E23, E24, E25, E26, E27, E28, E29, E30, E31, E32, E33, E34, E35,
    E36, E37, E38, E39, E40, E41, E42, E43, E44, E45, E46, E47, E48, E49, E50, E51, E52, E53,
    E54, E55, E56, E57, E58, E59, E60, E61, E62, E63
);

fn main() {
    println!("=== W3-2: events under load ===\n");

    // ── [1] send 100k of a single type ─────────────────────────
    {
        let mut world = World::new();
        world.add_event::<E00>();
        let (t, _) = median_of(|| {
            for i in 0..100_000u64 {
                world.send_event(E00(i));
            }
            world.flush_all_events(); // clear before the next sample
            100_000
        });
        print_row(
            "[1] send ×100k of a single type (+flush)",
            t,
            t.as_nanos() as f64 / 100_000.0,
            "ns/send",
        );
    }

    // ── [2] flush_all: 64 registered, 2 active ─────────────────
    {
        let mut world = World::new();
        register_all(&mut world);
        let (t, _) = median_of(|| {
            for i in 0..100u64 {
                world.send_event(E07(i));
                world.send_event(E42(i));
            }
            world.flush_all_events();
            64
        });
        print_row(
            "[2] flush_all (64 types, 2 active, 200 events)",
            t,
            t.as_nanos() as f64 / 64.0,
            "ns/type",
        );
    }

    // ── [3] flush_events_by_type (scheduler path) ──────────────
    {
        let mut world = World::new();
        register_all(&mut world);
        let ids = [
            std::any::TypeId::of::<E07>(),
            std::any::TypeId::of::<E42>(),
        ];
        let (t, _) = median_of(|| {
            for i in 0..100u64 {
                world.send_event(E07(i));
                world.send_event(E42(i));
            }
            world.flush_events_by_type(&ids);
            2
        });
        print_row(
            "[3] flush_events_by_type (2 of 64, 200 events)",
            t,
            t.as_nanos() as f64 / 2.0,
            "ns/type",
        );
    }

    // ── [4] flush with a lagging reader (append path) ──────────
    {
        let mut world = World::new();
        world.add_event::<E00>();
        // We register a cursor and NEVER read it — every update takes the
        // "not everyone caught up" branch: append of old + restore of capacity.
        let _lagging = world.events_mut::<E00>().add_reader();
        let (t, _) = median_of(|| {
            // Bound the growth: the reader lags, but the buffer is cleared once per
            // sample by the separate read_all pass below.
            for i in 0..1_000u64 {
                world.send_event(E00(i));
            }
            world.flush_all_events();
            let drained = {
                let evs = world.events_mut::<E00>();
                let n = evs.len() as u64;
                evs.advance_reader_mut(&_lagging);
                n
            };
            world.flush_all_events(); // the reader has now caught up — the buffer is cleared
            drained
        });
        print_row(
            "[4] flush with a lagging reader (1k events)",
            t,
            t.as_nanos() as f64 / 1_000.0,
            "ns/event",
        );
    }

    // ── [5] scheduler: 16 systems, 8 event types ───────────────
    {
        use apex_scheduler::{Scheduler, StageLabel};

        /// Counter of delivered events (resource).
        struct Sink(u64);

        macro_rules! pipe {
            ($w:ident, $r:ident, $E:ident) => {
                system! {
                    fn $w(out: &mut Vec<$E>) {
                        out.send($E(1));
                    }
                }
                system! {
                    fn $r(evs: &[$E], sink: ResMut<Sink>) {
                        sink.0 += evs.len() as u64;
                    }
                }
            };
        }
        pipe!(w0, r0, E00);
        pipe!(w1, r1, E01);
        pipe!(w2, r2, E02);
        pipe!(w3, r3, E03);
        pipe!(w4, r4, E04);
        pipe!(w5, r5, E05);
        pipe!(w6, r6, E06);
        pipe!(w7, r7, E07);

        let mut world = World::new();
        register_all(&mut world);
        world.insert_resource(Sink(0));

        let mut sched = Scheduler::new();
        sched.add_systems(
            StageLabel::Update,
            (w0, r0, w1, r1, w2, r2, w3, r3),
        );
        sched.add_systems(
            StageLabel::Update,
            (w4, r4, w5, r5, w6, r6, w7, r7),
        );

        const FRAMES: u64 = 1_000;
        let (t, sink) = median_of(|| {
            for _ in 0..FRAMES {
                sched.run(&mut world);
            }
            world.resource::<Sink>().0
        });
        print_row(
            "[5] scheduler: 16 systems / 8 event pipelines, 1k frames",
            t,
            t.as_nanos() as f64 / FRAMES as f64,
            "ns/frame",
        );
        println!("      (sink={}, events delivered across all samples)", sink);
    }

    println!("\nDone.");
}
