//! W3-2 — events под нагрузкой (plans/CORE_REFACTORING.md §8, apex-engine).
//!
//! Бенчи руководства меряли throughput одного типа на чистом мире. Здесь —
//! профиль «движок под нагрузкой»:
//!   1. send: 100k мелких событий одного типа (бейзлайн pending-пути);
//!   2. flush_all при 64 ЗАРЕГИСТРИРОВАННЫХ типах, из которых активны 2
//!      (типовой кадр: зарегистрировано много, шлют единицы) — цена свипа
//!      пустых очередей;
//!   3. flush_events_by_type (точечный per-stage путь планировщика) — те же
//!      64 типа, флашим 2 активных;
//!   4. flush с ОТСТАЮЩИМ читателем (append-путь update: перенос старых
//!      событий + восстановление capacity) — аллокационно худший случай;
//!   5. цикл планировщика: 16 систем × (writer→reader) через 8 типов событий,
//!      пустой мир — оверхед event-ordering барьеров и per-stage флаша.
//!
//! Запуск: `cargo run --release -p apex-bench --bin events_load`

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

// 64 типа событий — генерим макросом обёртки над u64.
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
    println!("=== W3-2: events под нагрузкой ===\n");

    // ── [1] send 100k одного типа ──────────────────────────────
    {
        let mut world = World::new();
        world.add_event::<E00>();
        let (t, _) = median_of(|| {
            for i in 0..100_000u64 {
                world.send_event(E00(i));
            }
            world.flush_all_events(); // очистка к следующему сэмплу
            100_000
        });
        print_row(
            "[1] send ×100k одного типа (+flush)",
            t,
            t.as_nanos() as f64 / 100_000.0,
            "ns/send",
        );
    }

    // ── [2] flush_all: 64 зарегистрированных, 2 активных ───────
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
            "[2] flush_all (64 типов, 2 активных, 200 событий)",
            t,
            t.as_nanos() as f64 / 64.0,
            "ns/тип",
        );
    }

    // ── [3] flush_events_by_type (путь планировщика) ───────────
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
            "[3] flush_events_by_type (2 из 64, 200 событий)",
            t,
            t.as_nanos() as f64 / 2.0,
            "ns/тип",
        );
    }

    // ── [4] flush с отстающим читателем (append-путь) ──────────
    {
        let mut world = World::new();
        world.add_event::<E00>();
        // Курсор регистрируем и НИКОГДА не читаем — каждый update идёт по
        // ветке «не все догнали»: append старых + восстановление capacity.
        let _lagging = world.events_mut::<E00>().add_reader();
        let (t, _) = median_of(|| {
            // Ограничиваем рост: читатель отстал, но буфер чистится раз в
            // сэмпл сторонним read_all-проходом ниже.
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
            world.flush_all_events(); // теперь читатель догнал — буфер очистится
            drained
        });
        print_row(
            "[4] flush с отстающим читателем (1k событий)",
            t,
            t.as_nanos() as f64 / 1_000.0,
            "ns/событие",
        );
    }

    // ── [5] планировщик: 16 систем, 8 типов событий ────────────
    {
        use apex_scheduler::{Scheduler, StageLabel};

        /// Счётчик доставленных событий (ресурс).
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
            "[5] scheduler: 16 систем / 8 event-пайплайн, 1k кадров",
            t,
            t.as_nanos() as f64 / FRAMES as f64,
            "ns/кадр",
        );
        println!("      (sink={}, событий доставлено за все сэмплы)", sink);
    }

    println!("\nГотово.");
}
