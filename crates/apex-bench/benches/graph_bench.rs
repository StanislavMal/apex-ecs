use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use apex_graph::Graph;
use petgraph::graph::DiGraph;
use petgraph::algo::toposort;

fn bench_topological_sort(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_topological_sort");
    
    for size in [10, 100, 500, 1000, 5000] {
        // Benchmark apex-graph - создаём граф один раз, затем многократно тестируем алгоритм
        group.bench_with_input(BenchmarkId::new("apex_graph", size), &size, |b, &size| {
            let mut graph = Graph::<(), ()>::new();
            let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
            // Создаём цепочку: 0→1→2→...→size-1
            for i in 0..size-1 {
                graph.add_edge(nodes[i], nodes[i+1], ());
            }
            
            b.iter(|| {
                // Используем compute_topological_sort() чтобы избежать проблем с lifetime
                let result = graph.compute_topological_sort();
                criterion::black_box(result)
            })
        });
        
        // Benchmark petgraph для сравнения - аналогично создаём граф один раз
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
    
    // Бенчмарк добавления узлов
    group.bench_function("add_nodes", |b| {
        b.iter(|| {
            let mut graph = Graph::<usize, ()>::new();
            for i in 0..1000 {
                criterion::black_box(graph.add_node(i));
            }
        })
    });
    
    // Бенчмарк добавления рёбер
    group.bench_function("add_edges", |b| {
        b.iter(|| {
            let mut graph = Graph::<(), usize>::new();
            let nodes: Vec<_> = (0..100).map(|_| graph.add_node(())).collect();
            for i in 0..1000 {
                let from = nodes[i % 100];
                let to = nodes[(i + 1) % 100];
                criterion::black_box(graph.add_edge(from, to, i));
            }
        })
    });
    
    // Бенчмарк удаления рёбер
    group.bench_function("remove_edges", |b| {
        b.iter(|| {
            let mut graph = Graph::<(), usize>::new();
            let nodes: Vec<_> = (0..100).map(|_| graph.add_node(())).collect();
            let edges: Vec<_> = (0..1000)
                .map(|i| {
                    let from = nodes[i % 100];
                    let to = nodes[(i + 1) % 100];
                    graph.add_edge(from, to, i)
                })
                .collect();
            
            for &edge in &edges[0..500] {
                criterion::black_box(graph.remove_edge(edge));
            }
        })
    });
    
    // Бенчмарк навигационных запросов
    group.bench_function("predecessors_successors", |b| {
        let mut graph = Graph::<(), ()>::new();
        let nodes: Vec<_> = (0..100).map(|_| graph.add_node(())).collect();
        
        // Создаём полный граф направлений
        for i in 0..100 {
            for j in 0..100 {
                if i != j {
                    graph.add_edge(nodes[i], nodes[j], ());
                }
            }
        }
        
        b.iter(|| {
            for &node in &nodes[0..10] {
                let preds: Vec<_> = graph.predecessors(node).collect();
                let succs: Vec<_> = graph.successors(node).collect();
                criterion::black_box(preds);
                criterion::black_box(succs);
            }
        })
    });
    
    // Бенчмарк обновления веса рёбер
    group.bench_function("update_edge_weight", |b| {
        let mut graph = Graph::<(), usize>::new();
        let nodes: Vec<_> = (0..10).map(|_| graph.add_node(())).collect();
        let edges: Vec<_> = (0..50)
            .map(|i| {
                let from = nodes[i % 10];
                let to = nodes[(i + 1) % 10];
                graph.add_edge(from, to, i)
            })
            .collect();
        
        b.iter(|| {
            for (i, &edge) in edges.iter().enumerate() {
                let _ = graph.update_edge_weight(edge, i * 2);
            }
        })
    });
    
    group.finish();
}

// Бенчмарк кэширования топологической сортировки
fn bench_topological_sort_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_topological_sort_cache");
    
    for size in [100, 500, 1000] {
        group.bench_with_input(
            BenchmarkId::new("cold_cache", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let mut graph = Graph::<(), ()>::new();
                    let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
                    for i in 0..size-1 {
                        graph.add_edge(nodes[i], nodes[i+1], ());
                    }
                    
                    // Первый вызов - cold cache
                    let result = graph.compute_topological_sort();
                    criterion::black_box(result);
                })
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("hot_cache", size),
            &size,
            |b, &size| {
                let mut graph = Graph::<(), ()>::new();
                let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
                for i in 0..size-1 {
                    graph.add_edge(nodes[i], nodes[i+1], ());
                }
                
                // Прогрев кэша
                let _ = graph.topological_sort();
                
                b.iter(|| {
                    // Повторный вызов - hot cache (используем кэшированный результат)
                    let result = graph.topological_sort();
                    criterion::black_box(result);
                })
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("cache_invalidation", size),
            &size,
            |b, &size| {
                let mut graph = Graph::<(), ()>::new();
                let nodes: Vec<_> = (0..size).map(|_| graph.add_node(())).collect();
                for i in 0..size-1 {
                    graph.add_edge(nodes[i], nodes[i+1], ());
                }
                
                b.iter(|| {
                    // Добавляем новое ребро (инвалидирует кэш)
                    graph.add_edge(nodes[0], nodes[size-1], ());
                    // Удаляем ребро (тоже инвалидирует кэш)
                    let edge = graph.add_edge(nodes[1], nodes[size-2], ());
                    let _ = graph.remove_edge(edge);
                    
                    // Вызов после инвалидации кэша
                    let result = graph.compute_topological_sort();
                    criterion::black_box(result);
                })
            },
        );
    }
    
    group.finish();
}

