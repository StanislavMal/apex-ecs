use crate::Position;
use apex_core::prelude::*;
use cgmath::{Matrix4, Rad, SquareMatrix, Transform as _, Vector3};

// Wave-5 A/B harness (§7): fixed-chunk `par_for_each` vs adaptive-split
// `par_for_each_split` on a SKEWED archetype-size distribution. Six marker
// types give six archetypes of very different sizes, all matched by the query
// (they share Matrix4 + Position). Per-element work (matrix inversions) is heavy
// enough that scheduling/imbalance shows up over rayon overhead. If split does
// not beat fixed here reproducibly, it is not restored (campaign rule).
struct S0;
impl apex_core::component::Component for S0 {}
struct S1;
impl apex_core::component::Component for S1 {}
struct S2;
impl apex_core::component::Component for S2 {}
struct S3;
impl apex_core::component::Component for S3 {}
struct S4;
impl apex_core::component::Component for S4 {}

const INVERSIONS: usize = 40;

pub struct ParSkew {
    world: World,
}

impl Default for ParSkew {
    fn default() -> Self {
        Self::new()
    }
}

impl ParSkew {
    pub fn new() -> Self {
        let mut world = World::new();
        let seed = || {
            (
                Matrix4::<f32>::from_angle_x(Rad(1.2)),
                Position(Vector3::unit_x()),
            )
        };
        // One dominant archetype + a long tail of increasingly small ones —
        // total ~7000 rows spread very unevenly across 6 archetypes.
        world.spawn_many(5000, move |_| seed());
        for _ in 0..1500 {
            world.spawn((seed().0, seed().1, S0));
        }
        for _ in 0..400 {
            world.spawn((seed().0, seed().1, S1));
        }
        for _ in 0..80 {
            world.spawn((seed().0, seed().1, S2));
        }
        for _ in 0..18 {
            world.spawn((seed().0, seed().1, S3));
        }
        for _ in 0..5 {
            world.spawn((seed().0, seed().1, S4));
        }
        Self { world }
    }

    #[inline]
    fn heavy(mat: &Matrix4<f32>, pos: &mut Position) {
        let mut m = *mat;
        for _ in 0..INVERSIONS {
            m = m.invert().unwrap_or(m);
        }
        pos.0 = m.transform_vector(pos.0);
    }

    /// UNIFORM control: all rows in a single archetype — where split's recursion
    /// (temp allocations) is pure overhead and must NOT regress vs fixed.
    pub fn new_uniform() -> Self {
        let mut world = World::new();
        world.spawn_many(7000, |_| {
            (
                Matrix4::<f32>::from_angle_x(Rad(1.2)),
                Position(Vector3::unit_x()),
            )
        });
        Self { world }
    }

    /// Run `par_for_each` (adaptive-split since wave 5). Perf-guard for the
    /// split path — the wave-5 A/B that justified split (fixed vs split, ~7%
    /// uniform / ~2-3% skew) is archived in the plan; this monitors regressions.
    pub fn run(&mut self) {
        self.world
            .query_mut::<(Read<Matrix4<f32>>, Write<Position>)>()
            .par_for_each(|_, (mat, mut pos)| Self::heavy(mat, &mut pos));
    }
}
