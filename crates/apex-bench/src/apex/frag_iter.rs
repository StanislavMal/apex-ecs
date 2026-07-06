use apex_core::prelude::*;
use apex_macros::Component;

macro_rules! declare_markers {
    ($($name:ident),*) => {
        $(
            #[derive(Component)]
            pub struct $name(pub f32);
        )*
    }
}

declare_markers!(
    A, B, C, D, E, F, G, H, I, J, K, L, M,
    N, O, P, Q, R, S, T, U, V, W, X, Y, Z
);

#[derive(Component)]
pub struct Data(pub f32);

// FragIter — iteration over 26 archetypes × 20 entities with fragmented access.
// Persistent QueryState (the idiomatic apex path = analog of bevy QueryState) — the archetype
// match is computed once, not on every call.
pub struct FragIter {
    world: World,
    state: QueryState<Write<Data>>,
}

macro_rules! spawn_batches {
    ($world:ident; $( $name:ident ),*) => {
        $(
            $world.spawn_batch((0..20).map(|_| ($name(0.0), Data(1.0))));
        )*
    };
}

impl Default for FragIter {
    fn default() -> Self {
        Self::new()
    }
}

impl FragIter {
    pub fn new() -> Self {
        let mut world = World::new();

        spawn_batches!(world; A, B, C, D, E, F, G, H, I, J, K, L, M,
                            N, O, P, Q, R, S, T, U, V, W, X, Y, Z);

        Self { world, state: QueryState::new() }
    }

    pub fn run(&mut self) {
        self.state.query_mut(&mut self.world).for_each_mut(|_, mut data| {
            data.0 *= 2.0;
        });
    }
}
