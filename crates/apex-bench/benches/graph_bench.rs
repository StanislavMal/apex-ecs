use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use apex_graph::Graph;
use petgraph::graph::DiGraph;
use petgraph::algo::toposort;

fn bench_topological_sort(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_topological_sort");
    
    for size in [10, 100, 500, 1000] {
        // Benchmark apex-graph
        group.bench_with_input(BenchmarkId::new("apex_graph", size), &size, |b, &size| {
            let mut graph = Graph::<(), ()>::new();
            let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
            // Создаём цепочку: 0→1→2→...→size-1
            for i in 0..size-1 {
                graph.add_edge(nodes[i], nodes[i+1], ());
            }
            b.iter(|| {
                // Используем compute_topological_sort чтобы избежать проблем с borrow checker
                let result = graph.compute_topological_sort();
                criterion::black_box(result)
            })
        });
        
        // Benchmark petgraph для сравнения
        group.bench_with_input(BenchmarkId::new("petgraph", size), &size, |b, &size| {
            let mut graph = DiGraph::<(), ()>::new();
            let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
            for i in 0..size-1 {
                graph.add_edge(nodes[i], nodes[i+1], ());
            }
            b.iter(|| toposort(&graph, None))
        });
    }
    
    group.finish();
}

fn bench_parallel_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_parallel_levels");
    
    for size in [10, 50, 100, 200] {
        group.bench_with_input(BenchmarkId::new("apex_graph", size), &size, |b, &size| {
            let mut graph = Graph::<(), ()>::new();
            let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
            
            // Создаём граф с несколькими уровнями параллелизма
            // Уровень 0: 0,1,2
            // Уровень 1: 3,4,5 (зависят от 0,1,2)
            // Уровень 2: 6,7,8 (зависят от 3,4,5)
            for i in 0..size/3 {
                let from = i * 3;
                let to = (i + 1) * 3;
                if to < size {
                    for f in from..from+3 {
                        for t in to..to+3 {
                            if f < size && t < size {
                                graph.add_edge(nodes[f], nodes[t], ());
                            }
                        }
                    }
                }
            }
            
            b.iter(|| {
                let result = graph.parallel_levels();
                criterion::black_box(result)
            })
        });
    }
    
    group.finish();
}

fn bench_bfs_vs_dfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_traversal");
    
    for size in [100, 500, 1000] {
        group.bench_with_input(BenchmarkId::new("bfs", size), &size, |b, &size| {
            let mut graph = Graph::<(), ()>::new();
            let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
            
            // Создаём дерево с ветвлением 2
            for i in 0..size/2 {
                let left = i * 2 + 1;
                let right = i * 2 + 2;
                if left < size {
                    graph.add_edge(nodes[i], nodes[left], ());
                }
                if right < size {
                    graph.add_edge(nodes[i], nodes[right], ());
                }
            }
            
            b.iter(|| graph.bfs(nodes[0]))
        });
        
        group.bench_with_input(BenchmarkId::new("dfs", size), &size, |b, &size| {
            let mut graph = Graph::<(), ()>::new();
            let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
            
            for i in 0..size/2 {
                let left = i * 2 + 1;
                let right = i * 2 + 2;
                if left < size {
                    graph.add_edge(nodes[i], nodes[left], ());
                }
                if right < size {
                    graph.add_edge(nodes[i], nodes[right], ());
                }
            }
            
            b.iter(|| graph.dfs(nodes[0]))
        });
    }
    
    group.finish();
}

fn bench_graph_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_operations");
    
    group.bench_function("add_remove_nodes_edges", |b| {
        b.iter(|| {
            let mut graph = Graph::<usize, usize>::new();
            let mut nodes = Vec::new();
            
            // Добавляем 1000 узлов
            for i in 0..1000 {
                nodes.push(graph.add_node(i));
            }
            
            // Добавляем 2000 рёбер
            for i in 0..2000 {
                let from = i % 1000;
                let to = (i + 1) % 1000;
                graph.add_edge(nodes[from], nodes[to], i);
            }
            
            // Удаляем половину рёбер (симуляция)
            // Удаление не реализовано в текущем apex-graph, поэтому просто итерируем
            graph.edge_count();
            graph.node_count();
        })
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_topological_sort,
    bench_parallel_levels,
    bench_bfs_vs_dfs,
    bench_graph_operations
);

criterion_main!(benches);