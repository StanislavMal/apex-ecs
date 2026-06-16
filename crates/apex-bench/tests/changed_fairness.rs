//! Страж честности `changed_iter`: apex и bevy ОБЯЗАНЫ выдавать одинаковое число изменённых
//! сущностей каждый кадр (иначе бенч сравнивает разную работу). Прогоняем несколько кадров.

#[cfg(all(feature = "bevy"))]
#[test]
fn changed_iter_apex_and_bevy_yield_same_count() {
    use apex_bench::apex::changed_iter::ChangedIter;
    use apex_bench::bevy::changed_iter::Benchmark as BevyChanged;

    let mut a = ChangedIter::new();
    let mut b = BevyChanged::new();

    // Первый кадр — прогрев (начальный last_run у движков может отличаться).
    let _ = a.run();
    let _ = b.run();

    // Установившийся режим: оба должны видеть РОВНО 1000 изменённых (10% из 10k).
    for frame in 0..5 {
        let ca = a.run();
        let cb = b.run();
        assert_eq!(ca, 1000, "apex кадр {frame}: ожидалось 1000 changed, got {ca}");
        assert_eq!(cb, 1000, "bevy кадр {frame}: ожидалось 1000 changed, got {cb}");
    }
}
