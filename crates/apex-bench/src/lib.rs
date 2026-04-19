//! Apex ECS benchmarks for ecs_bench_suite
//! 
//! Implements the standard benchmark traits from ecs_bench_suite
//! for comparing Apex ECS with other Rust ECS implementations.

pub mod simple_insert;
pub mod simple_iter;
pub mod fragmented_iter;
pub mod schedule;
pub mod add_remove;
pub mod relations_bench;
pub mod commands_bench;

// Common components used in benchmarks
#[derive(Clone, Copy)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy)]
pub struct Mass(pub f32);

#[derive(Clone, Copy)]
pub struct Renderable {
    pub mesh: u32,
    pub material: u32,
}

#[derive(Clone, Copy)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

// Re-export implementations
pub use simple_insert::Benchmark as SimpleInsertBenchmark;
pub use simple_iter::Benchmark as SimpleIterBenchmark;
pub use fragmented_iter::Benchmark as FragmentedIterBenchmark;
pub use schedule::Benchmark as ScheduleBenchmark;
pub use add_remove::Benchmark as AddRemoveBenchmark;
pub use relations_bench::Benchmark as RelationsBenchmark;
pub use commands_bench::Benchmark as CommandsBenchmark;
