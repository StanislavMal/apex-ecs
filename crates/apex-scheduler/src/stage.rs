use crate::SystemId;

/// Stage — группа систем которые можно выполнять параллельно.
///
/// Все ParSystem внутри одного Stage не имеют Read/Write конфликтов
/// между собой (инвариант гарантируется Scheduler::compile).
/// Sequential системы всегда выполняются одиночно в своём Stage.
#[derive(Debug, Clone)]
pub struct Stage {
    pub system_ids: Vec<SystemId>,
    /// True если все системы этого Stage — ParSystem без конфликтов.
    /// False если хотя бы одна Sequential система присутствует.
    pub(crate) all_parallel: bool,
}

impl Stage {
    pub fn new(system_ids: Vec<SystemId>, all_parallel: bool) -> Self {
        Self { system_ids, all_parallel }
    }

    /// Можно ли запускать системы этого Stage параллельно?
    pub fn is_parallelizable(&self) -> bool {
        self.all_parallel && self.system_ids.len() > 1
    }

    pub fn system_count(&self) -> usize {
        self.system_ids.len()
    }
}