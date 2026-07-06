use bevy_ecs::prelude::*;

#[derive(Component)]
struct Data(f32);

pub struct Benchmark {
    world: World,
    entities: Vec<Entity>,
    query: QueryState<&'static Data, Changed<Data>>,
}

impl Benchmark {
    pub fn new() -> Self {
        let mut world = World::new();
        let entities = world
            .spawn_batch((0..10_000).map(|_| (Data(0.0),)))
            .collect::<Vec<_>>();
        let query = world.query_filtered::<&Data, Changed<Data>>();
        Self {
            world,
            entities,
            query,
        }
    }

    pub fn run(&mut self) -> u32 {
        // Advance the change-tick so that this frame's mutations are newer than `last_change_tick`.
        self.world.increment_change_tick();
        for &e in &self.entities[..1000] {
            if let Some(mut d) = self.world.get_mut::<Data>(e) {
                d.0 += 1.0;
            }
        }
        let mut count = 0u32;
        for _ in self.query.iter(&self.world) {
            count += 1;
        }
        // Frame boundary: `clear_trackers` advances `last_change_tick` to the current one ⇒ on the
        // NEXT frame `Changed` filters exactly the next frame's mutations (not all spawn-changed).
        self.world.clear_trackers();
        count
    }
}
