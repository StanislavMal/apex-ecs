use crate::SystemId;
use std::any::TypeId;
use std::fmt;

/// Метка этапа выполнения (Bevy-подобные именованные фазы).
///
/// Этапы выполняются строго последовательно: все системы этапа N
/// завершаются до начала этапа N+1.
///
/// Стандартный порядок: Startup → First → PreUpdate → FixedUpdate → Update → PostUpdate → Last
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StageLabel {
    /// Однократный запуск при первом `run()`.
    Startup,
    /// Выполняется до всех остальных этапов.
    First,
    /// Обработка ввода, получение данных извне.
    PreUpdate,
    /// Фиксированный временной шаг (физика, детерминированная логика).
    /// Выполняется с гарантированным фиксированным dt (например 60 Hz).
    FixedUpdate,
    /// Основная игровая логика (движение, AI).
    Update,
    /// Пост-обработка: трансформации, коллизии, эффекты.
    PostUpdate,
    /// Финальная обработка, сбор статистики.
    Last,
    /// Пользовательский этап с произвольным именем.
    Custom(String),
}

impl StageLabel {
    /// Создать пользовательский этап по имени (аналог `StageLabel::Custom`).
    ///
    /// Это краткий конструктор для группировки систем::
    /// ```
    /// # use apex_scheduler::StageLabel;
    /// let sim = StageLabel::tag("simulation");
    /// assert_eq!(sim, StageLabel::Custom("simulation".into()));
    /// ```
    pub fn tag(name: impl Into<String>) -> Self {
        StageLabel::Custom(name.into())
    }

    /// Все стандартные этапы в порядке выполнения.
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

    /// Приоритет этапа для сортировки (меньше = раньше).
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

/// Stage — группа систем которые можно выполнять параллельно.
///
/// Все ParSystem внутри одного Stage не имеют Read/Write конфликтов
/// между собой (инвариант гарантируется Scheduler::compile).
/// Sequential системы всегда выполняются одиночно в своём Stage.
#[derive(Debug, Clone)]
pub struct Stage {
    /// Метка этапа, к которому относится эта группа.
    pub label: StageLabel,
    pub system_ids: Vec<SystemId>,
    /// True если все системы этого Stage — ParSystem без конфликтов.
    /// False если хотя бы одна Sequential система присутствует.
    pub(crate) all_parallel: bool,
    /// TypeId событий, которые системы этого Stage могут писать (emit).
    /// После выполнения Stage вызывается per-Stage flush именно этих типов.
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

    /// Можно ли запускать системы этого Stage параллельно?
    pub fn is_parallelizable(&self) -> bool {
        self.all_parallel && self.system_ids.len() > 1
    }

    /// Возвращает `true`, если Stage был скомпилирован в параллельном режиме
    /// (все системы внутри не имеют конфликтов доступа).
    pub fn is_parallel(&self) -> bool {
        self.all_parallel
    }

    pub fn system_count(&self) -> usize {
        self.system_ids.len()
    }
}
