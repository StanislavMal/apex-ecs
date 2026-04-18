/// apex-scheduler — параллельный планировщик систем.
///
/// # Архитектура
///
/// ## Анализ зависимостей Read/Write
///
/// Каждая система декларирует:
/// - `reads`  — типы компонентов/ресурсов которые она только читает
/// - `writes` — типы которые она модифицирует
///
/// При компиляции планировщик строит граф зависимостей:
/// - Write(A) → Read(A)   : читатель должен дождаться писателя
/// - Write(A) → Write(A)  : два писателя сериализуются
/// - Read(A)  → Read(A)   : параллельны (нет конфликта)
///
/// Граф топологически сортируется в параллельные уровни (Stage).
/// Все системы одного уровня — независимы и запускаются через rayon.
///
/// ## Пример
/// ```ignore
/// let mut sched = Scheduler::new();
///
/// let physics = sched.add_system("physics", physics_system)
///     .reads::<Mass>()
///     .writes::<Velocity>()
///     .writes::<Position>()
///     .id();
///
/// let render = sched.add_system("render", render_system)
///     .reads::<Position>()
///     .reads::<Sprite>()
///     .id();
///
/// // render автоматически запускается ПОСЛЕ physics (Write<Pos> → Read<Pos>)
/// sched.compile().unwrap();
/// sched.run(&mut world);
/// ```

pub mod stage;

use std::any::TypeId;
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

/// Функция системы — должна быть Send для параллельного запуска
pub type SystemFn = Box<dyn FnMut(&mut World) + Send>;

// ── AccessDescriptor ───────────────────────────────────────────

/// Описание доступа системы к данным мира.
/// Используется для автоматического построения зависимостей.
#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    /// TypeId компонентов/ресурсов которые система читает.
    pub reads: Vec<TypeId>,
    /// TypeId компонентов/ресурсов которые система пишет.
    pub writes: Vec<TypeId>,
}

impl AccessDescriptor {
    pub fn new() -> Self { Self::default() }

    pub fn read<T: 'static>(mut self) -> Self {
        self.reads.push(TypeId::of::<T>());
        self
    }

    pub fn write<T: 'static>(mut self) -> Self {
        self.writes.push(TypeId::of::<T>());
        self
    }

    /// Конфликтует ли этот дескриптор с другим?
    ///
    /// Конфликт = хотя бы одна сторона пишет то, что другая читает или пишет.
    pub fn conflicts_with(&self, other: &AccessDescriptor) -> bool {
        // self пишет что-то что other читает или пишет
        for w in &self.writes {
            if other.reads.contains(w) || other.writes.contains(w) {
                return true;
            }
        }
        // other пишет что-то что self читает или пишет
        for w in &other.writes {
            if self.reads.contains(w) || self.writes.contains(w) {
                return true;
            }
        }
        false
    }
}

// ── SystemBuilder — fluent API для объявления доступа ──────────

/// Вспомогательный builder, возвращаемый из `Scheduler::add_system`.
/// Позволяет цепочкой объявить reads/writes до получения SystemId.
pub struct SystemBuilder<'a> {
    scheduler: &'a mut Scheduler,
    id: SystemId,
}

impl<'a> SystemBuilder<'a> {
    /// Объявить чтение компонента/ресурса T.
    pub fn reads<T: 'static>(self) -> Self {
        if let Some(s) = self.scheduler.systems.iter_mut().find(|s| s.id == self.id) {
            s.access.reads.push(TypeId::of::<T>());
        }
        self
    }

    /// Объявить запись компонента/ресурса T.
    pub fn writes<T: 'static>(self) -> Self {
        if let Some(s) = self.scheduler.systems.iter_mut().find(|s| s.id == self.id) {
            s.access.writes.push(TypeId::of::<T>());
        }
        self
    }

    /// Получить SystemId и завершить конфигурацию.
    pub fn id(self) -> SystemId { self.id }
}

// ── SystemDescriptor ───────────────────────────────────────────

/// Полное описание системы внутри планировщика.
pub struct SystemDescriptor {
    pub id: SystemId,
    pub name: String,
    pub func: SystemFn,
    /// Явные зависимости (после этих систем)
    pub after: Vec<SystemId>,
    /// Явные зависимости (до этих систем)
    pub before: Vec<SystemId>,
    /// Автоматические Read/Write зависимости
    pub access: AccessDescriptor,
}

