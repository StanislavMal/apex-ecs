use legion::*;

#[derive(Copy, Clone)]
struct A(f32);

#[derive(Copy, Clone)]
struct B(f32);

pub fn setup() -> (World, Vec<Entity>) {
    let mut world = World::default();
    let entities = world
        .extend((0..10_000).map(|_| (A(0.0), B(0.0))))
        .to_vec();
    (world, entities)
}

pub fn run((mut world, entities): (World, Vec<Entity>)) {
    for e in entities {
        world.remove(e);
    }
}
