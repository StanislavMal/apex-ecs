use crate::SystemId;
use std::any::TypeId;
use std::fmt;

/// Execution stage label (Bevy-like named phases).
///
/// Stages run strictly sequentially: all systems of stage N
/// finish before stage N+1 begins.
///
/// Standard order: Startup → First → PreUpdate → FixedUpdate → Update → PostUpdate → Last
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StageLabel {
    /// Runs once on the first `run()`.
    Startup,
    /// Runs before all other stages.
    First,
    /// Input handling, fetching external data.
    PreUpdate,
    /// Fixed time step (physics, deterministic logic).
    /// Runs with a guaranteed fixed dt (e.g. 60 Hz).
    FixedUpdate,
    /// Core game logic (movement, AI).
    Update,
    /// Post-processing: transforms, collisions, effects.
    PostUpdate,
    /// Final processing, statistics gathering.
    Last,
    /// User-defined stage with an arbitrary name.
    Custom(String),
}

impl StageLabel {
    /// Create a user-defined stage by name (alias for `StageLabel::Custom`).
    ///
    /// A short constructor for grouping systems::
    /// ```
    /// # use apex_scheduler::StageLabel;
    /// let sim = StageLabel::tag("simulation");
    /// assert_eq!(sim, StageLabel::Custom("simulation".into()));
    /// ```
    pub fn tag(name: impl Into<String>) -> Self {
        StageLabel::Custom(name.into())
    }

    /// All standard stages in execution order.
    pub fn standard_order() -> &'static [StageLabel] {
        &[
            StageLabel::Startup,
            StageLabel::First,
            StageLabel::PreUpdate,
            StageLabel::FixedUpdate,
            StageLabel::Update,
            StageLabel::PostUpdate,
            StageLabel::Last,
        ]
    }

    /// Stage priority for sorting (lower = earlier).
    pub fn priority(&self) -> u8 {
        match self {
            StageLabel::Startup => 0,
            StageLabel::First => 1,
            StageLabel::PreUpdate => 2,
            StageLabel::FixedUpdate => 3,
            StageLabel::Update => 4,
            StageLabel::PostUpdate => 5,
            StageLabel::Last => 6,
            StageLabel::Custom(_) => 7,
        }
    }
}

impl fmt::Display for StageLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageLabel::Startup => write!(f, "Startup"),
            StageLabel::First => write!(f, "First"),
            StageLabel::PreUpdate => write!(f, "PreUpdate"),
            StageLabel::FixedUpdate => write!(f, "FixedUpdate"),
            StageLabel::Update => write!(f, "Update"),
            StageLabel::PostUpdate => write!(f, "PostUpdate"),
            StageLabel::Last => write!(f, "Last"),
            StageLabel::Custom(name) => write!(f, "Custom({})", name),
        }
    }
}

/// Stage — a group of systems that can run in parallel.
///
/// All ParSystems within a single Stage have no Read/Write conflicts
/// with each other (invariant guaranteed by Scheduler::compile).
/// Sequential systems always run alone in their Stage.
#[derive(Debug, Clone)]
pub struct Stage {
    /// Label of the stage this group belongs to.
    pub label: StageLabel,
    pub system_ids: Vec<SystemId>,
    /// True if all systems of this Stage are conflict-free ParSystems.
    /// False if at least one Sequential system is present.
    pub(crate) all_parallel: bool,
    /// TypeIds of events the systems of this Stage may emit.
    /// After the Stage runs, a per-Stage flush of exactly these types is invoked.
    pub(crate) emit_event_types: Vec<TypeId>,
}

impl Stage {
    pub fn new(label: StageLabel, system_ids: Vec<SystemId>, all_parallel: bool) -> Self {
        Self {
            label,
            system_ids,
            all_parallel,
            emit_event_types: Vec::new(),
        }
    }

    /// Can the systems of this Stage be run in parallel?
    pub fn is_parallelizable(&self) -> bool {
        self.all_parallel && self.system_ids.len() > 1
    }

    /// Returns `true` if the Stage was compiled in parallel mode
    /// (all systems within have no access conflicts).
    pub fn is_parallel(&self) -> bool {
        self.all_parallel
    }

    pub fn system_count(&self) -> usize {
        self.system_ids.len()
    }
}
