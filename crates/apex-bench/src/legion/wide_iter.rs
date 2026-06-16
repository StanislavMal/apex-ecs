use legion::*;

#[derive(Copy, Clone)]
struct A(f32);
#[derive(Copy, Clone)]
struct B(f32);
#[derive(Copy, Clone)]
struct C(f32);
#[derive(Copy, Clone)]
struct D(f32);
#[derive(Copy, Clone)]
struct E(f32);

pub struct Benchmark(
    World,
    Query<(Read<B>, Read<C>, Read<D>, Read<E>, Write<A>)>,
);

impl Benchmark {
    pub fn new() -> Self {
        let mut world = World::default();
        world.extend((0..10_000).map(|i| (A(0.0), B(i as f32), C(1.0), D(2.0), E(3.0))));
        let query = <(Read<B>, Read<C>, Read<D>, Read<E>, Write<A>)>::query();
        Self(world, query)
    }

    pub fn run(&mut self) {
        self.1.for_each_mut(&mut self.0, |(b, c, d, e, a)| {
            a.0 += b.0 + c.0 + d.0 + e.0;
        });
    }
}
