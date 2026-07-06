// Bevy 0.18 renamed the buffered `Events<T>` → `Messages<T>` (and `Event` became the observer
// trigger type). `Messages<T>` is the exact counterpart of apex `Events<T>`: a queue + reader cursors.
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

// Steady-state frame loop — bevy counterpart of apex `FrameLoopBench`: each frame write a batch +
// read with a persistent cursor + update (rotation). Standalone `Messages<T>` (same level as apex
// standalone `Events<T>`), identical work: N send + N read + 1 rotate per frame.
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
            messages.update(); // buffer rotation (per-frame)
        }
        sum
    }
}

/// Bevy counterpart of apex `gated_reader_readout`: same protocol (write every
/// frame, rotate every frame, read only every `read_every` frames). Bevy's
/// `Messages` expire after 2 rotations, so a gated reader drops the events
/// written while it was not running ⇒ `read < written`.
pub fn gated_reader_readout(frames: u64, read_every: u64) -> (u64, u64) {
    let mut messages = Messages::<E>::default();
    let mut cursor = messages.get_cursor();
    let (mut written, mut read) = (0u64, 0u64);
    for frame in 0..frames {
        messages.write(E(frame));
        written += 1;
        if frame % read_every == read_every - 1 {
            read += cursor.read(&messages).count() as u64;
        }
        messages.update(); // rotate every frame (drops 2-frame-old messages)
    }
    read += cursor.read(&messages).count() as u64;
    (written, read)
}
