use criterion::*;
use apex_bench::*;

fn bench_simple_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_insert");
    group.bench_function("apex", |b| {
        let mut bench = apex::simple_insert::SimpleInsert::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "flecs")]
    group.bench_function("flecs", |b| {
        let mut bench = flecs::FlecsSimpleInsert::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let mut bench = legion::simple_insert::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::simple_insert::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_simple_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_iter");
    group.bench_function("apex", |b| {
        let mut bench = apex::simple_iter::SimpleIter::new();
        b.iter(move || bench.run());
    });
    // W2-0.5: плотная chunk-итерация (графа «скорость Legion + тики Bevy»)
    group.bench_function("apex_chunked", |b| {
        let mut bench = apex::simple_iter::SimpleIter::new();
        b.iter(move || bench.run_chunked());
    });
    #[cfg(feature = "flecs")]
    group.bench_function("flecs", |b| {
        let bench = flecs::FlecsSimpleIter::new();
        b.iter(|| bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let mut bench = legion::simple_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::simple_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_frag_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("fragmented_iter");
    group.bench_function("apex", |b| {
        let mut bench = apex::frag_iter::FragIter::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "flecs")]
    group.bench_function("flecs", |b| {
        let bench = flecs::FlecsFragIter::new();
        b.iter(|| bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let mut bench = legion::frag_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::frag_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_schedule(c: &mut Criterion) {
    let mut group = c.benchmark_group("schedule");
    group.bench_function("apex", |b| {
        let mut bench = apex::schedule::Schedule::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let mut bench = legion::schedule::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::schedule::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_heavy_compute(c: &mut Criterion) {
    let mut group = c.benchmark_group("heavy_compute");
    group.bench_function("apex", |b| {
        let mut bench = apex::heavy_compute::HeavyCompute::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "flecs")]
    group.bench_function("flecs", |b| {
        let bench = flecs::FlecsHeavyCompute::new();
        b.iter(|| bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let mut bench = legion::heavy_compute::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::heavy_compute::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_add_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_remove_component");
    group.bench_function("apex", |b| {
        let mut bench = apex::add_remove::AddRemove::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "flecs")]
    group.bench_function("flecs", |b| {
        let mut bench = flecs::FlecsAddRemove::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let mut bench = legion::add_remove::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::add_remove::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_commands_spawn(c: &mut Criterion) {
    let mut group = c.benchmark_group("commands_spawn");
    group.bench_function("apex", |b| {
        let mut bench = apex::commands_spawn::CommandsSpawn::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::commands_spawn::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_despawn(c: &mut Criterion) {
    let mut group = c.benchmark_group("despawn");
    // iter_batched: setup (населить мир) — вне измерения; измеряем только despawn.
    group.bench_function("apex", |b| {
        b.iter_batched(
            apex::despawn::setup,
            apex::despawn::run,
            BatchSize::SmallInput,
        );
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        b.iter_batched(
            legion::despawn::setup,
            legion::despawn::run,
            BatchSize::SmallInput,
        );
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        b.iter_batched(
            bevy::despawn::setup,
            bevy::despawn::run,
            BatchSize::SmallInput,
        );
    });
}

fn bench_get_component(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_component");
    group.bench_function("apex", |b| {
        let bench = apex::get_component::GetComponent::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let bench = legion::get_component::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let bench = bevy::get_component::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_changed_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("changed_iter");
    group.bench_function("apex", |b| {
        let mut bench = apex::changed_iter::ChangedIter::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::changed_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_events(c: &mut Criterion) {
    let mut group = c.benchmark_group("events");
    group.bench_function("apex", |b| {
        let mut bench = apex::events::EventsBench::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::events::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_relations(c: &mut Criterion) {
    let mut group = c.benchmark_group("relations");
    group.bench_function("apex", |b| {
        let mut bench = apex::relations::Relations::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::relations::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_commands_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("commands_insert");
    group.bench_function("apex", |b| {
        b.iter_batched(
            apex::commands_insert::setup,
            apex::commands_insert::run,
            BatchSize::SmallInput,
        );
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        b.iter_batched(
            bevy::commands_insert::setup,
            bevy::commands_insert::run,
            BatchSize::SmallInput,
        );
    });
}

fn bench_despawn_recursive(c: &mut Criterion) {
    let mut group = c.benchmark_group("despawn_recursive");
    group.bench_function("apex", |b| {
        b.iter_batched(
            apex::despawn_recursive::setup,
            apex::despawn_recursive::run,
            BatchSize::SmallInput,
        );
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        b.iter_batched(
            bevy::despawn_recursive::setup,
            bevy::despawn_recursive::run,
            BatchSize::SmallInput,
        );
    });
}

fn bench_wide_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("wide_iter");
    group.bench_function("apex", |b| {
        let mut bench = apex::wide_iter::WideIter::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "legion")]
    group.bench_function("legion", |b| {
        let mut bench = legion::wide_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
    #[cfg(feature = "bevy")]
    group.bench_function("bevy", |b| {
        let mut bench = bevy::wide_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
}

fn bench_propagate(c: &mut Criterion) {
    // Apex-фокус (наш дифференциатор; bevy propagate — отдельный crate/schedule). Регресс-страж.
    let mut group = c.benchmark_group("propagate");
    group.bench_function("apex", |b| {
        let mut bench = apex::propagate::Propagate::new();
        b.iter(move || bench.run());
    });
}

// Wave-5 §7: perf-guard for the adaptive-split par_for_each on skewed and
// uniform archetype distributions (the split-vs-fixed A/B is archived in the plan).
fn bench_par_split(c: &mut Criterion) {
    let mut group = c.benchmark_group("par_split");
    group.bench_function("skew", |b| {
        let mut bench = apex::par_skew::ParSkew::new();
        b.iter(|| bench.run());
    });
    group.bench_function("uniform", |b| {
        let mut bench = apex::par_skew::ParSkew::new_uniform();
        b.iter(|| bench.run());
    });
}

criterion_group!(
    benches,
    bench_par_split,
    bench_simple_insert,
    bench_simple_iter,
    bench_frag_iter,
    bench_schedule,
    bench_heavy_compute,
    bench_add_remove,
    bench_commands_spawn,
    bench_despawn,
    bench_get_component,
    bench_changed_iter,
    bench_events,
    bench_relations,
    bench_propagate,
    bench_despawn_recursive,
    bench_wide_iter,
    bench_commands_insert,
);
criterion_main!(benches);
