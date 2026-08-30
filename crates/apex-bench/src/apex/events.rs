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
//
// ⚠ THE RUNG IS PART OF THE INSTRUMENT (2026-08-30, BENCH-EVENTS-0830). This cell used to run
// 10 000 frames x 8 events, and at that rung the fixed per-frame cost and the per-event cost are
// the same order, so the number was decided by how the loop happened to be laid out in the
// harness binary rather than by `Events<T>`: three builds of the SAME event code gave 134.0,
// 160.7 and 198.5 us while bevy sat still at 104, and the very same `run()` called from another
// binary (`--bin events_shapes`) cost 10.4 ns/frame against criterion's 19.9. A batch of 512
// puts the per-event work 50x above the per-frame term, so what the cell reports is the quantity
// that actually differs between the two engines — and that one IS stable: apex 1.75-1.79x ahead
// across independent runs, because we push 8 bytes per event where bevy pushes a 16-byte
// `MessageInstance`. The per-frame term keeps its own cell (`events_frame_idle`), at the rung
// where IT dominates.
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
        Self { frames: 1_000, per_frame: 512 }
    }


    /// How many events one `run()` sends. The honesty guard derives the expected sum from THIS,
    /// so moving the rung cannot leave a stale constant asserting the old shape.
    pub fn event_count(&self) -> u64 {
        self.frames * self.per_frame
    }

    /// The same loop at ONE event per frame: the rotate + cursor bookkeeping is then ~80 % of
    /// the frame, so a regression in `Events::update` shows here and nowhere else.
    pub fn idle() -> Self {
        Self { frames: 10_000, per_frame: 1 }
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
