use std::collections::VecDeque;
use rustc_hash::{FxHashMap, FxHashSet};
use thunderdome::Index;

use crate::{Graph, GraphError};

impl<N, E> Graph<N, E> {
    /// Топологическая сортировка (алгоритм Кана)
    /// Возвращает узлы в порядке зависимостей
    /// Используется для порядка выполнения систем
    pub fn topological_sort(&self) -> Result<Vec<Index>, GraphError> {
        // Считаем входящие степени
        let mut in_degree: FxHashMap<Index, usize> = self
            .nodes
            .iter()
            .map(|(idx, _)| {
                let degree = self
                    .adjacency_in
                    .get(&idx)
                    .map(|v| v.len())
                    .unwrap_or(0);
                (idx, degree)
            })
            .collect();

        // Очередь узлов без входящих рёбер
        let mut queue: VecDeque<Index> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&idx, _)| idx)
            .collect();

        let mut result = Vec::with_capacity(self.nodes.len());

        while let Some(node) = queue.pop_front() {
            result.push(node);

            // Обновляем степени соседей
            if let Some(edges) = self.adjacency_out.get(&node) {
                for &edge_idx in edges {
                    if let Some(edge) = self.edges.get(edge_idx) {
                        let degree = in_degree.get_mut(&edge.to).unwrap();
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push_back(edge.to);
                        }
                    }
                }
            }
        }

        if result.len() == self.nodes.len() {
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
    pub fn parallel_levels(&self) -> Result<Vec<Vec<Index>>, GraphError> {
        let sorted = self.topological_sort()?;

        // level[node] = на каком уровне находится узел
        let mut level: FxHashMap<Index, usize> = FxHashMap::default();

        for &node in &sorted {
            // Уровень = max(уровни предшественников) + 1
            let node_level = self
                .predecessors(node)
                .filter_map(|pred| level.get(&pred))
                .max()
                .map(|&max_pred| max_pred + 1)
                .unwrap_or(0);

            level.insert(node, node_level);
        }

        // Группируем по уровням
        let max_level = level.values().max().copied().unwrap_or(0);
        let mut levels: Vec<Vec<Index>> = vec![Vec::new(); max_level + 1];

        for (node, lvl) in level {
            levels[lvl].push(node);
        }

        Ok(levels)
    }

    /// Проверка наличия цикла
    pub fn has_cycle(&self) -> bool {
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