use crate::SystemId;

/// Stage — группа систем которые можно выполнять параллельно.
///
/// Все системы внутри одного Stage не имеют Read/Write конфликтов
/// между собой (гарантируется компилятором графа в `Scheduler::compile`).
#[derive(Debug, Clone)]
pub struct Stage {
    pub system_ids: Vec<SystemId>,
}

impl Stage {
    pub fn new(system_ids: Vec<SystemId>) -> Self {
        Self { system_ids }
    }

    /// Можно ли запускать системы этого стейджа параллельно?
    /// True если в стейдже больше одной системы.
    pub fn is_parallelizable(&self) -> bool {
        self.system_ids.len() > 1
    }

    pub fn system_count(&self) -> usize {
        self.system_ids.len()
    }
}