use crate::SystemId;

/// Стейдж — группа систем которые можно выполнять параллельно
#[derive(Debug, Clone)]
pub struct Stage {
    pub system_ids: Vec<SystemId>,
}

impl Stage {
    pub fn new(system_ids: Vec<SystemId>) -> Self {
        Self { system_ids }
    }

    pub fn is_parallelizable(&self) -> bool {
        self.system_ids.len() > 1
    }
}