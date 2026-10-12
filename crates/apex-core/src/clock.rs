//! The core's own clock (ADR-020).
//!
//! The core measures time for itself in three places: the scheduler's cost model (ADR-003, the
//! SEQ-vs-PAR verdict per stage), the `APEX_MAIN_PROF` profiler, and the `APEX_PROP_TRACE` /
//! `APEX_PROP_LADDER` diagnostics of `propagate_transforms`. None of them changes what a system
//! computes - only how fast and what gets logged.
//!
//! `std::time::Instant::now()` panics on `wasm32-unknown-unknown` ("time not implemented on this
//! platform"): the browser build trapped on the first `Scheduler::run`, although `cargo check` for
//! the target was green. Here that target gets an `Instant` that reads no clock: every measurement
//! is zero. The cost model then never warms up and the stage verdict stays on the entity heuristic -
//! the same path `ParallelPolicy::Fixed` takes; without threads on that target rayon runs a parallel
//! stage on the calling thread anyway. The profilers are switched on by environment variables, which
//! that target does not have.
//!
//! Every other target re-exports `std::time::Instant` unchanged.

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub use std::time::Instant;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use no_clock::Instant;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod no_clock {
    use std::time::Duration;

    /// A point in time on a target without a clock: every instant is the same one, so every
    /// interval is zero.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant;

    impl Instant {
        #[inline]
        pub fn now() -> Self {
            Instant
        }

        #[inline]
        pub fn elapsed(&self) -> Duration {
            Duration::ZERO
        }

        #[inline]
        pub fn duration_since(&self, _earlier: Instant) -> Duration {
            Duration::ZERO
        }
    }

    impl std::ops::Sub for Instant {
        type Output = Duration;

        #[inline]
        fn sub(self, _earlier: Instant) -> Duration {
            Duration::ZERO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Instant;

    /// Off the clockless target the core measures with the standard clock, not a stand-in: a
    /// stand-in here would silently freeze the cost model on every native build.
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    #[test]
    fn a_native_build_measures_with_the_standard_clock() {
        let t0: std::time::Instant = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(t0.elapsed() >= std::time::Duration::from_millis(2));
    }
}
