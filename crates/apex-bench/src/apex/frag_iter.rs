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

impl FragIter {
    pub fn new() -> Self {
        let mut world = World::new();

        for i in 0..26 {
            for _ in 0..20 {
                let val = i as f32;
                match i {
                    0  => { world.spawn_bundle((A(val), Data(val))); },
                    1  => { world.spawn_bundle((B(val), Data(val))); },
                    2  => { world.spawn_bundle((C(val), Data(val))); },
                    3  => { world.spawn_bundle((D(val), Data(val))); },
                    4  => { world.spawn_bundle((E(val), Data(val))); },
                    5  => { world.spawn_bundle((F(val), Data(val))); },
                    6  => { world.spawn_bundle((G(val), Data(val))); },
                    7  => { world.spawn_bundle((H(val), Data(val))); },
                    8  => { world.spawn_bundle((I(val), Data(val))); },
                    9  => { world.spawn_bundle((J(val), Data(val))); },
                    10 => { world.spawn_bundle((K(val), Data(val))); },
                    11 => { world.spawn_bundle((L(val), Data(val))); },
                    12 => { world.spawn_bundle((M(val), Data(val))); },
                    13 => { world.spawn_bundle((N(val), Data(val))); },
                    14 => { world.spawn_bundle((O(val), Data(val))); },
                    15 => { world.spawn_bundle((P(val), Data(val))); },
                    16 => { world.spawn_bundle((Q(val), Data(val))); },
                    17 => { world.spawn_bundle((R(val), Data(val))); },
                    18 => { world.spawn_bundle((S(val), Data(val))); },
                    19 => { world.spawn_bundle((T(val), Data(val))); },
                    20 => { world.spawn_bundle((U(val), Data(val))); },
                    21 => { world.spawn_bundle((V(val), Data(val))); },
                    22 => { world.spawn_bundle((W(val), Data(val))); },
                    23 => { world.spawn_bundle((X(val), Data(val))); },
                    24 => { world.spawn_bundle((Y(val), Data(val))); },
                    25 => { world.spawn_bundle((Z(val), Data(val))); },
                    _ => unreachable!(),
                }
            }
        }

        Self { world }
    }

    pub fn run(&self) {
        self.world.query_typed::<Write<Data>>()
            .for_each_component(|data| {
                data.0 *= 2.0;
            });
    }
}