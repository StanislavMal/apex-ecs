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

// Static counterpart of apex `ChangedIterStatic`: nothing changed, the query proves it.
pub struct BenchmarkStatic {
    world: World,
    query: QueryState<&'static Data, Changed<Data>>,
}

impl BenchmarkStatic {
    pub fn new() -> Self {
        let mut world = World::new();
        world.spawn_batch((0..10_000).map(|_| (Data(0.0),)));
        let query = world.query_filtered::<&Data, Changed<Data>>();
        // Swallow the spawn-changed ticks so every measured frame starts clean.
        world.clear_trackers();
        Self { world, query }
    }

    pub fn run(&mut self) -> u32 {
        self.world.increment_change_tick();
        let mut count = 0u32;
        for _ in self.query.iter(&self.world) {
            count += 1;
        }
        self.world.clear_trackers();
        count // always 0
    }
}

macro_rules! declare_markers {
    ($($name:ident),*) => {
        $(
            #[derive(Component)]
            struct $name;
        )*
    }
}

declare_markers!(
    M00, M01, M02, M03, M04, M05, M06, M07, M08, M09, M10, M11, M12,
    M13, M14, M15, M16, M17, M18, M19, M20, M21, M22, M23, M24, M25
);

// Fragmented counterpart of apex `ChangedIterFrag`: 26 archetypes × 400 rows, one hot.
pub struct BenchmarkFrag {
    world: World,
    hot: Vec<Entity>,
    query: QueryState<&'static Data, Changed<Data>>,
}

impl BenchmarkFrag {
    pub fn new() -> Self {
        let mut world = World::new();
        macro_rules! spawn_batches {
            ($world:ident; $( $name:ident ),*) => {
                $(
                    $world.spawn_batch((0..400).map(|_| ($name, Data(0.0)))).for_each(drop);
                )*
            };
        }
        let hot = world
            .spawn_batch((0..400).map(|_| (M00, Data(0.0))))
            .collect::<Vec<_>>();
        spawn_batches!(world; M01, M02, M03, M04, M05, M06, M07, M08, M09, M10, M11, M12,
                              M13, M14, M15, M16, M17, M18, M19, M20, M21, M22, M23, M24, M25);
        let query = world.query_filtered::<&Data, Changed<Data>>();
        world.clear_trackers();
        Self { world, hot, query }
    }

    pub fn run(&mut self) -> u32 {
        self.world.increment_change_tick();
        for &e in &self.hot[..40] {
            if let Some(mut d) = self.world.get_mut::<Data>(e) {
                d.0 += 1.0;
            }
        }
        let mut count = 0u32;
        for _ in self.query.iter(&self.world) {
            count += 1;
        }
        self.world.clear_trackers();
        count // always 40
    }
}
