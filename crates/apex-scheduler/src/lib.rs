//! apex-scheduler — гибридный планировщик систем.
//!
//! # Типы систем
//!
//! ## Sequential — `FnMut(&mut World)`
//! Полный доступ. Всегда одиночно. Для structural changes.
//!
//! ## ParSystem (struct) — явный AccessDescriptor
//! ```ignore
//! struct MySystem;
//! impl ParSystem for MySystem {
//!     fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
//!     fn run(&mut self, ctx: SystemContext<'_>) { ... }
//! }
//! sched.add_par_system("my", MySystem);
//! ```
//!
//! ## FnParSystem — функция + AccessDescriptor
//! ```ignore
//! sched.add_fn_par_system("my", |ctx| { ... },
//!     AccessDescriptor::new().write::<Pos>()
//! );
//! ```
//!
//! # Параллельное выполнение (`feature = "parallel"`)
//!
//! ```text
//! Stage 0 [PARALLEL]: physics ‖ health_clamp   ← rayon::scope
//! Stage 1 [sequential]: damage_apply            ← &mut World
//! Stage 2 [PARALLEL]: ai ‖ loot_drop            ← rayon::scope
//! ```
//!
//! ## Почему это безопасно
//!
//! Системы в одном Stage не имеют Write-конфликтов (гарантия `compile()`).
//! Данные компонентов хранятся как `*mut u8` в `Column::data` (raw heap).
//! Разные Column → разные буферы памяти → нет aliasing.
//!
//! `SendPtr<T>` решает проблему `*mut T: !Send`: это newtype с
//! `unsafe impl Send`, аналогично `NonNull` в std.

pub mod stage;

use rustc_hash::FxHashMap;
use thiserror::Error;
use apex_graph::Graph;
use thunderdome::Index;
use apex_core::{
    AccessDescriptor,
    world::{World, ParallelWorld},
    query::{WorldQuery, Query},
    component::Tick,
    entity::Entity,
    system_param::{Res, ResMut, EventReader, EventWriter},
};

pub use stage::Stage;
pub use apex_core::AccessDescriptor as Access;

// ── SendPtr ────────────────────────────────────────────────────
//
// Обёртка над *mut T, которую можно отправить в другой поток.
//
// # Safety
// Использующий код обязан гарантировать:
// - Указатель валиден на всё время жизни в потоке
// - Нет aliasing доступов из других потоков к тем же данным
//
// В scheduler это выполняется:
// - Каждый SendPtr<SystemDescriptor> создаётся из уникального индекса
//   в `self.systems` — нет двух потоков с одним ptr
// - rayon::scope гарантирует завершение всех потоков до выхода,
//   т.е. ptr живёт достаточно долго
#[derive(Copy, Clone)]
struct SendPtr<T>(*mut T);

// SAFETY: см. документацию выше. Использование строго ограничено
// run_hybrid_parallel где уникальность ptr гарантирована кодом.
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

impl<T> SendPtr<T> {
    #[inline]
    unsafe fn as_mut(&self) -> &mut T { &mut *self.0 }
}

// ── SchedulerError ─────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("Circular dependency between systems")]
    CircularDependency,
    #[error("System '{0}' not found")]
    SystemNotFound(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SystemId(pub u32);

pub type SystemFn = Box<dyn FnMut(&mut World) + Send>;

// ── SystemContext ──────────────────────────────────────────────

/// Ограниченный view на World для ParSystem.
///
/// Планировщик гарантирует что в одном Stage нет Write-конфликтов,
/// поэтому мутация данных компонентов через raw ptr в Column безопасна.
pub struct SystemContext<'w> {
    world:   *const World,
    _marker: std::marker::PhantomData<&'w World>,
}

// SAFETY: SystemContext хранит только *const World.
// Мутабельный доступ происходит через Column::data (*mut u8) —
// разные Column = разные буферы, aliasing исключён compile()-инвариантом.
unsafe impl Send for SystemContext<'_> {}
unsafe impl Sync for SystemContext<'_> {}

impl<'w> SystemContext<'w> {
    pub(crate) fn new(world: &'w World) -> Self {
        Self { world: world as *const World, _marker: std::marker::PhantomData }
    }

