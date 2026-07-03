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

// FragIter — итерация по 26 архетипам × 20 сущностей с фрагментированным доступом.
// Персистентный QueryState (идиоматичный путь apex = аналог bevy QueryState) — матч
// архетипов вычисляется один раз, не каждый вызов.
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
        self.state.query_mut(&mut self.world).for_each(|_, mut data| {
            data.0 *= 2.0;
        });
    }
}
