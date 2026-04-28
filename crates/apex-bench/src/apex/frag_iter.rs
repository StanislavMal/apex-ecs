use apex_core::prelude::*;

macro_rules! declare_markers {
    ($($name:ident),*) => {
        $(
            pub struct $name(pub f32);
        )*
    }
}

declare_markers!(
    A, B, C, D, E, F, G, H, I, J, K, L, M,
    N, O, P, Q, R, S, T, U, V, W, X, Y, Z
);

pub struct Data(pub f32);

// FragIter — итерация по 26 архетипам × 20 сущностей с фрагментированным доступом
// World хранится как owned, query_typed() создаётся на каждой итерации
pub struct FragIter {
    world: World,
}

macro_rules! spawn_batches {
    ($world:ident; $( $name:ident ),*) => {
        $(
            $world.spawn_batch((0..20).map(|_| ($name(0.0), Data(1.0))));
        )*
    };
}

impl FragIter {
    pub fn new() -> Self {
        let mut world = World::new();

        spawn_batches!(world; A, B, C, D, E, F, G, H, I, J, K, L, M,
                            N, O, P, Q, R, S, T, U, V, W, X, Y, Z);

        Self { world }
    }

    pub fn run(&self) {
        self.world.query_typed::<Write<Data>>()
            .for_each(|_, data| {
                data.0 *= 2.0;
            });
    }
}