    #[inline]
    pub fn query<Q: WorldQuery>(&self) -> Query<'_, Q> {
        unsafe { Query::new(&*self.world) }
    }

    #[inline]
    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> Query<'_, Q> {
        unsafe { Query::new_with_tick(&*self.world, last_run) }
    }

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(unsafe { (*self.world).resource::<T>() })
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = (*self.world)
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        EventReader(unsafe { (*self.world).events::<T>() })
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = (*self.world)
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered");
            EventWriter::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn entity_count(&self) -> usize {
        unsafe { (*self.world).entity_count() }
    }

    #[inline]
    pub fn for_each<Q, F>(&self, f: F)
    where
        Q: WorldQuery,
        F: FnMut(Entity, Q::Item<'_>),
    {
        self.query::<Q>().for_each(f);
    }

    #[inline]
    pub fn for_each_component<Q, F>(&self, f: F)
    where
        Q: WorldQuery,
        F: FnMut(Q::Item<'_>),
    {
        self.query::<Q>().for_each_component(f);
    }
}

// ── ParSystem trait ────────────────────────────────────────────

/// Параллельная система с декларативным доступом.
pub trait ParSystem: Send + Sync {
    fn access() -> AccessDescriptor where Self: Sized;
    fn run(&mut self, ctx: SystemContext<'_>);
    fn name() -> &'static str where Self: Sized { std::any::type_name::<Self>() }
}

// ── FnParSystem ────────────────────────────────────────────────

struct FnParSystem {
    func:   Box<dyn FnMut(SystemContext<'_>) + Send + Sync>,
    access: AccessDescriptor,
}

impl ParSystem for FnParSystem {
    fn access() -> AccessDescriptor where Self: Sized { AccessDescriptor::new() }
    fn run(&mut self, ctx: SystemContext<'_>) { (self.func)(ctx); }
}

// ── SystemKind ─────────────────────────────────────────────────

enum SystemKind {
    Sequential(SystemFn),
    Parallel {
        system: Box<dyn ParSystem>,
        access: AccessDescriptor,
    },
}

impl SystemKind {
    fn is_parallel(&self) -> bool { matches!(self, SystemKind::Parallel { .. }) }

    fn access(&self) -> Option<&AccessDescriptor> {
        match self {
            SystemKind::Parallel { access, .. } => Some(access),
            SystemKind::Sequential(_)           => None,
        }
    }
}

// ── SystemDescriptor ───────────────────────────────────────────

struct SystemDescriptor {
    id:     SystemId,
    name:   String,
    kind:   SystemKind,
    after:  Vec<SystemId>,
    before: Vec<SystemId>,
}

// ── SystemBuilder ──────────────────────────────────────────────

pub struct SystemBuilder<'a> {
    scheduler: &'a mut Scheduler,
    id:        SystemId,
}

impl<'a> SystemBuilder<'a> {
    pub fn id(self) -> SystemId { self.id }
}

// ── ExecutionPlan ──────────────────────────────────────────────

struct ExecutionPlan {
    stages:     Vec<Stage>,
    flat_order: Vec<SystemId>,
}

// ── Scheduler ─────────────────────────────────────────────────

pub struct Scheduler {
    systems:        Vec<SystemDescriptor>,
    next_id:        u32,
    execution_plan: Option<ExecutionPlan>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self { systems: Vec::new(), next_id: 0, execution_plan: None }
    }

    // ── Регистрация ────────────────────────────────────────────

    pub fn add_system<F>(&mut self, name: impl Into<String>, func: F) -> SystemBuilder<'_>
    where
        F: FnMut(&mut World) + Send + 'static,
    {
        let id = SystemId(self.next_id);
        self.next_id += 1;
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Sequential(Box::new(func)),
            after:  Vec::new(),
            before: Vec::new(),
        });
        self.execution_plan = None;
        SystemBuilder { scheduler: self, id }
    }

    pub fn add_par_system<S: ParSystem + 'static>(
        &mut self,
        name:   impl Into<String>,
        system: S,
    ) -> SystemId {
        let id     = SystemId(self.next_id);
        self.next_id += 1;
        let access = S::access();
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel { system: Box::new(system), access },
            after:  Vec::new(),
            before: Vec::new(),
        });
        self.execution_plan = None;
        id
    }

    pub fn add_fn_par_system<F>(
        &mut self,
        name:   impl Into<String>,
        func:   F,
        access: AccessDescriptor,
    ) -> SystemId
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let id       = SystemId(self.next_id);
        self.next_id += 1;
        let system   = FnParSystem { func: Box::new(func), access: access.clone() };
        self.systems.push(SystemDescriptor {
            id,
            name: name.into(),
            kind: SystemKind::Parallel { system: Box::new(system), access },
            after:  Vec::new(),
            before: Vec::new(),
        });
        self.execution_plan = None;
        id
    }

    pub fn add_dependency(&mut self, system: SystemId, after_id: SystemId) {
        if let Some(s) = self.systems.iter_mut().find(|s| s.id == system) {
            if !s.after.contains(&after_id) { s.after.push(after_id); }
            self.execution_plan = None;
        }
    }

    // ── Компиляция ─────────────────────────────────────────────

    pub fn compile(&mut self) -> Result<(), SchedulerError> {
        let mut graph: Graph<SystemId, ()> = Graph::new();

        let nodes: FxHashMap<SystemId, Index> = self.systems
            .iter()
            .map(|s| (s.id, graph.add_node(s.id)))
            .collect();

        // 1. Явные зависимости
        for system in &self.systems {
            for &after_id in &system.after {
                if let (Some(&from), Some(&to)) =
                    (nodes.get(&after_id), nodes.get(&system.id))
                { graph.add_edge(from, to, ()); }
            }
            for &before_id in &system.before {
                if let (Some(&from), Some(&to)) =
                    (nodes.get(&system.id), nodes.get(&before_id))
                { graph.add_edge(from, to, ()); }
            }
        }

        // 2. Sequential барьеры
        let n = self.systems.len();
        for i in 0..n {
            if !self.systems[i].kind.is_parallel() {
                for j in 0..i {
                    if let (Some(&from), Some(&to)) =
                        (nodes.get(&self.systems[j].id), nodes.get(&self.systems[i].id))
                    { graph.add_edge(from, to, ()); }
                }
                for j in (i + 1)..n {
                    if let (Some(&from), Some(&to)) =
                        (nodes.get(&self.systems[i].id), nodes.get(&self.systems[j].id))
                    { graph.add_edge(from, to, ()); }
                }
            }
        }

        // 3. Write/Read конфликты
        for i in 0..n {
            for j in (i + 1)..n {
                let ai = match self.systems[i].kind.access() { Some(a) => a, None => continue };
                let aj = match self.systems[j].kind.access() { Some(a) => a, None => continue };
                if ai.conflicts_with(aj) {
                    if let (Some(&from), Some(&to)) =
                        (nodes.get(&self.systems[i].id), nodes.get(&self.systems[j].id))
                    { graph.add_edge(from, to, ()); }
                }
            }
        }

        // 4. Топосорт → параллельные уровни
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
                let all_parallel = system_ids.iter().all(|sid| {
                    self.systems.iter()
                        .find(|s| s.id == *sid)
                        .map(|s| s.kind.is_parallel())
                        .unwrap_or(false)
                });
                Stage::new(system_ids, all_parallel)
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

    /// Выбирает параллельный или последовательный путь.
    pub fn run(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        #[cfg(feature = "parallel")]
        self.run_hybrid_parallel(world);

        #[cfg(not(feature = "parallel"))]
        self.run_sequential(world);
    }

    /// Последовательное выполнение — используется в тестах и как fallback.
    pub fn run_sequential(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        let order: Vec<SystemId> = self.execution_plan
            .as_ref().unwrap().flat_order.clone();

        for sys_id in order {
            if let Some(system) = self.systems.iter_mut().find(|s| s.id == sys_id) {
                match &mut system.kind {
                    SystemKind::Sequential(f)             => f(world),
                    SystemKind::Parallel { system, .. }   => {
                        system.run(SystemContext::new(world));
                    }
                }
            }
        }
    }

    /// Истинный параллельный запуск через Rayon.
    ///
    /// # Архитектура
    ///
    /// ```text
    ///  main thread
    ///      │
    ///      ├─ Stage 0 [PARALLEL] ─ rayon::scope ────────────────┐
    ///      │    ├── thread A: physics.run(ctx)    ──────────►    │
    ///      │    └── thread B: health.run(ctx)     ──────────►    │
    ///      │  <── join: все потоки завершены ────────────────────┘
    ///      │
    ///      ├─ Stage 1 [sequential]: commands(world) ← &mut World
    ///      │
    ///      └─ Stage 2 [PARALLEL] ─ rayon::scope ────────────────┐
    ///           ├── thread A: ai.run(ctx)         ──────────►    │
    ///           └── thread B: loot.run(ctx)       ──────────►    │
    ///         <── join ────────────────────────────────────────── ┘
    /// ```
    ///
    /// # Safety инварианты
    ///
    /// **Compile-time (compile()):**
    /// Системы в одном Stage не имеют Write-конфликтов по `AccessDescriptor`.
    /// Значит никакие два потока не пишут в один и тот же Column-буфер.
    ///
    /// **Runtime (SendPtr):**
    /// Каждый `SendPtr<SystemDescriptor>` создаётся из **уникального** индекса
    /// в `self.systems`. Два потока никогда не получают один и тот же ptr.
    ///
    /// **Lifetime (rayon::scope):**
    /// `scope` гарантирует join всех потоков до своего возврата.
    /// `ParallelWorld` (содержащий `*const World`) дропается после join —
    /// dangling pointer невозможен.
    #[cfg(feature = "parallel")]
    fn run_hybrid_parallel(&mut self, world: &mut World) {
        let plan = self.execution_plan.as_ref().unwrap();
        let stages: Vec<(Vec<SystemId>, bool)> = plan.stages
            .iter()
            .map(|s| (s.system_ids.clone(), s.all_parallel))
            .collect();

        for (stage_ids, all_parallel) in &stages {
            // ── Sequential или одиночная система ───────────────
            if !all_parallel || stage_ids.len() <= 1 {
                for &sys_id in stage_ids {
                    if let Some(system) = self.systems.iter_mut().find(|s| s.id == sys_id) {
                        match &mut system.kind {
                            SystemKind::Sequential(f)           => f(world),
                            SystemKind::Parallel { system, .. } => {
                                system.run(SystemContext::new(world));
                            }
                        }
                    }
                }
                continue;
            }

            // ── Параллельный Stage ─────────────────────────────
            //
            // Создаём ParallelWorld — это маркер, позволяющий передать
            // *const World в потоки Rayon. Он живёт строго внутри scope.
            //
            // SAFETY: все системы этого Stage прошли compile():
            //   - нет двух систем с Write-конфликтом
            //   - каждая Column — отдельный буфер памяти
            //   - archetypes.len() не изменяется (нет structural changes)
            let par_world: ParallelWorld<'_> = unsafe { world.as_parallel_world() };

            // Индексы систем в self.systems — гарантированно уникальные
            // (compile() строит их из множества уникальных SystemId)
            let indices: Vec<usize> = stage_ids
                .iter()
                .filter_map(|sid| self.systems.iter().position(|s| s.id == *sid))
                .collect();

            // SAFETY: indices уникальны → каждый SendPtr<SystemDescriptor>
            // указывает на отдельный элемент Vec → нет aliasing.
            let ptrs: Vec<SendPtr<SystemDescriptor>> = indices
                .iter()
                .map(|&idx| SendPtr(&mut self.systems[idx] as *mut SystemDescriptor))
                .collect();

            rayon::scope(|scope| {
                for ptr in &ptrs {
                    // Клонируем SendPtr (Copy семантика через unsafe)
                    // чтобы каждая closure владела своим экземпляром.
                    let sys_ptr = SendPtr(ptr.0);
                    let world_ref = &par_world;

                    scope.spawn(move |_| {
                        // SAFETY:
                        // - sys_ptr.0 уникален (см. выше)
                        // - par_world корректен (join гарантирован scope)
                        let descriptor = unsafe { sys_ptr.as_mut() };
                        if let SystemKind::Parallel { system, .. } = &mut descriptor.kind {
                            let world = unsafe { world_ref.get() };
                            system.run(SystemContext::new(world));
                        }
                    });
                }
            });
            // join: все потоки завершены, par_world дропается здесь
        }
    }

    // ── Инспекция ──────────────────────────────────────────────

    pub fn system_count(&self) -> usize { self.systems.len() }

    pub fn stages(&self) -> Option<&[Stage]> {
        self.execution_plan.as_ref().map(|p| p.stages.as_slice())
    }

    pub fn debug_plan(&self) -> String {
        let Some(plan) = &self.execution_plan else {
            return "(not compiled)".to_string();
        };
        let mut out = String::new();
        for (i, stage) in plan.stages.iter().enumerate() {
            let mode = if stage.is_parallelizable()  { "PARALLEL" }
                       else if stage.all_parallel     { "parallel/single" }
                       else                           { "sequential" };
            out.push_str(&format!("Stage {} [{}]:\n", i, mode));
            for sys_id in &stage.system_ids {
                if let Some(s) = self.systems.iter().find(|s| s.id == *sys_id) {
                    let kind_str = match &s.kind {
                        SystemKind::Parallel { access, .. } =>
                            format!("par | reads:{} writes:{}", access.reads.len(), access.writes.len()),
                        SystemKind::Sequential(_) =>
                            "seq | full &mut World".to_string(),
                    };
                    out.push_str(&format!("  - {} [{}]\n", s.name, kind_str));
                }
            }
        }
        out
    }
}

