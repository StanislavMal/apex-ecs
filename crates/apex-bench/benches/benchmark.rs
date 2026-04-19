use criterion::{criterion_group, criterion_main, Criterion};
use apex_bench::*;

fn simple_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_insert");
    
    group.bench_function("apex", |b| {
        let mut benchmark = SimpleInsertBenchmark::new();
        b.iter(|| benchmark.run());
    });
    
    group.finish();
}

fn simple_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_iter");
    
    group.bench_function("apex", |b| {
        let mut benchmark = SimpleIterBenchmark::new();
        b.iter(|| benchmark.run());
    });
    
    group.finish();
}

fn fragmented_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("fragmented_iter");
    
    group.bench_function("apex", |b| {
        let mut benchmark = FragmentedIterBenchmark::new();
        b.iter(|| benchmark.run());
    });
    
    group.finish();
}

fn schedule(c: &mut Criterion) {
    let mut group = c.benchmark_group("schedule");
    
    group.bench_function("apex", |b| {
        let mut benchmark = ScheduleBenchmark::new();
        b.iter(|| benchmark.run());
    });
    
    group.finish();
}

fn add_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_remove");
    
    group.bench_function("apex", |b| {
        let mut benchmark = AddRemoveBenchmark::new();
        b.iter(|| benchmark.run());
    });
    
    group.finish();
}

criterion_group!(
    benches,
    simple_insert,
    simple_iter,
    fragmented_iter,
    schedule,
    add_remove
);
criterion_main!(benches);
