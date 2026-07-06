use apex_core::events::Events;

// Events — send 10k events + read all with a single reader (full round-trip). Not covered by other
// benches; the baseline inter-system communication path. Standalone `Events<T>` — the same level of
// abstraction as bevy `Events<T>` (without World/registration). Returns the sum (honesty guard:
// both implementations must read all 10k ⇒ sum == 49995000).
#[derive(Clone, Copy)]
pub struct E(pub u64);

pub struct EventsBench;

impl Default for EventsBench {
    fn default() -> Self {
        Self::new()
    }
}

impl EventsBench {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) -> u64 {
        let mut events = Events::<E>::new();
        let cursor = events.add_reader();
        for i in 0..10_000u64 {
            events.send(E(i));
        }
        events.update(); // pending → readable
        let mut sum = 0u64;
        for e in events.read(&cursor).iter() {
            sum += e.0;
        }
        sum
    }
}

// Steady-state frame loop: EVERY frame — send a batch, read with a persistent reader,
// rotate the buffer. Unlike a one-off batch (EventsBench) it amortizes the per-frame cost of
// send+read+rotate (rotation is called EVERY frame) — closer to a real engine frame.
// A persistent cursor — like a system reader. Both implementations read all events.
pub struct FrameLoopBench {
    frames: u64,
    per_frame: u64,
}

impl Default for FrameLoopBench {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameLoopBench {
    pub fn new() -> Self {
        Self { frames: 10_000, per_frame: 8 }
    }

    pub fn run(&mut self) -> u64 {
        let mut events = Events::<E>::new();
        let cursor = events.add_reader();
        let mut sum = 0u64;
        let mut n = 0u64;
        for _ in 0..self.frames {
            for _ in 0..self.per_frame {
                events.send(E(n));
                n += 1;
            }
            events.update(); // pending → readable (rotation)
            for e in events.read(&cursor).iter() {
                sum += e.0;
            }
        }
        sum
    }
}

/// Correctness, NOT throughput: a reader that runs only every `read_every` frames
/// (a run-condition / state-gated system) while events are written every frame and
/// the buffer is rotated every frame. Returns `(written, read)`. Apex preserves a
/// lagging reader's events (no-loss) ⇒ `read == written`. Bevy's messages expire
/// after 2 frames ⇒ a gated reader drops the events written while it slept.
pub fn gated_reader_readout(frames: u64, read_every: u64) -> (u64, u64) {
    let mut events = Events::<E>::new();
    let cursor = events.add_reader();
    let (mut written, mut read) = (0u64, 0u64);
    for frame in 0..frames {
        events.send(E(frame));
        written += 1;
        events.update(); // engine rotates the buffer every frame
        if frame % read_every == read_every - 1 {
            read += events.read(&cursor).as_slice().len() as u64;
        }
    }
    // final drain so the tail (written after the last read-frame) is counted
    read += events.read(&cursor).as_slice().len() as u64;
    (written, read)
}