impl Default for Scheduler { fn default() -> Self { Self::new() } }

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::{prelude::*, world::World};

    #[derive(Clone, Copy)] struct Pos { x: f32, y: f32 }
    #[derive(Clone, Copy)] struct Vel { x: f32, y: f32 }
    #[derive(Clone, Copy)] struct Hp(f32);
    #[derive(Clone, Copy)] struct DeltaTime(f32);

    struct MovementSystem;
    impl ParSystem for MovementSystem {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Vel>().write::<Pos>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<(Read<Vel>, Write<Pos>), _>(|(vel, pos)| {
                pos.x += vel.x;
                pos.y += vel.y;
            });
        }
    }

    struct HealthSystem;
    impl ParSystem for HealthSystem {
        fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Hp>() }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.for_each_component::<Write<Hp>, _>(|hp| { hp.0 = hp.0.max(0.0); });
        }
    }

    #[test]
    fn sequential_ordering() {
        let mut sched = Scheduler::new();
        let log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> = Default::default();

        let log_a = log.clone();
        let a = sched.add_system("a", move |_| { log_a.lock().unwrap().push("a"); }).id();
        let log_b = log.clone();
        let b = sched.add_system("b", move |_| { log_b.lock().unwrap().push("b"); }).id();

        sched.add_dependency(b, a);
        sched.compile().unwrap();

        let mut world = World::new();
        sched.run_sequential(&mut world);
        assert_eq!(*log.lock().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn par_no_conflict_same_stage() {
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSystem);
        sched.add_par_system("health",   HealthSystem);
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1);
        assert!(stages[0].all_parallel);
        assert_eq!(stages[0].system_count(), 2);
    }

    #[test]
    fn par_write_conflict_separate_stages() {
        struct WriterA; impl ParSystem for WriterA {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB; impl ParSystem for WriterB {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("a", WriterA);
        sched.add_par_system("b", WriterB);
        sched.compile().unwrap();

        assert_eq!(sched.stages().unwrap().len(), 2);
    }

    #[test]
    fn sequential_breaks_parallel_groups() {
        let mut sched = Scheduler::new();
        sched.add_par_system("par_a", MovementSystem);
        sched.add_system("barrier", |_| {});
        sched.add_par_system("par_b", HealthSystem);
        sched.compile().unwrap();

        let stages = sched.stages().unwrap();
        assert!(stages.len() >= 3);
        assert!(stages.iter().any(|s| !s.all_parallel));
    }

    #[test]
    fn fn_par_system_works() {
        let mut sched = Scheduler::new();
        sched.add_fn_par_system(
            "movement",
            |ctx: SystemContext<'_>| {
                ctx.for_each_component::<(Read<Vel>, Write<Pos>), _>(|(vel, pos)| {
                    pos.x += vel.x;
                    pos.y += vel.y;
                });
            },
            AccessDescriptor::new().read::<Vel>().write::<Pos>(),
        );

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.spawn_bundle((Pos { x: 0.0, y: 0.0 }, Vel { x: 3.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each_component(|p| { result = (p.x, p.y); });

        assert!((result.0 - 3.0).abs() < 1e-6);
        assert!((result.1 - 4.0).abs() < 1e-6);
    }

    #[test]
    fn fn_par_system_with_resource() {
        let mut sched = Scheduler::new();
        sched.add_fn_par_system(
            "scaled_movement",
            |ctx: SystemContext<'_>| {
                let dt = ctx.resource::<DeltaTime>();
                ctx.for_each_component::<(Read<Vel>, Write<Pos>), _>(|(vel, pos)| {
                    pos.x += vel.x * (*dt).0;
                    pos.y += vel.y * (*dt).0;
                });
            },
            AccessDescriptor::new()
                .read::<DeltaTime>()
                .read::<Vel>()
                .write::<Pos>(),
        );

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.insert_resource(DeltaTime(0.5));
        world.spawn_bundle((Pos { x: 0.0, y: 0.0 }, Vel { x: 2.0, y: 4.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each_component(|p| { result = (p.x, p.y); });

        assert!((result.0 - 1.0).abs() < 1e-6);
        assert!((result.1 - 2.0).abs() < 1e-6);
    }

    #[test]
    fn par_system_runs_correctly() {
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSystem);

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.spawn_bundle((Pos { x: 0.0, y: 0.0 }, Vel { x: 1.0, y: 2.0 }));

        sched.run_sequential(&mut world);

        let mut result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each_component(|p| { result = (p.x, p.y); });

        assert!((result.0 - 1.0).abs() < 1e-6);
        assert!((result.1 - 2.0).abs() < 1e-6);
    }

    #[test]
    fn circular_dependency_detected() {
        let mut sched = Scheduler::new();
        let a = sched.add_system("a", |_| {}).id();
        let b = sched.add_system("b", |_| {}).id();
        sched.add_dependency(b, a);
        sched.add_dependency(a, b);
        assert!(sched.compile().is_err());
    }

    /// Параллельное выполнение: обе системы применяют свои изменения.
    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_stage_correctness() {
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSystem);
        sched.add_par_system("health",   HealthSystem);
        sched.compile().unwrap();
        assert!(sched.stages().unwrap()[0].is_parallelizable());

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.register_component::<Hp>();
        world.spawn_bundle((
            Pos { x: 0.0, y: 0.0 },
            Vel { x: 1.0, y: 2.0 },
            Hp(-5.0),
        ));

        sched.run(&mut world); // параллельный путь

        let mut pos_result = (0.0f32, 0.0f32);
        Query::<Read<Pos>>::new(&world).for_each_component(|p| { pos_result = (p.x, p.y); });
        assert!((pos_result.0 - 1.0).abs() < 1e-6, "movement not applied: {:?}", pos_result);
        assert!((pos_result.1 - 2.0).abs() < 1e-6, "movement not applied: {:?}", pos_result);

        let mut hp_result = -1.0f32;
        Query::<Read<Hp>>::new(&world).for_each_component(|hp| { hp_result = hp.0; });
        assert!((hp_result - 0.0).abs() < 1e-6, "health clamp not applied: {}", hp_result);
    }

    /// Множество entity — стресс-тест параллельного Stage.
    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_stage_many_entities() {
        const N: usize = 10_000;

        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSystem);
        sched.add_par_system("health",   HealthSystem);
        sched.compile().unwrap();

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.register_component::<Hp>();
        world.spawn_many_silent(N, |i| (
            Pos { x: i as f32, y: 0.0 },
            Vel { x: 1.0, y: 0.5 },
            Hp(if i % 3 == 0 { -1.0 } else { 50.0 }),
        ));

        sched.run(&mut world);

        // Все Pos должны быть сдвинуты на (1, 0.5)
        let mut count = 0usize;
        Query::<Read<Pos>>::new(&world).for_each_component(|p| {
            // x = i + 1.0, y = 0.5 — проверяем что y изменилось
            assert!(p.y > 0.0, "pos.y должен быть > 0 после movement");
            count += 1;
        });
        assert_eq!(count, N, "все entity должны быть обработаны");

        // Все Hp должны быть >= 0
        Query::<Read<Hp>>::new(&world).for_each_component(|hp| {
            assert!(hp.0 >= 0.0, "hp после clamp должен быть >= 0, got {}", hp.0);
        });
    }
}