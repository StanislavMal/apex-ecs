pub mod stage;

use std::any::TypeId;
use rustc_hash::FxHashMap;
use thiserror::Error;
use apex_graph::Graph;
use thunderdome::Index;
use apex_core::{
    world::World,
    query::{WorldQuery, Query},
    component::Tick,
    entity::Entity,
};

pub use stage::Stage;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("Circular dependency between systems")]
    CircularDependency,
    #[error("System '{0}' not found")]
    SystemNotFound(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SystemId(u32);

pub type SystemFn = Box<dyn FnMut(&mut World) + Send>;

// ── AccessDescriptor ───────────────────────────────────────────

#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    pub reads:  Vec<TypeId>,
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

    pub fn conflicts_with(&self, other: &AccessDescriptor) -> bool {
        for w in &self.writes {
            if other.reads.contains(w) || other.writes.contains(w) { return true; }
        }
        for w in &other.writes {
            if self.reads.contains(w) || self.writes.contains(w) { return true; }
        }
        false
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.writes.is_empty()
    }
}

// ── SystemContext ──────────────────────────────────────────────

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
    pub fn resource<T: Send + Sync + 'static>(&self) -> &T {
        unsafe { (*self.world).resource::<T>() }
    }

    /// Мутабельный доступ к ресурсу для параллельных систем.
    ///
    /// Использует `ResourceMap::get_raw_ptr` — метод определён
    /// в `apex-core` в том же крейте что и `ResourceMap`.
    /// Никаких нарушений orphan rules.
    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> &mut T {
        unsafe {
            &mut *(*self.world)
                .resources
                .get_raw_ptr::<T>()
                .expect("resource not found — did you insert_resource()?")
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
}

// ── ParSystem trait ────────────────────────────────────────────

pub trait ParSystem: Send + Sync {
    fn access() -> AccessDescriptor where Self: Sized;
    fn run(&mut self, ctx: SystemContext<'_>);
    fn name() -> &'static str where Self: Sized {
        std::any::type_name::<Self>()
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
    fn is_parallel(&self) -> bool {
        matches!(self, SystemKind::Parallel { .. })
    }

    fn access(&self) -> Option<&AccessDescriptor> {
        match self {
            SystemKind::Parallel { access, .. } => Some(access),
            SystemKind::Sequential(_) => None,
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
    id: SystemId,
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
        name: impl Into<String>,
        system: S,
    ) -> SystemId {
        let id = SystemId(self.next_id);
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

        // 2. Sequential барьеры
        let n = self.systems.len();
        for i in 0..n {
            if !self.systems[i].kind.is_parallel() {
                for j in 0..i {
                    if let (Some(&from), Some(&to)) =
                        (nodes.get(&self.systems[j].id), nodes.get(&self.systems[i].id))
                    {
                        graph.add_edge(from, to, ());
                    }
                }
                for j in (i + 1)..n {
                    if let (Some(&from), Some(&to)) =
                        (nodes.get(&self.systems[i].id), nodes.get(&self.systems[j].id))
                    {
                        graph.add_edge(from, to, ());
                    }
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
                    {
                        graph.add_edge(from, to, ());
                    }
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
                    SystemKind::Sequential(f) => f(world),
                    SystemKind::Parallel { system, .. } => {
                        let ctx = SystemContext::new(world);
                        system.run(ctx);
                    }
                }
            }
        }
    }

    #[cfg(feature = "parallel")]
    fn run_hybrid_parallel(&mut self, world: &mut World) {
        use rayon::prelude::*;

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
                            SystemKind::Sequential(f) => f(world),
                            SystemKind::Parallel { system, .. } => {
                                let ctx = SystemContext::new(world);
                                system.run(ctx);
                            }
                        }
                    }
                }
            } else {
                // Parallel Stage → rayon::scope
                // SAFETY: системы не имеют Write-конфликтов (инвариант compile())
                let world_ptr = world as *mut World;
                let indices: Vec<usize> = stage_ids.iter()
                    .filter_map(|sid| self.systems.iter().position(|s| s.id == *sid))
                    .collect();

                rayon::scope(|s| {
                    for idx in &indices {
                        let system_ptr = &mut self.systems[*idx] as *mut SystemDescriptor;
                        let world_ref: &World = unsafe { &*world_ptr };
                        s.spawn(move |_| {
                            let descriptor = unsafe { &mut *system_ptr };
                            if let SystemKind::Parallel { system, .. } = &mut descriptor.kind {
                                let ctx = SystemContext::new(world_ref);
                                system.run(ctx);
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
            let mode = if stage.is_parallelizable() { "PARALLEL" }
                       else if stage.all_parallel   { "parallel/single" }
                       else                          { "sequential" };
            out.push_str(&format!("Stage {} [{}]:\n", i, mode));
            for sys_id in &stage.system_ids {
                if let Some(s) = self.systems.iter().find(|s| s.id == *sys_id) {
                    let kind_str = if s.kind.is_parallel() {
                        let acc = s.kind.access().unwrap();
                        format!("par | reads:{} writes:{}", acc.reads.len(), acc.writes.len())
                    } else {
                        "seq | full &mut World".to_string()
                    };
                    out.push_str(&format!("  - {} [{}]\n", s.name, kind_str));
                }
            }
        }
        out
    }
}

impl Default for Scheduler {
    fn default() -> Self { Self::new() }
}

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::{prelude::*, world::World};

    #[derive(Clone, Copy)] struct Pos { x: f32, y: f32 }
    #[derive(Clone, Copy)] struct Vel { x: f32, y: f32 }
    #[derive(Clone, Copy)] struct Health(f32);

    struct MovementSystem;
    impl ParSystem for MovementSystem {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().read::<Vel>().write::<Pos>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<(Read<Vel>, Write<Pos>)>()
               .for_each_component(|(vel, pos)| { pos.x += vel.x; pos.y += vel.y; });
        }
    }

    struct HealthSystem;
    impl ParSystem for HealthSystem {
        fn access() -> AccessDescriptor {
            AccessDescriptor::new().write::<Health>()
        }
        fn run(&mut self, ctx: SystemContext<'_>) {
            ctx.query::<Write<Health>>()
               .for_each_component(|hp| { hp.0 = hp.0.max(0.0); });
        }
    }

    #[test]
    fn sequential_explicit_ordering() {
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
    fn par_systems_no_conflict_same_stage() {
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSystem);
        sched.add_par_system("health",   HealthSystem);
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 1, "non-conflicting par systems → 1 stage");
        assert!(stages[0].all_parallel);
        assert_eq!(stages[0].system_count(), 2);
    }

    #[test]
    fn par_write_conflict_different_stages() {
        struct WriterA;
        impl ParSystem for WriterA {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }
        struct WriterB;
        impl ParSystem for WriterB {
            fn access() -> AccessDescriptor { AccessDescriptor::new().write::<Pos>() }
            fn run(&mut self, _: SystemContext<'_>) {}
        }

        let mut sched = Scheduler::new();
        sched.add_par_system("writer_a", WriterA);
        sched.add_par_system("writer_b", WriterB);
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert_eq!(stages.len(), 2, "write conflict → 2 stages");
    }

    #[test]
    fn sequential_breaks_parallel_groups() {
        let mut sched = Scheduler::new();
        sched.add_par_system("par_a", MovementSystem);
        sched.add_system("seq_barrier", |_| {});
        sched.add_par_system("par_b", HealthSystem);
        sched.compile().unwrap();
        let stages = sched.stages().unwrap();
        assert!(stages.len() >= 3, "sequential breaks parallel groups");
        assert!(stages.iter().any(|s| !s.all_parallel), "should have sequential stage");
    }

    #[test]
    fn par_system_runs_correctly() {
        let mut sched = Scheduler::new();
        sched.add_par_system("movement", MovementSystem);

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.spawn_bundle((Pos { x: 0.0, y: 0.0 }, Vel { x: 1.0, y: 2.0 }));
        world.spawn_bundle((Pos { x: 5.0, y: 5.0 }, Vel { x: -1.0, y: 0.0 }));

        sched.run_sequential(&mut world);

        let mut positions: Vec<(f32, f32)> = Vec::new();
        Query::<Read<Pos>>::new(&world)
            .for_each_component(|p| positions.push((p.x, p.y)));
        positions.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        assert!((positions[0].0 - 1.0).abs() < 1e-6);
        assert!((positions[0].1 - 2.0).abs() < 1e-6);
        assert!((positions[1].0 - 4.0).abs() < 1e-6);
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

    #[test]
    fn mixed_seq_par_pipeline() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut sched = Scheduler::new();

        sched.add_par_system("movement", MovementSystem);
        let log2 = log.clone();
        sched.add_system("commands", move |_| {
            log2.lock().unwrap().push("commands".to_string());
        });
        sched.add_par_system("health", HealthSystem);
        sched.compile().unwrap();

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_component::<Vel>();
        world.register_component::<Health>();
        world.spawn_bundle((Pos { x: 0.0, y: 0.0 }, Vel { x: 1.0, y: 0.0 }));
        world.spawn_bundle((Health(100.0),));

        sched.run_sequential(&mut world);
        assert!(log.lock().unwrap().contains(&"commands".to_string()));
    }
}