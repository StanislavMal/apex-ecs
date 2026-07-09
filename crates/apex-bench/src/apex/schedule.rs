use apex_core::prelude::*;
use apex_macros::Component;
use apex_scheduler::Scheduler;

// ---------------------------------------------------------------------------
// Components for schedule benchmarks
// ---------------------------------------------------------------------------
#[derive(Component, Clone, Copy)]
pub struct A(pub f32);
#[derive(Component, Clone, Copy)]
pub struct B(pub f32);
#[derive(Component, Clone, Copy)]
pub struct C(pub f32);
#[derive(Component, Clone, Copy)]
pub struct D(pub f32);
#[derive(Component, Clone, Copy)]
pub struct E(pub f32);

// ---------------------------------------------------------------------------
// AutoSystem implementations (system! macro)
// ---------------------------------------------------------------------------

system! {
    fn sys_ab(
        q: (&mut A, &mut B),
    ) {
        q.for_each_mut(|_, (mut a, mut b)| {
            std::mem::swap(&mut a.0, &mut b.0);
        });
    }
}

system! {
    fn sys_cd(
        q: (&mut C, &mut D),
    ) {
        q.for_each_mut(|_, (mut c, mut d)| {
            std::mem::swap(&mut c.0, &mut d.0);
        });
    }
}

system! {
    fn sys_ce(
        q: (&mut C, &mut E),
    ) {
        q.for_each_mut(|_, (mut c, mut e)| {
            std::mem::swap(&mut c.0, &mut e.0);
        });
    }
}

// ---------------------------------------------------------------------------
// Benchmark
// ---------------------------------------------------------------------------
pub struct Schedule {
    world: World,
    scheduler: Scheduler,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new()
    }
}

impl Schedule {
    pub fn new() -> Self {
        let mut world = World::new();
        // Group 1: A+B (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0)));
        // Group 2: A+B+C (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0), C(0.0)));
        // Group 3: A+B+C+D (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0), C(0.0), D(0.0)));
        // Group 4: A+B+C+E (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0), C(0.0), E(0.0)));

        let mut scheduler = Scheduler::new();
        scheduler.add_systems(
            apex_scheduler::StageLabel::Update,
            (
                apex_scheduler::sys("SysAB", sys_ab),
                apex_scheduler::sys("SysCD", sys_cd),
                apex_scheduler::sys("SysCE", sys_ce),
            ),
        );
        scheduler.compile().unwrap();

        Self { world, scheduler }
    }

    pub fn run(&mut self) {
        self.scheduler.run(&mut self.world);
    }
}
