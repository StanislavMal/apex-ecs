// Bevy 0.18 переименовал буферные `Events<T>` → `Messages<T>` (а `Event` стал триггер-типом
// наблюдателей). `Messages<T>` — точный аналог apex `Events<T>`: очередь + курсоры-читатели.
use bevy_ecs::message::{Message, Messages};

#[derive(Message)]
struct E(u64);

pub struct Benchmark;

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) -> u64 {
        let mut messages = Messages::<E>::default();
        for i in 0..10_000u64 {
            messages.write(E(i));
        }
        let mut cursor = messages.get_cursor();
        let mut sum = 0u64;
        for e in cursor.read(&messages) {
            sum += e.0;
        }
        sum
    }
}

// Steady-state кадровый цикл — bevy-аналог apex `FrameLoopBench`: каждый кадр write пачки +
// read персистентным курсором + update (ротация). Standalone `Messages<T>` (тот же уровень,
// что apex standalone `Events<T>`), одинаковая работа: N send + N read + 1 rotate на кадр.
pub struct FrameLoopBenchmark {
    frames: u64,
    per_frame: u64,
}

impl FrameLoopBenchmark {
    pub fn new() -> Self {
        Self { frames: 10_000, per_frame: 8 }
    }

    pub fn run(&mut self) -> u64 {
        let mut messages = Messages::<E>::default();
        let mut cursor = messages.get_cursor();
        let mut sum = 0u64;
        let mut n = 0u64;
        for _ in 0..self.frames {
            for _ in 0..self.per_frame {
                messages.write(E(n));
                n += 1;
            }
            for e in cursor.read(&messages) {
                sum += e.0;
            }
            messages.update(); // ротация буфера (per-кадр)
        }
        sum
    }
}
