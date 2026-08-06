use apex_core::prelude::*;
use apex_core::component::Tick;
use apex_core::query::Changed;
use apex_macros::Component;

// ChangedIter — a realistic "process only what changed": each frame 10% of entities are mutated,
// the system iterates `Changed<Data>`. Measures the SCAN of change-ticks (10k) + collecting the
// changed ones (1k). Not covered by other benches; a key pattern of reactive systems. Returns the
// NUMBER of changed entities (for honesty verification: apex and bevy must both yield the same ~1000).
#[derive(Component, Clone, Copy)]
pub struct Data(pub f32);

pub struct ChangedIter {
    world: World,
    entities: Vec<Entity>,
    last_run: Tick,
    // Persistent QueryState (the idiomatic path = counterpart of bevy's stored QueryState) — the
    // archetype match is cached, not rebuilt every frame.
    state: QueryState<(Changed<Data>, &'static Data)>,
}

impl Default for ChangedIter {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangedIter {
    pub fn new() -> Self {
        let mut world = World::new();
        let entities = world.spawn_many(10_000, |_| (Data(0.0),));
        world.advance_change_tick();
        let last_run = world.current_tick();
        Self {
            world,
            entities,
            last_run,
            state: QueryState::new(),
        }
    }

    pub fn run(&mut self) -> u32 {
        self.world.advance_change_tick();
        let now = self.world.current_tick();
        // Mutate the first 10% — stamps the current tick (> last_run).
        for &e in &self.entities[..1000] {
            if let Some(mut d) = self.world.get_mut::<Data>(e) {
                d.0 += 1.0;
            }
        }
        // Iterate Changed relative to the previous frame — scan 10k ticks, collect ~1000.
        let mut count = 0u32;
        self.state
            .query_with_tick(&self.world, self.last_run)
            .for_each(|_, _| count += 1);
        self.last_run = now;
        count
    }
}

// ChangedIterStatic — the PE-C2 target case: NOTHING changed this frame, the query must
// discover that as cheaply as possible. Today this is a linear scan of 10k row ticks per
// consumer per frame; with the per-column tick aggregate it collapses to O(archetypes).
pub struct ChangedIterStatic {
    world: World,
    last_run: Tick,
    state: QueryState<(Changed<Data>, &'static Data)>,
}

impl Default for ChangedIterStatic {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangedIterStatic {
    pub fn new() -> Self {
        let mut world = World::new();
        world.spawn_many_silent(10_000, |_| (Data(0.0),));
        world.advance_change_tick();
        let last_run = world.current_tick();
        Self {
            world,
            last_run,
            state: QueryState::new(),
        }
    }

    pub fn run(&mut self) -> u32 {
        self.world.advance_change_tick();
        let now = self.world.current_tick();
        let mut count = 0u32;
        self.state
            .query_with_tick(&self.world, self.last_run)
            .for_each(|_, _| count += 1);
        self.last_run = now;
        count // always 0 — verified by the fairness test
    }
}

macro_rules! declare_markers {
    ($($name:ident),*) => {
        $(
            #[derive(Component)]
            pub struct $name;
        )*
    }
}

declare_markers!(
    M00, M01, M02, M03, M04, M05, M06, M07, M08, M09, M10, M11, M12,
    M13, M14, M15, M16, M17, M18, M19, M20, M21, M22, M23, M24, M25
);

// ChangedIterFrag — 26 archetypes × 400 rows, only ONE archetype mutates (10% of its
// rows). Models a live scene: a handful of animated archetypes amid static ones. The
// aggregate skip should cut the scan to the one hot archetype.
pub struct ChangedIterFrag {
    world: World,
    hot: Vec<Entity>,
    last_run: Tick,
    state: QueryState<(Changed<Data>, &'static Data)>,
}

impl Default for ChangedIterFrag {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangedIterFrag {
    pub fn new() -> Self {
        let mut world = World::new();
        macro_rules! spawn_batches {
            ($world:ident; $( $name:ident ),*) => {
                $(
                    $world.spawn_batch((0..400).map(|_| ($name, Data(0.0))));
                )*
            };
        }
        // The FIRST batch is the hot archetype; the other 25 stay static.
        let hot = world.spawn_batch((0..400).map(|_| (M00, Data(0.0))));
        spawn_batches!(world; M01, M02, M03, M04, M05, M06, M07, M08, M09, M10, M11, M12,
                              M13, M14, M15, M16, M17, M18, M19, M20, M21, M22, M23, M24, M25);
        world.advance_change_tick();
        let last_run = world.current_tick();
        Self {
            world,
            hot,
            last_run,
            state: QueryState::new(),
        }
    }

    pub fn run(&mut self) -> u32 {
        self.world.advance_change_tick();
        let now = self.world.current_tick();
        for &e in &self.hot[..40] {
            if let Some(mut d) = self.world.get_mut::<Data>(e) {
                d.0 += 1.0;
            }
        }
        let mut count = 0u32;
        self.state
            .query_with_tick(&self.world, self.last_run)
            .for_each(|_, _| count += 1);
        self.last_run = now;
        count // always 40
    }
}
