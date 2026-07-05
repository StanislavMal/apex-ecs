//! Error policy — the systematic form of §0.2a ("loud, never silent").
//!
//! Every conscious drop, refusal or no-op the engine performs instead of
//! corrupting state (inserting into a dead entity, refusing to re-spawn a live
//! one, dropping snapshot bytes with no serde) is reported as an [`Anomaly`]
//! through the per-[`World`](crate::World) [`ErrorHandler`]. The handler decides
//! what to *do* about it: log (throttled — the historical `warn_once!`
//! behavior), panic (strict mode for tests/CI), stay silent, or run a custom
//! callback. It also **counts** anomalies, so a headless or CI run can assert
//! "zero drops this frame".
//!
//! Why a per-`World` field and not a process-global handler (Bevy's
//! `GLOBAL_ERROR_HANDLER`): we run multiple worlds (`IsolatedWorld`, snapshots,
//! editor scratch worlds) in one process, and each wants its own policy — a
//! strict test world can panic on drops while the live world logs. A global
//! `OnceLock` cannot express that. This mirrors the `ChunkConfig`-on-`World`
//! precedent (engine config is a `World` field with a `from_env()` opt-in).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::entity::Entity;

/// Severity of a reported [`Anomaly`]. Selects the log level in
/// [`ErrorMode::Warn`] (`warn!` vs `error!`) and the counter it increments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Recoverable: the operation was skipped/dropped but the world stays valid.
    Warn,
    /// Serious data loss or a broken invariant the caller almost certainly did
    /// not intend.
    Error,
}

/// A single §0.2a event: a conscious drop/refusal the engine surfaces instead
/// of swallowing. Built at the call site and passed by reference to the
/// [`ErrorHandler`]; it borrows its `detail` message, so it never allocates.
pub struct Anomaly<'a> {
    /// Severity — picks log level and counter.
    pub severity: Severity,
    /// The operation that hit the anomaly, e.g. `"World::insert"`.
    pub op: &'static str,
    /// The entity involved, if any.
    pub entity: Option<Entity>,
    /// The component type name involved, if any. Borrowed for the anomaly's
    /// lifetime, so a `&'static` type name and a runtime snapshot name both fit.
    pub component: Option<&'a str>,
    /// Human-readable explanation of what was dropped/refused and why.
    pub detail: fmt::Arguments<'a>,
}

impl fmt::Display for Anomaly<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.op)?;
        if let Some(c) = self.component {
            write!(f, "::<{c}>")?;
        }
        if let Some(e) = self.entity {
            write!(f, " on entity {}:{}", e.index(), e.generation())?;
        }
        write!(f, " — {}", self.detail)
    }
}

/// What an [`ErrorHandler`] does with each reported [`Anomaly`].
#[derive(Clone)]
pub enum ErrorMode {
    /// Log once per call site (throttled, historical `warn_once!` behavior) at
    /// `warn!`/`error!` per [`Severity`]. The default.
    Warn,
    /// Panic on the first anomaly — strict mode for tests/CI that must catch
    /// silent losses. Not throttled.
    Panic,
    /// Do nothing but keep counting — for headless runs that assert on
    /// [`ErrorHandler::counts`] afterwards.
    Silent,
    /// Invoke a user callback for every anomaly (not throttled).
    Custom(Arc<dyn Fn(&Anomaly) + Send + Sync>),
}

impl fmt::Debug for ErrorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorMode::Warn => f.write_str("Warn"),
            ErrorMode::Panic => f.write_str("Panic"),
            ErrorMode::Silent => f.write_str("Silent"),
            ErrorMode::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

/// A snapshot of anomaly counters (see [`ErrorHandler::counts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnomalyCounts {
    /// Number of [`Severity::Warn`] anomalies reported.
    pub warn: u64,
    /// Number of [`Severity::Error`] anomalies reported.
    pub error: u64,
}

impl AnomalyCounts {
    /// Total across both severities. `counts().total() == 0` is the "no drops"
    /// assertion for a CI/headless run.
    pub fn total(&self) -> u64 {
        self.warn + self.error
    }
}

/// Per-[`World`](crate::World) policy for §0.2a anomalies — the systematic form
/// of the historical `warn_once!`. See the [module docs](self).
///
/// ```
/// use apex_core::{ErrorHandler, ErrorMode};
/// let mut world = apex_core::World::new();
/// // Strict: turn every conscious drop into a panic (tests/CI).
/// world.set_error_mode(ErrorMode::Panic);
/// // Or configure from `APEX_ERROR_MODE`:
/// world.set_error_handler(ErrorHandler::from_env());
/// ```
pub struct ErrorHandler {
    mode: ErrorMode,
    warn_count: AtomicU64,
    error_count: AtomicU64,
}

impl ErrorHandler {
    /// Build a handler with an explicit mode (counters start at zero).
    pub fn new(mode: ErrorMode) -> Self {
        Self {
            mode,
            warn_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
        }
    }

