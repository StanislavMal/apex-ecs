//! Диагностика регресса heavy_compute: (1) матрицы после spawn_many здоровы (от from_angle_x),
//! (2) par_for_each посещает каждую сущность РОВНО раз (не дублирует работу).

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

    // (1a) ЧЕРЕЗ world.get (random-access, путь НЕ менялся): row 0 (write_into_batch) vs
    // row 1/500/999 (bulk-copy) — изолирует «данные при спавне» от «чтение for_each».
    for &i in &[0usize, 1, 500, 999] {
        let det = world.get::<Matrix4<f32>>(entities[i]).unwrap().determinant();
        assert!(
            (det - 1.0).abs() < 1e-4,
            "get: матрица entity[{i}] повреждена при spawn: det={det} (≈1.0 ожид.)"
        );
    }

    // (1b) ЧЕРЕЗ for_each (путь lazy-entity, который я менял).
    let mut count = 0u64;
    let mut bad = 0u64;
    world.query::<Read<Matrix4<f32>>>().for_each(|_, m| {
        count += 1;
        if (m.determinant() - 1.0).abs() >= 1e-4 {
            bad += 1;
        }
    });
    assert_eq!(count, 1000, "spawn_many создал не 1000 сущностей с Matrix4");
    assert_eq!(bad, 0, "for_each: {bad} матриц прочитаны повреждёнными (read-путь)");

    // (2) par_for_each посещает каждую сущность РОВНО раз.
    let visits = AtomicU64::new(0);
    world
        .query::<Read<Matrix4<f32>>>()
        .par_for_each(|_, _| {
            visits.fetch_add(1, Ordering::Relaxed);
        });
    assert_eq!(
        visits.load(Ordering::Relaxed),
        1000,
        "par_for_each посетил не 1000 раз — дублирование/перекрытие чанков!"
    );

    // (3) ПАРАЛЛЕЛИЗМ: par_for_each с реальной нагрузкой (100 инверсий) должен быть быстрее serial.
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
    world.query::<(Read<Matrix4<f32>>, Write<apex_bench::Position>)>()
        .for_each(|_, (m, _p)| { sink.fetch_add(heavy(*m).to_bits() as u64, Ordering::Relaxed); });
    let seq = t.elapsed();

    let t = Instant::now();
    world.query::<(Read<Matrix4<f32>>, Write<apex_bench::Position>)>()
        .par_for_each(|_, (m, _p)| { sink.fetch_add(heavy(*m).to_bits() as u64, Ordering::Relaxed); });
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
        "par_for_each НЕ быстрее serial (seq={seq:?} par={par:?}) — параллелизм потерян!"
    );
}
