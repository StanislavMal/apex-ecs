use criterion::{criterion_group, criterion_main, Criterion};
use apex_bench::*;

fn relations_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("relations");
    
    group.bench_function("apex", |b| {
        let mut benchmark = RelationsBenchmark::new();
        b.iter(|| benchmark.run());
    });
    
    group.finish();
}

fn commands_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("commands");
    
    group.bench_function("apex", |b| {
        let mut benchmark = CommandsBenchmark::new();
        b.iter(|| benchmark.run());
    });
    
    group.finish();
}

fn specialized_report(_c: &mut Criterion) {
    println!("\n=== Специализированные бенчмарки Apex ECS ===");
    println!("Эти тесты проверяют сильные стороны, не учтённые в стандартных бенчмарках:");
    
    println!("\n📊 1. Отношения (Relations):");
    println!("   - Иерархии объектов (ChildOf)");
    println!("   - Графы владения (Owns)");
    println!("   - Traversal отношений");
    println!("   - Массовое добавление/удаление");
    
    println!("\n📊 2. Команды (Commands):");
    println!("   - Отложенный спавн сущностей");
    println!("   - Команды во время итерации");
    println!("   - Batch операции");
    println!("   - Транзакционные изменения");
    
    println!("\n🎯 Ожидаемые преимущества Apex ECS:");
    println!("   - Нативные отношения vs эмуляция через компоненты");
    println!("   - Командный буфер для безопасных мутаций");
    println!("   - Эффективный batch спавн");
    println!("   - Автоматическое кэширование запросов");
    
    println!("\n⚠️  Примечание:");
    println!("   Эти тесты не имеют прямых аналогов в ecs_bench_suite");
    println!("   Сравнение с другими ECS требует реализации аналогичного функционала");
}

criterion_group!(
    benches,
    relations_benchmark,
    commands_benchmark,
    specialized_report
);
criterion_main!(benches);