// ECS-специфичные бенчмарки
fn bench_ecs_system_dependency(c: &mut Criterion) {
    let mut group = c.benchmark_group("ecs_system_dependency");
    
    // Моделирование графа зависимостей систем
    for system_count in [50, 200, 500] {
        group.bench_with_input(
            BenchmarkId::new("build_and_sort", system_count),
            &system_count,
            |b, &system_count| {
                b.iter(|| {
                    let mut graph = Graph::<usize, ()>::new(); // usize = system ID
                    let systems: Vec<_> = (0..system_count).map(|i| graph.add_node(i)).collect();
                    
                    // Создаём реалистичные зависимости: каждая система зависит от 2-5 предыдущих
                    for i in 0..system_count {
                        let deps = (i.saturating_sub(5)..i).filter(|&d| d != i);
                        for dep in deps {
                            graph.add_edge(systems[dep], systems[i], ());
                        }
                    }
                    
                    // Топологическая сортировка для планировщика
                    let schedule = graph.compute_topological_sort();
                    criterion::black_box(schedule);
                })
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("parallel_levels", system_count),
            &system_count,
            |b, &system_count| {
                let mut graph = Graph::<usize, ()>::new();
                let systems: Vec<_> = (0..system_count).map(|i| graph.add_node(i)).collect();
                
                for i in 0..system_count {
                    let deps = (i.saturating_sub(5)..i).filter(|&d| d != i);
                    for dep in deps {
                        graph.add_edge(systems[dep], systems[i], ());
                    }
                }
                
                b.iter(|| {
                    let levels = graph.parallel_levels();
                    criterion::black_box(levels);
                })
            },
        );
    }
    
    group.finish();
}

fn bench_ecs_entity_relationships(c: &mut Criterion) {
    let mut group = c.benchmark_group("ecs_entity_relationships");
    
    // Моделирование графа связей между сущностями
    for entity_count in [1000, 5000, 10000] {
        group.bench_with_input(
            BenchmarkId::new("relationship_updates", entity_count),
            &entity_count,
            |b, &entity_count| {
                let mut graph = Graph::<usize, usize>::new(); // usize = entity ID, weight = relationship type
                let entities: Vec<_> = (0..entity_count).map(|i| graph.add_node(i)).collect();
                
                // Начальные связи (дерево иерархии)
                let mut edges = Vec::new();
                for i in 1..entity_count {
                    let parent = entities[i / 2];
                    let child = entities[i];
                    edges.push(graph.add_edge(parent, child, 0)); // 0 = parent-child
                }
                
                b.iter(|| {
                    // Симуляция частых обновлений связей (каждый кадр)
                    for &edge in &edges[0..100] {
                        let _ = graph.update_edge_weight(edge, 1); // Изменение типа связи
                    }
                    
                    // Навигационные запросы (поиск всех детей корневой сущности)
                    let root = entities[0];
                    let children: Vec<_> = graph.successors(root).collect();
                    criterion::black_box(children);
                })
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("relationship_queries", entity_count),
            &entity_count,
            |b, &entity_count| {
                let mut graph = Graph::<usize, usize>::new();
                let entities: Vec<_> = (0..entity_count).map(|i| graph.add_node(i)).collect();
                
                // Создаём сложную сеть связей
                for i in 0..entity_count {
                    for j in (i + 1)..(i + 10).min(entity_count) {
                        graph.add_edge(entities[i], entities[j], i % 3); // разные типы связей
                    }
                }
                
                b.iter(|| {
                    // Запросы связей для случайных сущностей
                    for &entity in &entities[0..100] {
                        let incoming: Vec<_> = graph.predecessors(entity).collect();
                        let outgoing: Vec<_> = graph.successors(entity).collect();
                        criterion::black_box(incoming);
                        criterion::black_box(outgoing);
                    }
                })
            },
        );
    }
    
    group.finish();
}

fn bench_ecs_event_propagation(c: &mut Criterion) {
    let mut group = c.benchmark_group("ecs_event_propagation");
    
    // Моделирование DAG для распространения событий
    for listener_count in [100, 500, 1000] {
        group.bench_with_input(
            BenchmarkId::new("event_dag_traversal", listener_count),
            &listener_count,
            |b, &listener_count| {
                let mut graph = Graph::<usize, u32>::new(); // usize = listener ID, weight = event priority
                let listeners: Vec<_> = (0..listener_count).map(|i| graph.add_node(i)).collect();
                
                // Создаём DAG для распространения событий (каждый listener подписан на 2-3 предыдущих)
                for i in 0..listener_count {
                    let sources = (i.saturating_sub(3)..i).filter(|&s| s != i);
                    for source in sources {
                        graph.add_edge(listeners[source], listeners[i], (i % 5) as u32);
                    }
                }
                
                b.iter(|| {
                    // BFS для распространения события от корневого listener
                    let root = listeners[0];
                    let reachable = graph.bfs(root);
                    criterion::black_box(reachable);
                    
                    // DFS для альтернативного обхода
                    let dfs_order = graph.dfs(root);
                    criterion::black_box(dfs_order);
                })
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("dynamic_subscription", listener_count),
            &listener_count,
            |b, &listener_count| {
                let mut graph = Graph::<usize, u32>::new();
                let listeners: Vec<_> = (0..listener_count).map(|i| graph.add_node(i)).collect();
                
                // Начальная подписка
                let mut edges = Vec::new();
                for i in 1..listener_count {
                    let source = listeners[i - 1];
                    let target = listeners[i];
                    edges.push(graph.add_edge(source, target, 1));
                }
                
                b.iter(|| {
                    // Динамическая переподписка (симуляция изменения подписок)
                    for i in 0..10 {
                        if let Some(&edge) = edges.get(i) {
                            let _ = graph.remove_edge(edge);
                            let new_source = listeners[(i + 5) % listener_count];
                            let new_target = listeners[i];
                            let new_edge = graph.add_edge(new_source, new_target, 2);
                            edges[i] = new_edge;
                        }
                    }
                    
                    // Проверка на циклы после изменений
                    let has_cycle = graph.has_cycle();
                    criterion::black_box(has_cycle);
                })
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_topological_sort,
    bench_parallel_levels,
    bench_bfs_vs_dfs,
    bench_graph_operations,
    bench_topological_sort_cache,
    bench_ecs_system_dependency,
    bench_ecs_entity_relationships,
    bench_ecs_event_propagation
);

criterion_main!(benches);
