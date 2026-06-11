//! W2-1: группировка insert-бёрстов в Commands::apply.
//!
//! A/B в одном бинаре: «бёрст» (3 insert'а подряд на entity — группируется,
//! 1 archetype move) против «interleaved» (те же команды, но вперемешку по
//! entity — группа не складывается, 3 move'а на entity, как до W2-1).
//!
//! Запуск: `cargo run --release -p apex-bench --bin commands_burst`

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
    // Меряем ТОЛЬКО apply: стоимость enqueue (арена/очередь) одинакова в
    // обоих сценариях и размывала бы разницу.

    // Бёрст: insert(e, Vel); insert(e, Acc); insert(e, Hp) подряд.
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
    println!("apply бёрст (группируется, 1 move/entity):     {burst:.2} ms на {N}×3 insert");

    // Interleaved: те же команды, но по компоненту за проход — группа из
    // одной команды не складывается, путь как до W2-1 (3 move на entity).
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
    println!("apply interleaved (без группы, 3 move/entity): {interleaved:.2} ms");
    println!("выигрыш бёрста: {:.2}×", interleaved / burst);
}
