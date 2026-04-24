use crate::SystemId;
use std::fmt;

/// Метка этапа выполнения (Bevy-подобные именованные фазы).
///
/// Этапы выполняются строго последовательно: все системы этапа N
/// завершаются до начала этапа N+1.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StageLabel {
    /// Однократный запуск при первом `run()`.
    Startup,
    /// Выполняется до всех остальных этапов.
    First,
    /// Обработка ввода, получение данных извне.
    PreUpdate,
    /// Основная игровая логика (движение, AI, физика).
    Update,
    /// Пост-обработка: трансформации, коллизии, эффекты.
    PostUpdate,
    /// Финальная обработка, сбор статистики.
    Last,
    /// Пользовательский этап с произвольным именем.
    Custom(String),
}

impl StageLabel {
    /// Все стандартные этапы в порядке выполнения.
    pub fn standard_order() -> &'static [StageLabel] {
        &[
            StageLabel::Startup,
            StageLabel::First,
            StageLabel::PreUpdate,
            StageLabel::Update,
            StageLabel::PostUpdate,
            StageLabel::Last,
        ]
    }

    /// Приоритет этапа для сортировки (меньше = раньше).
    pub fn priority(&self) -> u8 {
        match self {
            StageLabel::Startup   => 0,
            StageLabel::First     => 1,
            StageLabel::PreUpdate => 2,
            StageLabel::Update    => 3,
            StageLabel::PostUpdate => 4,
            StageLabel::Last      => 5,
            StageLabel::Custom(_) => 6,
        }
    }
}

impl fmt::Display for StageLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageLabel::Startup   => write!(f, "Startup"),
            StageLabel::First     => write!(f, "First"),
            StageLabel::PreUpdate => write!(f, "PreUpdate"),
            StageLabel::Update    => write!(f, "Update"),
            StageLabel::PostUpdate => write!(f, "PostUpdate"),
            StageLabel::Last      => write!(f, "Last"),
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
}

impl Stage {
    pub fn new(label: StageLabel, system_ids: Vec<SystemId>, all_parallel: bool) -> Self {
        Self { label, system_ids, all_parallel }
    }

    /// Можно ли запускать системы этого Stage параллельно?
    pub fn is_parallelizable(&self) -> bool {
        self.all_parallel && self.system_ids.len() > 1
    }

    pub fn system_count(&self) -> usize {
        self.system_ids.len()
    }
}