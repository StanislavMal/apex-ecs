//! FixedUpdate — fixed simulation step (D2-5, analogous to Bevy `Time<Fixed>`).
//!
//! The [`FixedTime`] resource accumulates real frame time; before the
//! `StageLabel::FixedUpdate` stage the scheduler asks it for the number of steps
//! and runs the stage that many times (each step — with its own command
//! application and per-stage event flush). Leftover time carries over to the next frame.
//!
//! Wiring:
//! - in the engine `apex-app` inserts a default `FixedTime` and feeds it
//!   `Time.delta_seconds` each frame;
//! - in the bare core: `world.insert_resource(FixedTime::from_hz(60.0))` and
//!   `world.resource_mut::<FixedTime>().accumulate(frame_dt)` before `run()`.
//! - WITHOUT the resource the FixedUpdate stage runs once per frame (like any other).
//!
//! ⚠ Interaction with DtConditioner (apex-window): the conditioned sim-dt is
//! almost a multiple of the display period — the accumulator works on top of it
//! normally, the fixed step's `dt` is independent (typically 1/60). The
//! conditioner's EMA/deadband removes INPUT jitter; the death spiral is bounded
//! by `max_steps_per_frame`.

/// Fixed-step resource (see the module documentation).
#[derive(Debug, Clone)]
pub struct FixedTime {
    /// Fixed simulation step, seconds (typically 1/60).
    pub dt: f32,
    /// Cap of steps per frame — protection against the "death spiral" (slow frame →
    /// more steps → even slower). Excess time is DISCARDED.
    pub max_steps_per_frame: u32,
    accumulator: f32,
}

impl Default for FixedTime {
    fn default() -> Self {
        Self::from_hz(60.0)
    }
}

impl FixedTime {
    pub fn from_hz(hz: f32) -> Self {
        Self {
            dt: 1.0 / hz,
            max_steps_per_frame: 8,
            accumulator: 0.0,
        }
    }

    pub fn from_dt(dt: f32) -> Self {
        Self {
            dt,
            max_steps_per_frame: 8,
            accumulator: 0.0,
        }
    }

    /// Accumulate real frame time (called once per frame before `run()`).
    #[inline]
    pub fn accumulate(&mut self, frame_dt: f32) {
        self.accumulator += frame_dt.max(0.0);
    }

    /// Accumulated, not yet processed time (for render interpolation:
    /// `alpha = overstep_fraction()`).
    #[inline]
    pub fn overstep(&self) -> f32 {
        self.accumulator
    }

    /// Fraction of the unprocessed step `[0, 1)` — interpolation coefficient.
    #[inline]
    pub fn overstep_fraction(&self) -> f32 {
        (self.accumulator / self.dt).fract()
    }

    /// How many steps to run this frame; deducts them from the accumulator.
    /// Called by the scheduler before the FixedUpdate stage.
    pub fn drain_steps(&mut self) -> usize {
        if self.dt <= 0.0 {
            return 0;
        }
        let steps = (self.accumulator / self.dt) as u32;
        if steps > self.max_steps_per_frame {
            // Death spiral: run the cap, discard the excess.
            self.accumulator = 0.0;
            return self.max_steps_per_frame as usize;
        }
        self.accumulator -= steps as f32 * self.dt;
        steps as usize
    }
}
