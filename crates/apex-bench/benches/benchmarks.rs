use criterion::*;
use apex_bench::*;

// ---------------------------------------------------------------------------
// simple_insert
// ---------------------------------------------------------------------------
fn bench_simple_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_insert");
    group.bench_function("apex", |b| {
        let mut bench = apex::simple_insert::SimpleInsert::new();
        b.iter(move || bench.run());
    });
    group.bench_function("legion", |b| {
        let mut bench = legion::simple_insert::Benchmark::new();
        b.iter(move || bench.run());
    });
    group.bench_function("bevy", |b| {
        let mut bench = bevy::simple_insert::Benchmark::new();
        b.iter(move || bench.run());
    });
}

// ---------------------------------------------------------------------------
// simple_iter
// ---------------------------------------------------------------------------
fn bench_simple_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_iter");
    group.bench_function("apex", |b| {
        let mut bench = apex::simple_iter::SimpleIter::new();
        b.iter(move || bench.run());
    });
    group.bench_function("legion", |b| {
        let mut bench = legion::simple_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
    group.bench_function("bevy", |b| {
        let mut bench = bevy::simple_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
}

// ---------------------------------------------------------------------------
// frag_iter
// ---------------------------------------------------------------------------
fn bench_frag_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("fragmented_iter");
    group.bench_function("apex", |b| {
        let mut bench = apex::frag_iter::FragIter::new();
        b.iter(move || bench.run());
    });
    group.bench_function("legion", |b| {
        let mut bench = legion::frag_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
    group.bench_function("bevy", |b| {
        let mut bench = bevy::frag_iter::Benchmark::new();
        b.iter(move || bench.run());
    });
}

// ---------------------------------------------------------------------------
// schedule
// ---------------------------------------------------------------------------
fn bench_schedule(c: &mut Criterion) {
    let mut group = c.benchmark_group("schedule");
    group.bench_function("apex", |b| {
        let mut bench = apex::schedule::Schedule::new();
        b.iter(move || bench.run());
    });
    group.bench_function("legion", |b| {
        let mut bench = legion::schedule::Benchmark::new();
        b.iter(move || bench.run());
    });
    group.bench_function("bevy", |b| {
        let mut bench = bevy::schedule::Benchmark::new();
        b.iter(move || bench.run());
    });
}

// ---------------------------------------------------------------------------
// heavy_compute
// ---------------------------------------------------------------------------
fn bench_heavy_compute(c: &mut Criterion) {
    let mut group = c.benchmark_group("heavy_compute");
    group.bench_function("apex", |b| {
        let mut bench = apex::heavy_compute::HeavyCompute::new();
        b.iter(move || bench.run());
    });
    group.bench_function("legion", |b| {
        let mut bench = legion::heavy_compute::Benchmark::new();
        b.iter(move || bench.run());
    });
    group.bench_function("bevy", |b| {
        let mut bench = bevy::heavy_compute::Benchmark::new();
        b.iter(move || bench.run());
    });
}

// ---------------------------------------------------------------------------
// add_remove
// ---------------------------------------------------------------------------
fn bench_add_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_remove_component");
    group.bench_function("apex", |b| {
        let mut bench = apex::add_remove::AddRemove::new();
        b.iter(move || bench.run());
    });
    group.bench_function("legion", |b| {
        let mut bench = legion::add_remove::Benchmark::new();
        b.iter(move || bench.run());
    });
    // Bevy 0.1 panics in this benchmark
    // group.bench_function("bevy", |b| {
    //     let mut bench = bevy::add_remove::Benchmark::new();
    //     b.iter(move || bench.run());
    // });
}

criterion_group!(
    benches,
    bench_simple_insert,
    bench_simple_iter,
    bench_frag_iter,
    bench_schedule,
    bench_heavy_compute,
    bench_add_remove,
);
criterion_main!(benches);
