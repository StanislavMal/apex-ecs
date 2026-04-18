pub mod stage;

use rustc_hash::FxHashMap;
use thiserror::Error;
use apex_graph::Graph;
use thunderdome::Index;
use apex_core::world::World;

pub use stage::Stage;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("Circular dependency between systems")]
    CircularDependency,
    #[error("System '{0}' not found")]
    SystemNotFound(String),
}

/// ID системы
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SystemId(u32);

/// Функция системы
pub type SystemFn = Box<dyn FnMut(&mut World) + Send>;

/// Описание системы
pub struct SystemDescriptor {
    pub id: SystemId,
    pub name: String,
    pub func: SystemFn,
    /// Явно: эта система должна быть после
    pub after: Vec<SystemId>,
    /// Явно: эта система должна быть до
    pub before: Vec<SystemId>,
}

/// Планировщик систем
pub struct Scheduler {
    systems: Vec<SystemDescriptor>,
    next_id: u32,
    /// Кешированный план выполнения (параллельные уровни)
    execution_plan: Option<Vec<Stage>>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            next_id: 0,
            execution_plan: None,
        }
    }

    /// Добавить систему
    pub fn add_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemId
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        let id = SystemId(self.next_id);
        self.next_id += 1;
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            func: Box::new(func),
            after: Vec::new(),
            before: Vec::new(),
        });
        // Инвалидируем кеш
        self.execution_plan = None;
        id
    }

    /// Добавить зависимость: system после after_id
    pub fn add_dependency(&mut self, system: SystemId, after_id: SystemId) {
        if let Some(s) = self.systems.iter_mut().find(|s| s.id == system) {
            s.after.push(after_id);
            self.execution_plan = None;
        }
    }

    /// Скомпилировать план выполнения
    pub fn compile(&mut self) -> Result<(), SchedulerError> {
        let mut graph: Graph<SystemId, ()> = Graph::new();

        // Добавляем все системы как узлы
        let nodes: FxHashMap<SystemId, Index> = self
            .systems
            .iter()
            .map(|s| (s.id, graph.add_node(s.id)))
            .collect();

        // Добавляем зависимости
        for system in &self.systems {
            for &after_id in &system.after {
                if let (Some(&from), Some(&to)) =
                    (nodes.get(&after_id), nodes.get(&system.id))
                {
                    graph.add_edge(from, to, ());
                }
            }
            for &before_id in &system.before {
                if let (Some(&from), Some(&to)) =
                    (nodes.get(&system.id), nodes.get(&before_id))
                {
                    graph.add_edge(from, to, ());
                }
            }
        }

        // Вычисляем параллельные уровни
        let levels = graph
            .parallel_levels()
            .map_err(|_| SchedulerError::CircularDependency)?;

        // Строим план
        let plan: Vec<Stage> = levels
            .into_iter()
            .map(|level| {
                let system_ids: Vec<SystemId> = level
                    .iter()
                    .filter_map(|&node| graph.node_data(node))
                    .copied()
                    .collect();
                Stage { system_ids }
            })
            .collect();

        self.execution_plan = Some(plan);
        Ok(())
    }

    /// Запустить все системы (однопоточно пока)
    pub fn run(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        let plan = self.execution_plan.as_ref().unwrap();

        // Собираем порядок
        let order: Vec<SystemId> = plan
            .iter()
            .flat_map(|stage| stage.system_ids.iter().copied())
            .collect();

        // Запускаем в порядке
        for sys_id in order {
            if let Some(system) = self.systems.iter_mut().find(|s| s.id == sys_id) {
                (system.func)(world);
            }
        }
    }

    pub fn system_count(&self) -> usize {
        self.systems.len()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}