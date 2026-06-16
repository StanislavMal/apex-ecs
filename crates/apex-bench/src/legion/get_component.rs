use legion::*;

#[derive(Copy, Clone)]
struct A(f32);

#[derive(Copy, Clone)]
struct B(f32);

pub struct Benchmark {
    world: World,
    entities: Vec<Entity>,
}

impl Benchmark {
    pub fn new() -> Self {
        let mut world = World::default();
        let entities = world
            .extend((0..10_000).map(|i| (A(i as f32), B(0.0))))
            .to_vec();
        Self { world, entities }
    }

    pub fn run(&self) -> f32 {
        let mut sum = 0.0f32;
        for &e in &self.entities {
            sum += self
                .world
                .entry_ref(e)
                .unwrap()
                .get_component::<A>()
                .unwrap()
                .0;
        }
        sum
    }
}
