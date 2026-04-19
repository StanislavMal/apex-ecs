use std::collections::VecDeque;
use rustc_hash::FxHashSet;
use thunderdome::Index;

use crate::{Graph, GraphError};

impl<N, E> Graph<N, E> {
    /// Топологическая сортировка (алгоритм Кана) с кэшированием
    /// Возвращает узлы в порядке зависимостей
    /// Используется для порядка выполнения систем
    pub fn topological_sort(&mut self) -> Result<&[Index], GraphError> {
        // Проверяем кэш
        if self.dirty || self.cached_topological.is_none() {
            // Вычисляем заново
            let result = self.compute_topological_sort()?;
            self.cached_topological = Some(result);
            self.dirty = false;
        }
        
        Ok(self.cached_topological.as_ref().unwrap())
    }
    
    /// Внутренняя реализация топологической сортировки (без кэширования)
    /// Публичная для бенчмарков и тестов
    pub fn compute_topological_sort(&self) -> Result<Vec<Index>, GraphError> {
        // Используем Vec вместо HashMap так как индексы плотные
        let node_count = self.nodes.len();
        let mut in_degree: Vec<usize> = vec![0; node_count];
        
        // Заполняем in_degree: для каждого ребра увеличиваем степень целевого узла
        for (_, edge) in self.edges.iter() {
            let to_slot = edge.to.slot() as usize;
            if to_slot < node_count {
                in_degree[to_slot] += 1;
            }
        }

        // Очередь узлов без входящих рёбер
        let mut queue: VecDeque<Index> = VecDeque::new();
        
        // Собираем узлы с нулевой степенью
        for (idx, _) in self.nodes.iter() {
            let slot = idx.slot() as usize;
            if slot < node_count && in_degree[slot] == 0 {
                queue.push_back(idx);
            }
        }

        let mut result = Vec::with_capacity(node_count);

        while let Some(node) = queue.pop_front() {
            result.push(node);
            let node_slot = node.slot() as usize;

            // Обновляем степени соседей
            if node_slot < self.adjacency_out.len() {
                if let Some(edges) = self.adjacency_out.get(node_slot) {
                    for &edge_idx in edges {
                        if let Some(edge) = self.edges.get(edge_idx) {
                            let to_slot = edge.to.slot() as usize;
                            if to_slot < node_count {
                                in_degree[to_slot] -= 1;
                                if in_degree[to_slot] == 0 {
                                    queue.push_back(edge.to);
                                }
                            }
                        }
                    }
                }
            }
        }

        if result.len() == node_count {
            Ok(result)
        } else {
            Err(GraphError::CycleDetected)
        }
    }

    /// BFS обход с заданного узла
    pub fn bfs(&self, start: Index) -> Vec<Index> {
        let mut visited = FxHashSet::default();
        let mut queue = VecDeque::new();
        let mut result = Vec::new();

        queue.push_back(start);
        visited.insert(start);

        while let Some(node) = queue.pop_front() {
            result.push(node);

            for successor in self.successors(node) {
                if !visited.contains(&successor) {
                    visited.insert(successor);
                    queue.push_back(successor);
                }
            }
        }

        result
    }

    /// DFS обход с заданного узла
    pub fn dfs(&self, start: Index) -> Vec<Index> {
        let mut visited = FxHashSet::default();
        let mut result = Vec::new();
        self.dfs_recursive(start, &mut visited, &mut result);
        result
    }

    fn dfs_recursive(
        &self,
        node: Index,
        visited: &mut FxHashSet<Index>,
        result: &mut Vec<Index>,
    ) {
        if !visited.insert(node) {
            return;
        }
        result.push(node);

        for successor in self.successors(node) {
            self.dfs_recursive(successor, visited, result);
        }
    }

    /// Параллельные уровни — группы узлов которые можно выполнять одновременно
    /// Возвращает Vec<Vec<Index>> где каждый Vec — независимый уровень
    pub fn parallel_levels(&mut self) -> Result<Vec<Vec<Index>>, GraphError> {
        // Получаем sorted как Vec чтобы освободить borrow
        let sorted_vec = self.compute_topological_sort()?;
        let node_count = self.nodes.len();

        // level[node_slot] = на каком уровне находится узел
        let mut level: Vec<usize> = vec![0; node_count];

        for &node in &sorted_vec {
            let node_slot = node.slot() as usize;
            
            // Уровень = max(уровни предшественников) + 1
            let mut max_pred_level = 0;
            let slot = node_slot;
            if slot < self.adjacency_in.len() {
                if let Some(edges) = self.adjacency_in.get(slot) {
                    for &edge_idx in edges {
                        if let Some(edge) = self.edges.get(edge_idx) {
                            let pred_slot = edge.from.slot() as usize;
                            if pred_slot < node_count {
                                max_pred_level = max_pred_level.max(level[pred_slot]);
                            }
                        }
                    }
                }
            }
            
            level[node_slot] = max_pred_level + 1;
        }

        // Группируем по уровням
        let max_level = *level.iter().max().unwrap_or(&0);
        let mut levels: Vec<Vec<Index>> = vec![Vec::new(); max_level];

        // Собираем индексы по уровням
        for (idx, _) in self.nodes.iter() {
            let slot = idx.slot() as usize;
            if slot < node_count {
                let lvl = level[slot];
                if lvl > 0 {
                    // Уровни 1-based, но в Vec 0-based
                    levels[lvl - 1].push(idx);
                }
            }
        }

        Ok(levels)
    }

    /// Проверка наличия цикла
    pub fn has_cycle(&mut self) -> bool {
        self.topological_sort().is_err()
    }

    /// Все узлы достижимые из start
    pub fn reachable_from(&self, start: Index) -> FxHashSet<Index> {
        self.bfs(start).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustc_hash::FxHashMap;

    #[test]
    fn test_topological_sort() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        let c = g.add_node("C");

        // A → B → C
        g.add_edge(a, b, ());
        g.add_edge(b, c, ());

        let sorted = g.topological_sort().unwrap();
        // A должен быть раньше B, B раньше C
        let pos: FxHashMap<Index, usize> = sorted
            .iter()
            .enumerate()
            .map(|(i, &idx)| (idx, i))
            .collect();

        assert!(pos[&a] < pos[&b]);
        assert!(pos[&b] < pos[&c]);
    }

    #[test]
    fn test_cycle_detection() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");

        g.add_edge(a, b, ());
        g.add_edge(b, a, ()); // Цикл!

        assert!(g.has_cycle());
    }

    #[test]
    fn test_parallel_levels() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        let c = g.add_node("C");
        let d = g.add_node("D");

        // A → C
        // B → C
        // C → D
        g.add_edge(a, c, ());
        g.add_edge(b, c, ());
        g.add_edge(c, d, ());

        let levels = g.parallel_levels().unwrap();
        // Уровень 0: A, B (независимы)
        // Уровень 1: C
        // Уровень 2: D
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].len(), 2); // A и B параллельно
        assert_eq!(levels[1].len(), 1); // C
        assert_eq!(levels[2].len(), 1); // D
    }
}
