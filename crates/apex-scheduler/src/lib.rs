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
//! fn my_system(ctx: SystemContext<'_>) {
//!     ctx.query::<Write<Pos>>().for_each_component(|pos| { pos.x += 1.0; });
//! }
//! sched.add_fn_par_system("my", my_system,
//!     AccessDescriptor::new().write::<Pos>()
//! );
//! ```

pub mod stage;

use rustc_hash::FxHashMap;
use thiserror::Error;
use apex_graph::Graph;
use thunderdome::Index;
use apex_core::{
    AccessDescriptor,
    world::World,
    query::{WorldQuery, Query},
    component::Tick,
    entity::Entity,
    system_param::{Res, ResMut, EventReader, EventWriter},
};

pub use stage::Stage;
pub use apex_core::AccessDescriptor as Access;

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
/// Предоставляет типизированный доступ через `query()`, `resource()` и т.д.
/// Планировщик гарантирует что в одном Stage нет Write-конфликтов.
pub struct SystemContext<'w> {
    world:   *const World,
    _marker: std::marker::PhantomData<&'w World>,
}

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
                .expect("resource_mut: resource not found. Did you call insert_resource()?");
            ResMut::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        // Используем публичный метод world.events() вместо приватного поля
        EventReader(unsafe { (*self.world).events::<T>() })
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        // Используем публичный метод world.event_queue_ptr() — добавлен в world.rs
        unsafe {
            let ptr = (*self.world)
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered. Did you call add_event()?");
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
    /// Декларация доступа — используется планировщиком.
    fn access() -> AccessDescriptor where Self: Sized;

    /// Выполнить систему.
    fn run(&mut self, ctx: SystemContext<'_>);

    /// Имя для отладки.
    fn name() -> &'static str where Self: Sized {
        std::any::type_name::<Self>()
    }
}

// ── FnParSystem — обёртка для функций ─────────────────────────

/// Адаптер для использования `fn(SystemContext<'_>)` как ParSystem.
struct FnParSystem {
    func:   Box<dyn FnMut(SystemContext<'_>) + Send + Sync>,
    access: AccessDescriptor,
    name:   &'static str,
}

impl ParSystem for FnParSystem {
    fn access() -> AccessDescriptor where Self: Sized {
        AccessDescriptor::new() // не используется — access хранится в поле
    }

    fn run(&mut self, ctx: SystemContext<'_>) {
        (self.func)(ctx);
    }
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

    /// Добавить Sequential систему с полным `&mut World`.
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

    /// Добавить ParSystem (struct с impl ParSystem).
    pub fn add_par_system<S: ParSystem + 'static>(
        &mut self,
        name: impl Into<String>,
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

    /// Добавить функцию-систему с явным AccessDescriptor.
    ///
    /// # Пример
    /// ```ignore
    /// fn movement(ctx: SystemContext<'_>) {
    ///     let dt = ctx.resource::<DeltaTime>().0;
    ///     ctx.for_each_component::<(Read<Velocity>, Write<Position>), _>(|(vel, pos)| {
    ///         pos.x += vel.x * dt;
    ///     });
    /// }
    ///
    /// sched.add_fn_par_system("movement", movement,
    ///     AccessDescriptor::new().read::<DeltaTime>().write::<Position>()
    /// );
    /// ```
    pub fn add_fn_par_system<F>(
        &mut self,
        name:   impl Into<String>,
        func:   F,
        access: AccessDescriptor,
    ) -> SystemId
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let id          = SystemId(self.next_id);
        self.next_id   += 1;
        let name_str    = name.into();
        let system      = FnParSystem {
            func:   Box::new(func),
            access: access.clone(),
            name:   std::any::type_name::<F>(),
        };
        self.systems.push(SystemDescriptor {
            id,
            name: name_str,
            kind: SystemKind::Parallel { system: Box::new(system), access },
            after:  Vec::new(),
            before: Vec::new(),
        });
        self.execution_plan = None;
        id
    }

    /// Добавить явную зависимость: `system` выполняется после `after_id`.
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

        // 2. Sequential барьеры — разрывают параллельные группы
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

        // 3. Write/Read конфликты между ParSystem
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

    pub fn run(&mut self, world: &mut World) {
        if self.execution_plan.is_none() {
            self.compile().expect("Failed to compile schedule");
        }

        #[cfg(feature = "parallel")]
        self.run_hybrid_parallel(world);

        #[cfg(not(feature = "parallel"))]
        self.run_sequential(world);
    }

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
                        let ctx = SystemContext::new(world);
                        system.run(ctx);
                    }
                }
            }
        }
    }

    #[cfg(feature = "parallel")]
    fn run_hybrid_parallel(&mut self, world: &mut World) {
        let plan = self.execution_plan.as_ref().unwrap();
        let stages: Vec<(Vec<SystemId>, bool)> = plan.stages
            .iter()
            .map(|s| (s.system_ids.clone(), s.all_parallel))
            .collect();

        for (stage_ids, all_parallel) in &stages {
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
            } else {
                // Parallel Stage → rayon::scope
                // SAFETY: системы не имеют Write-конфликтов (инвариант compile())
                use rayon::prelude::*;
                let world_ptr = world as *mut World;
                let indices: Vec<usize> = stage_ids.iter()
                    .filter_map(|sid| self.systems.iter().position(|s| s.id == *sid))
                    .collect();

                rayon::scope(|s| {
                    for idx in &indices {
                        let sys_ptr:   *mut SystemDescriptor = &mut self.systems[*idx];
                        let world_ref: &World                = unsafe { &*world_ptr };
                        s.spawn(move |_| {
                            let descriptor = unsafe { &mut *sys_ptr };
                            if let SystemKind::Parallel { system, .. } = &mut descriptor.kind {
                                system.run(SystemContext::new(world_ref));
                            }
                        });
                    }
                });
            }
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

    // ── ParSystem реализации ───────────────────────────────────

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

    // ── Тесты ─────────────────────────────────────────────────

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

        assert!((result.0 - 1.0).abs() < 1e-6); // 2.0 * 0.5
        assert!((result.1 - 2.0).abs() < 1e-6); // 4.0 * 0.5
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
}