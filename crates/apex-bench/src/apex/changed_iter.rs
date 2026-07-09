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