    /// Read `APEX_ERROR_MODE` (`warn` | `panic` | `silent`, case-insensitive).
    /// Unset or unrecognized → [`ErrorMode::Warn`]. Apply with
    /// [`World::set_error_handler`](crate::World::set_error_handler).
    pub fn from_env() -> Self {
        let mode = match std::env::var("APEX_ERROR_MODE") {
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "panic" => ErrorMode::Panic,
                "silent" => ErrorMode::Silent,
                _ => ErrorMode::Warn,
            },
            Err(_) => ErrorMode::Warn,
        };
        Self::new(mode)
    }

    /// The active mode.
    pub fn mode(&self) -> &ErrorMode {
        &self.mode
    }

    /// Replace the mode. Counters are preserved.
    pub fn set_mode(&mut self, mode: ErrorMode) {
        self.mode = mode;
    }

    /// Snapshot of the anomaly counters (incremented on every report,
    /// regardless of mode or log throttling).
    pub fn counts(&self) -> AnomalyCounts {
        AnomalyCounts {
            warn: self.warn_count.load(Ordering::Relaxed),
            error: self.error_count.load(Ordering::Relaxed),
        }
    }

    /// Reset both counters to zero (e.g. at the start of a frame before an
    /// end-of-frame "zero drops" assertion).
    pub fn reset_counts(&self) {
        self.warn_count.store(0, Ordering::Relaxed);
        self.error_count.store(0, Ordering::Relaxed);
    }

    /// Report an anomaly. **Not called directly** — use the [`anomaly!`] macro,
    /// which supplies the per-call-site throttle token. Always increments the
    /// severity counter; `throttle` gates only the [`ErrorMode::Warn`] log so a
    /// hot misuse path logs once but still counts every hit.
    #[doc(hidden)]
    pub fn report(&self, anomaly: &Anomaly<'_>, throttle: &std::sync::atomic::AtomicBool) {
        match anomaly.severity {
            Severity::Warn => self.warn_count.fetch_add(1, Ordering::Relaxed),
            Severity::Error => self.error_count.fetch_add(1, Ordering::Relaxed),
        };
        match &self.mode {
            ErrorMode::Silent => {}
            ErrorMode::Panic => panic!("{anomaly}"),
            ErrorMode::Custom(f) => f(anomaly),
            ErrorMode::Warn => {
                if !throttle.swap(true, Ordering::Relaxed) {
                    match anomaly.severity {
                        Severity::Warn => log::warn!("{anomaly}"),
                        Severity::Error => log::error!("{anomaly}"),
                    }
                }
            }
        }
    }
}

impl Default for ErrorHandler {
    fn default() -> Self {
        Self::new(ErrorMode::Warn)
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end: drive a real §0.2a misuse path (insert on a despawned entity)
    //! and assert each [`ErrorMode`] behaves. Counting is independent of the
    //! log throttle, so `counts()` is deterministic even though the `warn!`
    //! fires at most once per process per call site.
    use super::*;
    use crate::component::Component;
    use crate::World;
    use std::sync::atomic::{AtomicU32, Ordering as O};
    use std::sync::Mutex;

    #[derive(Debug, PartialEq)]
    struct C(u32);
    impl Component for C {}

    /// Spawn then despawn — `e` now names a dead entity whose generation is stale.
    fn dead_entity(world: &mut World) -> Entity {
        let e = world.spawn((C(1),));
        world.despawn(e);
        e
    }

    #[test]
    fn counts_every_hit_even_when_log_is_throttled() {
        let mut world = World::new();
        let e = dead_entity(&mut world);
        world.insert(e, C(2));
        world.insert(e, C(3)); // same call site: log throttled, count is not
        assert_eq!(world.error_handler().counts().warn, 2);
        assert_eq!(world.error_handler().counts().total(), 2);
    }

    #[test]
    fn silent_mode_counts_without_logging() {
        let mut world = World::new();
        world.set_error_mode(ErrorMode::Silent);
        let e = dead_entity(&mut world);
        world.insert(e, C(2));
        assert_eq!(world.error_handler().counts().warn, 1);
    }

    #[test]
    #[should_panic(expected = "World::insert")]
    fn panic_mode_turns_a_conscious_drop_into_a_panic() {
        let mut world = World::new();
        world.set_error_mode(ErrorMode::Panic);
        let e = dead_entity(&mut world);
        world.insert(e, C(2));
    }

    #[test]
    fn custom_mode_receives_structured_context() {
        let seen_op: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let seen_index = Arc::new(AtomicU32::new(u32::MAX));
        let (op2, idx2) = (seen_op.clone(), seen_index.clone());

        let mut world = World::new();
        world.set_error_mode(ErrorMode::Custom(Arc::new(move |a: &Anomaly| {
            *op2.lock().unwrap() = a.op.to_string();
            if let Some(e) = a.entity {
                idx2.store(e.index(), O::Relaxed);
            }
        })));
        let e = dead_entity(&mut world);
        world.insert(e, C(2));

        assert_eq!(&*seen_op.lock().unwrap(), "World::insert");
        assert_eq!(seen_index.load(O::Relaxed), e.index());
        // Custom mode still counts.
        assert_eq!(world.error_handler().counts().warn, 1);
    }

    #[test]
    fn reset_counts_clears_the_tally() {
        let mut world = World::new();
        world.set_error_mode(ErrorMode::Silent);
        let e = dead_entity(&mut world);
        world.insert(e, C(2));
        assert_eq!(world.error_handler().counts().total(), 1);
        world.error_handler_mut().reset_counts();
        assert_eq!(world.error_handler().counts().total(), 0);
    }
}
