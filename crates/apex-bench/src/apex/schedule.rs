use apex_core::prelude::*;
use apex_scheduler::{Scheduler, AutoSystem, SystemContext};

// ---------------------------------------------------------------------------
// Components for schedule benchmarks
// ---------------------------------------------------------------------------
pub struct A(pub f32);
pub struct B(pub f32);
pub struct C(pub f32);
pub struct D(pub f32);
pub struct E(pub f32);

// ---------------------------------------------------------------------------
// AutoSystem implementations
// ---------------------------------------------------------------------------
struct SysAB;
struct SysCD;
struct SysCE;

impl AutoSystem for SysAB {
    type Query = (Write<A>, Write<B>);
    fn run(&mut self, ctx: SystemContext<'_>) {
        ctx.query::<Self::Query>().for_each_component(|(a, b)| {
            std::mem::swap(&mut a.0, &mut b.0);
        });
    }
}

impl AutoSystem for SysCD {
    type Query = (Write<C>, Write<D>);
    fn run(&mut self, ctx: SystemContext<'_>) {
        ctx.query::<Self::Query>().for_each_component(|(c, d)| {
            std::mem::swap(&mut c.0, &mut d.0);
        });
    }
}

impl AutoSystem for SysCE {
    type Query = (Write<C>, Write<E>);
    fn run(&mut self, ctx: SystemContext<'_>) {
        ctx.query::<Self::Query>().for_each_component(|(c, e)| {
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

impl Schedule {
    pub fn new() -> Self {
        let mut world = World::new();
        world.register_component::<A>();
        world.register_component::<B>();
        world.register_component::<C>();
        world.register_component::<D>();
        world.register_component::<E>();

        // Group 1: A+B (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0)));
        // Group 2: A+B+C (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0), C(0.0)));
        // Group 3: A+B+C+D (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0), C(0.0), D(0.0)));
        // Group 4: A+B+C+E (10K)
        world.spawn_many(10_000, |_| (A(0.0), B(0.0), C(0.0), E(0.0)));

        let mut scheduler = Scheduler::new();
        scheduler.add_auto_system("SysAB", SysAB);
        scheduler.add_auto_system("SysCD", SysCD);
        scheduler.add_auto_system("SysCE", SysCE);
        scheduler.compile().unwrap();

        Self { world, scheduler }
    }

    pub fn run(&mut self) {
        self.scheduler.run(&mut self.world);
    }
}
