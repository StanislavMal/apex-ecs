//! Стражи честности для `events` и `relations`: apex и bevy ОБЯЗАНЫ выполнять одинаковую работу
//! (прочитать все 10k событий ⇒ одинаковая сумма; увидеть всех 10k детей).

#[cfg(feature = "bevy")]
#[test]
fn events_apex_and_bevy_read_all_10k() {
    use apex_bench::apex::events::EventsBench;
    use apex_bench::bevy::events::Benchmark as BevyEvents;

    // sum(0..10000) = 10000*9999/2 = 49_995_000.
    const EXPECTED: u64 = 49_995_000;
    assert_eq!(EventsBench::new().run(), EXPECTED, "apex прочитал не все события");
    assert_eq!(BevyEvents::new().run(), EXPECTED, "bevy прочитал не все события");
}

#[cfg(feature = "bevy")]
#[test]
fn relations_apex_and_bevy_see_all_10k_children() {
    use apex_bench::apex::relations::Relations;
    use apex_bench::bevy::relations::Benchmark as BevyRelations;

    assert_eq!(Relations::new().run(), 10_000, "apex увидел не всех детей");
    assert_eq!(BevyRelations::new().run(), 10_000, "bevy увидел не всех детей");
}