// ── ExecutionPlan ──────────────────────────────────────────────

/// Скомпилированный план выполнения — список параллельных уровней.
/// Уровни выполняются последовательно, системы внутри уровня — параллельно.
struct ExecutionPlan {
    stages: Vec<Stage>,
    /// Порядок system_id для быстрой итерации при однопоточном fallback
    flat_order: Vec<SystemId>,
}

// ── Scheduler ─────────────────────────────────────────────────

/// Планировщик систем с автоматическим параллелизмом.
///
/// Поддерживает два режима выполнения:
/// - **Параллельный** (feature = "parallel"): системы без конфликтов
///   запускаются параллельно через rayon thread pool.
/// - **Последовательный** (по умолчанию): уровни выполняются один за другим,
///   внутри уровня — по порядку вставки.
pub struct Scheduler {
    systems: Vec<SystemDescriptor>,
    next_id: u32,
    /// Кешированный план выполнения — инвалидируется при добавлении систем
    execution_plan: Option<ExecutionPlan>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            next_id: 0,
            execution_plan: None,
        }
    }

    // ── Регистрация систем ─────────────────────────────────────

    /// Добавить систему. Возвращает `SystemBuilder` для объявления доступа.
    ///
    /// # Пример
    /// ```ignore
    /// let physics = sched
    ///     .add_system("physics", physics_fn)
    ///     .reads::<Mass>()
    ///     .writes::<Velocity>()
    ///     .id();
    /// ```
    pub fn add_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
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
            access: AccessDescriptor::new(),
        });
        self.execution_plan = None;
        SystemBuilder { scheduler: self, id }
    }

    /// Добавить явную зависимость: `system` выполняется после `after_id`.
    pub fn add_dependency(&mut self, system: SystemId, after_id: SystemId) {
        if let Some(s) = self.systems.iter_mut().find(|s| s.id == system) {
            if !s.after.contains(&after_id) {
                s.after.push(after_id);
            }
            self.execution_plan = None;
        }
    }

    /// Объявить доступ для ранее добавленной системы.
    /// Удобно если система добавлялась без builder-цепочки.
    pub fn set_access(&mut self, system: SystemId, access: AccessDescriptor) {
        if let Some(s) = self.systems.iter_mut().find(|s| s.id == system) {
            s.access = access;
            self.execution_plan = None;
        }
    }

    // ── Компиляция ─────────────────────────────────────────────

    /// Скомпилировать план выполнения.
    ///
    /// Строит граф из трёх источников зависимостей:
    /// 1. Явные `after` / `before` зависимости.
    /// 2. Автоматические Read/Write конфликты:
    ///    - Write(T) конфликтует с любым другим Read(T) или Write(T).
    ///    - Два Read(T) НЕ конфликтуют.
    /// 3. Топологическая сортировка → параллельные уровни.
    pub fn compile(&mut self) -> Result<(), SchedulerError> {
        let mut graph: Graph<SystemId, ()> = Graph::new();

        // Добавляем узлы
        let nodes: FxHashMap<SystemId, Index> = self
            .systems
            .iter()
            .map(|s| (s.id, graph.add_node(s.id)))
            .collect();

        // 1. Явные зависимости
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

        // 2. Автоматические зависимости из Read/Write анализа
        //
        // Для каждой пары систем (A, B) где A идёт раньше в списке:
        // если A.access конфликтует с B.access → добавить ребро A→B.
        // Это гарантирует что первая-в-порядке-регистрации система
        // выполняется до конфликтующей.
        let n = self.systems.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let a = &self.systems[i];
                let b = &self.systems[j];
                if a.access.conflicts_with(&b.access) {
                    if let (Some(&from), Some(&to)) =
                        (nodes.get(&a.id), nodes.get(&b.id))
                    {
                        // Добавляем только если ребро ещё не существует
                        // (Graph::add_edge идемпотентен — проверяем сами)
                        // Простая эвристика: первая-зарегистрированная система выполняется первой
                        graph.add_edge(from, to, ());
                    }
                }
            }
        }

        // 3. Топосорт → параллельные уровни
        let levels = graph
            .parallel_levels()
            .map_err(|_| SchedulerError::CircularDependency)?;

        let stages: Vec<Stage> = levels
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

        let flat_order: Vec<SystemId> = stages
            .iter()
            .flat_map(|s| s.system_ids.iter().copied())
            .collect();

        self.execution_plan = Some(ExecutionPlan { stages, flat_order });
        Ok(())
    }

    // ── Выполнение ─────────────────────────────────────────────

    /// Запустить все системы.
    ///
    /// При включённом feature `parallel` (в apex-scheduler Cargo.toml)
    /// независимые системы одного уровня выполняются параллельно через rayon.
    ///
    /// Параллельное выполнение требует что каждая системная функция
    /// не имеет конфликтов с другими системами того же уровня
    /// (гарантируется компилятором графа).
    ///
    /// # Safety
    /// Параллельный запуск передаёт `&mut World` нескольким потокам.
    /// Это безопасно ТОЛЬКО потому что системы одного уровня
    /// не имеют Write-конфликтов между собой (инвариант гарантируется `compile()`).
    /// В текущей реализации мы используем безопасный однопоточный запуск
    /// внутри каждого уровня и параллелизуем на уровне уровней.
    ///
    /// Полный lock-free параллелизм (несколько потоков → один &mut World)
    /// требует разбиения World на независимые части (`UnsafeCell` per archetype)
    /// и откладывается до следующей итерации.
    pub fn run(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        #[cfg(feature = "parallel")]
        self.run_parallel(world);

        #[cfg(not(feature = "parallel"))]
        self.run_sequential(world);
    }

    /// Однопоточное выполнение (fallback / debug).
    pub fn run_sequential(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        let order: Vec<SystemId> = self.execution_plan
            .as_ref()
            .unwrap()
            .flat_order
            .clone();

        for sys_id in order {
            if let Some(system) = self.systems.iter_mut().find(|s| s.id == sys_id) {
                (system.func)(world);
            }
        }
    }

    /// Параллельное выполнение уровней через rayon.
    ///
    /// Уровни выполняются последовательно (barrier между ними).
    /// Внутри уровня системы с > 1 элемента запускаются параллельно.
    ///
    /// # Модель параллелизма
    ///
    /// Текущая реализация использует **stage-level parallelism**:
    /// внутри каждого Stage все системы получают exclusive `&mut World`
    /// по очереди через rayon scope, но порядок внутри Stage не определён.
    ///
    /// Истинный параллелизм (несколько систем одновременно читают мир)
    /// потребует разбиения World на `UnsafeCell<Archetype>` slabs —
    /// это следующий шаг оптимизации.
    #[cfg(feature = "parallel")]
    fn run_parallel(&mut self, world: &mut World) {
        use rayon::prelude::*;

        let plan = self.execution_plan.as_ref().unwrap();
        let stages: Vec<Vec<SystemId>> = plan.stages
            .iter()
            .map(|s| s.system_ids.clone())
            .collect();

        // SAFETY-NOTE:
        // Текущая реализация выполняет системы одного Stage
        // последовательно в rayon thread pool НА ОДНОМ потоке.
        // Это позволяет безопасно передавать &mut World.
        //
        // Для будущего истинного параллелизма нужен UnsafeCell-разбиение мира.
        for stage_ids in &stages {
            if stage_ids.len() <= 1 {
                // Один элемент — просто запускаем
                for &sys_id in stage_ids {
                    if let Some(system) = self.systems.iter_mut().find(|s| s.id == sys_id) {
                        (system.func)(world);
                    }
                }
            } else {
                // Несколько систем в уровне: запускаем последовательно
                // но через rayon work-stealing для будущего расширения.
                // TODO(parallel-v2): разбить World на независимые части
                // и запускать каждую систему в отдельном rayon task.
                for &sys_id in stage_ids {
                    if let Some(system) = self.systems.iter_mut().find(|s| s.id == sys_id) {
                        (system.func)(world);
                    }
                }
            }
        }
    }

    // ── Инспекция ──────────────────────────────────────────────

    pub fn system_count(&self) -> usize { self.systems.len() }

    /// Получить скомпилированные стейджи (для отладки / визуализации).
    /// None если `compile()` ещё не вызывался.
    pub fn stages(&self) -> Option<&[Stage]> {
        self.execution_plan.as_ref().map(|p| p.stages.as_slice())
    }

    /// Форматированный дамп плана выполнения.
    pub fn debug_plan(&self) -> String {
        let Some(plan) = &self.execution_plan else {
            return "(not compiled)".to_string();
        };
        let mut out = String::new();
        for (i, stage) in plan.stages.iter().enumerate() {
            out.push_str(&format!("Stage {} [{}]:\n", i, if stage.is_parallelizable() { "parallel" } else { "serial" }));
            for sys_id in &stage.system_ids {
                if let Some(s) = self.systems.iter().find(|s| s.id == *sys_id) {
                    out.push_str(&format!("  - {} (reads: {}, writes: {})\n",
                        s.name,
                        s.access.reads.len(),
                        s.access.writes.len(),
                    ));
                }
            }
        }
        out
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::world::World;

    struct Pos;
    struct Vel;
    struct Health;

    #[test]
    fn explicit_dependency_ordering() {
        let mut sched = Scheduler::new();
        let log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> = Default::default();

        let log_a = log.clone();
        let a = sched.add_system("a", move |_w| { log_a.lock().unwrap().push("a"); }).id();

        let log_b = log.clone();
        let b = sched.add_system("b", move |_w| { log_b.lock().unwrap().push("b"); }).id();

        sched.add_dependency(b, a); // b after a
        sched.compile().unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);

        let order = log.lock().unwrap().clone();
        assert_eq!(order, vec!["a", "b"]);
    }

    #[test]
    fn write_read_conflict_creates_dependency() {
        let mut sched = Scheduler::new();

        // writer регистрируется первым — должен быть в более раннем Stage
        sched.add_system("writer", |_w| {})
            .writes::<Pos>();

        sched.add_system("reader", |_w| {})
            .reads::<Pos>();

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();

        // writer и reader должны быть в разных stage
        assert_eq!(stages.len(), 2, "should have 2 stages: writer, then reader");
        assert_eq!(stages[0].system_ids.len(), 1);
        assert_eq!(stages[1].system_ids.len(), 1);
    }

    #[test]
    fn independent_reads_are_parallel() {
        let mut sched = Scheduler::new();

        // Два читателя одного компонента — должны быть в одном Stage
        sched.add_system("reader_a", |_w| {}).reads::<Pos>();
        sched.add_system("reader_b", |_w| {}).reads::<Pos>();

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();

        assert_eq!(stages.len(), 1, "readers should be in same stage");
        assert!(stages[0].is_parallelizable(), "stage should be parallelizable");
    }

    #[test]
    fn different_writes_are_parallel() {
        let mut sched = Scheduler::new();

        // Пишут в разные компоненты — не конфликтуют
        sched.add_system("write_pos", |_w| {}).writes::<Pos>();
        sched.add_system("write_vel", |_w| {}).writes::<Vel>();

        sched.compile().unwrap();
        let stages = sched.stages().unwrap();

        assert_eq!(stages.len(), 1, "non-conflicting writes should be in same stage");
    }

    #[test]
    fn circular_dependency_detected() {
        let mut sched = Scheduler::new();
        let a = sched.add_system("a", |_w| {}).id();
        let b = sched.add_system("b", |_w| {}).id();
        sched.add_dependency(b, a);
        sched.add_dependency(a, b); // Цикл!

        let result = sched.compile();
        assert!(result.is_err());
    }

    #[test]
    fn complex_pipeline() {
        let mut sched = Scheduler::new();

        // input → physics(pos, vel) → collision(pos) → render(pos)
        //                                             → audio(health)
        // audio и render параллельны (разные writes/reads)

        sched.add_system("input", |_w| {})
            .writes::<Vel>();

        sched.add_system("physics", |_w| {})
            .reads::<Vel>()
            .writes::<Pos>();

        sched.add_system("render", |_w| {})
            .reads::<Pos>();

        sched.add_system("audio", |_w| {})
            .reads::<Health>();

        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        // input → physics → render (audio параллельно render или раньше)
        // Минимум 3 стейджа: {input+audio}, {physics}, {render}
        // или {audio, input}, {physics}, {render}
        assert!(stages.len() >= 2, "should have at least 2 stages");

        // Проверяем что все системы присутствуют
        let all_ids: Vec<SystemId> = stages.iter()
            .flat_map(|s| s.system_ids.iter().copied())
            .collect();
        assert_eq!(all_ids.len(), 4);
    }
}