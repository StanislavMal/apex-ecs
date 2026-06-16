use bevy_ecs::prelude::*;

#[derive(Component)]
struct A(f32);
#[derive(Component)]
struct B(f32);
#[derive(Component)]
struct C(f32);
#[derive(Component)]
struct D(f32);
#[derive(Component)]
struct E(f32);

pub struct Benchmark(
    World,
    QueryState<(
        &'static B,
        &'static C,
        &'static D,
        &'static E,
        &'static mut A,
    )>,
);

impl Benchmark {
    pub fn new() -> Self {
        let mut world = World::new();
        world.spawn_batch((0..10_000).map(|i| (A(0.0), B(i as f32), C(1.0), D(2.0), E(3.0))));
        let query = world.query::<(&B, &C, &D, &E, &mut A)>();
        Self(world, query)
    }

    pub fn run(&mut self) {
        self.1
            .iter_mut(&mut self.0)
            .for_each(|(b, c, d, e, mut a)| {
                a.0 += b.0 + c.0 + d.0 + e.0;
            });
    }
}
