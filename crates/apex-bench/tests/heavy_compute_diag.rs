//! heavy_compute regression diagnostics: (1) matrices after spawn_many are healthy (from from_angle_x),
//! (2) par_for_each visits each entity EXACTLY once (does not duplicate work).

use apex_core::prelude::*;
use cgmath::{Matrix4, Rad, SquareMatrix, Vector3};
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn heavy_compute_matrices_healthy_and_visited_once() {
    let mut world = World::new();
    let entities = world.spawn_many(1000, |_| {
        (
            Matrix4::<f32>::from_angle_x(Rad(1.2)),
            apex_bench::Position(Vector3::unit_x()),
            apex_bench::Rotation(Vector3::unit_x()),
            apex_bench::Velocity(Vector3::unit_x()),
        )
    });
    assert_eq!(entities.len(), 1000);

    // (1a) VIA world.get (random-access, path did NOT change): row 0 (write_into_batch) vs
    // row 1/500/999 (bulk-copy) — isolates "data at spawn" from "for_each read".
    for &i in &[0usize, 1, 500, 999] {
        let det = world.get::<Matrix4<f32>>(entities[i]).unwrap().determinant();
        assert!(
            (det - 1.0).abs() < 1e-4,
            "get: matrix entity[{i}] corrupted at spawn: det={det} (≈1.0 expected)"
        );
    }

    // (1b) VIA for_each (the lazy-entity path that I changed).
    let mut count = 0u64;
    let mut bad = 0u64;
    world.query::<Read<Matrix4<f32>>>().for_each(|_, m| {
        count += 1;
        if (m.determinant() - 1.0).abs() >= 1e-4 {
            bad += 1;
        }
    });
    assert_eq!(count, 1000, "spawn_many did not create 1000 entities with Matrix4");
    assert_eq!(bad, 0, "for_each: {bad} matrices read corrupted (read path)");

    // (2) par_for_each visits each entity EXACTLY once.
    let visits = AtomicU64::new(0);
    world
        .query::<Read<Matrix4<f32>>>()
        .par_for_each(|_, _| {
            visits.fetch_add(1, Ordering::Relaxed);
        });
    assert_eq!(
        visits.load(Ordering::Relaxed),
        1000,
        "par_for_each did not visit 1000 times — chunk duplication/overlap!"
    );

    // (3) PARALLELISM: par_for_each with a real workload (100 inversions) must be faster than serial.
    use std::time::Instant;
    let heavy = |m0: Matrix4<f32>| {
        let mut m = m0;
        for _ in 0..100 {
            m = m.invert().unwrap_or(m);
        }
        m.determinant()
    };
    let sink = AtomicU64::new(0);

    let t = Instant::now();
    world.query_mut::<(Read<Matrix4<f32>>, Write<apex_bench::Position>)>()
        .for_each_mut(|_, (m, _p)| { sink.fetch_add(heavy(*m).to_bits() as u64, Ordering::Relaxed); });
    let seq = t.elapsed();

    let t = Instant::now();
    world.query_mut::<(Read<Matrix4<f32>>, Write<apex_bench::Position>)>()
        .par_for_each_mut(|_, (m, _p)| { sink.fetch_add(heavy(*m).to_bits() as u64, Ordering::Relaxed); });
    let par = t.elapsed();

    eprintln!(
        "rayon threads={} | seq={:?} | par={:?} | speedup={:.2}x | sink={}",
        rayon::current_num_threads(),
        seq, par,
        seq.as_secs_f64() / par.as_secs_f64().max(1e-9),
        sink.load(Ordering::Relaxed)
    );
    assert!(
        par < seq,
        "par_for_each NOT faster than serial (seq={seq:?} par={par:?}) — parallelism lost!"
    );
}
