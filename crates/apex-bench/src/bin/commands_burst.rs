//! W2-1: grouping insert bursts in Commands::apply.
//!
//! A/B in one binary: "burst" (3 inserts in a row on an entity — grouped,
//! 1 archetype move) against "interleaved" (the same commands, but interleaved by
//! entity — the group does not coalesce, 3 moves per entity, as before W2-1).
//!
//! Run: `cargo run --release -p apex-bench --bin commands_burst`

use apex_core::prelude::*;
use std::time::Instant;

#[derive(Clone, Copy)]
struct Pos(#[allow(dead_code)] f32);
impl ComponentTrait for Pos {}
#[derive(Clone, Copy)]
struct Vel(#[allow(dead_code)] f32);
impl ComponentTrait for Vel {}
#[derive(Clone, Copy)]
struct Acc(#[allow(dead_code)] f32);
impl ComponentTrait for Acc {}
#[derive(Clone, Copy)]
struct Hp(#[allow(dead_code)] f32);
impl ComponentTrait for Hp {}

const N: usize = 10_000;
const SAMPLES: usize = 9;

fn median<F: FnMut() -> f64>(mut f: F) -> f64 {
    let mut v: Vec<f64> = (0..SAMPLES).map(|_| f()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[SAMPLES / 2]
}

fn main() {
    // Measure ONLY apply: the enqueue cost (arena/queue) is identical in
    // both scenarios and would blur the difference.

    // Burst: insert(e, Vel); insert(e, Acc); insert(e, Hp) in a row.
    let burst = median(|| {
        let mut world = World::new();
        let entities = world.spawn_many(N, |_| (Pos(0.0),));
        let mut cmds = Commands::new();
        for &e in &entities {
            cmds.insert(e, Vel(1.0));
            cmds.insert(e, Acc(2.0));
            cmds.insert(e, Hp(3.0));
        }
        let t = Instant::now();
        cmds.apply(&mut world);
        t.elapsed().as_secs_f64() * 1e3
    });
    println!("apply burst (grouped, 1 move/entity):     {burst:.2} ms for {N}×3 insert");

    // Interleaved: the same commands, but one component per pass — a group of
    // a single command does not coalesce, the path is as before W2-1 (3 moves per entity).
    let interleaved = median(|| {
        let mut world = World::new();
        let entities = world.spawn_many(N, |_| (Pos(0.0),));
        let mut cmds = Commands::new();
        for &e in &entities {
            cmds.insert(e, Vel(1.0));
        }
        for &e in &entities {
            cmds.insert(e, Acc(2.0));
        }
        for &e in &entities {
            cmds.insert(e, Hp(3.0));
        }
        let t = Instant::now();
        cmds.apply(&mut world);
        t.elapsed().as_secs_f64() * 1e3
    });
    println!("apply interleaved (no group, 3 move/entity): {interleaved:.2} ms");
    println!("burst speedup: {:.2}×", interleaved / burst);
}
