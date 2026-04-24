use rustc_hash::FxHashSet;
use thunderdome::Index;

use crate::{Graph, GraphError};

impl<N, W> Graph<N, W> {
    /// Проверяет, существует ли путь от `from` к `to` (BFS).
    ///
    /// Используется для предотвращения создания циклов при добавлении рёбер.
    pub fn has_path(&self, from: Index, to: Index) -> bool {
        if from == to {
            return true;
        }
        if self.nodes.get(from).is_none() || self.nodes.get(to).is_none() {
            return false;
        }

        let slot_cap = self.slot_capacity();
        let mut visited = vec![false; slot_cap];

        let mut queue: Vec<Index> = Vec::new();
        let mut head = 0usize;

        let start_slot = from.slot() as usize;
        if start_slot < visited.len() {
            visited[start_slot] = true;
        }
        queue.push(from);

        while head < queue.len() {
            let node = queue[head];
            head += 1;

            let node_slot = node.slot() as usize;
            if let Some(edges) = self.adjacency_out.get(node_slot) {
                for &edge_idx in edges {
                    let Some(edge) = self.edges.get(edge_idx) else { continue; };
                    let succ = edge.to;
                    if self.nodes.get(succ).is_none() {
                        continue;
                    }
                    if succ == to {
                        return true;
                    }
                    let succ_slot = succ.slot() as usize;
                    if succ_slot < visited.len() && !visited[succ_slot] {
                        visited[succ_slot] = true;
                        queue.push(succ);
                    }
                }
            }
        }

        false
    }

    /// Топологическая сортировка (алгоритм Кана) с кэшированием.
    ///
    /// Возвращает узлы в порядке зависимостей.
    /// Корректна при удалениях (дырки в slot-space).
    pub fn topological_sort(&mut self) -> Result<&[Index], GraphError> {
        if self.dirty || self.cached_topological.is_none() {
            let result = self.compute_topological_sort()?;
            self.cached_topological = Some(result);
            self.dirty = false;
        }
        Ok(self.cached_topological.as_ref().unwrap())
    }

    /// Внутренняя реализация топологической сортировки (без кэширования).
    ///
    /// Оптимизация: in_degree берётся из adjacency_in[slot].len(),
    /// т.е. без сканирования всех рёбер.
    pub fn compute_topological_sort(&self) -> Result<Vec<Index>, GraphError> {
        let live_nodes = self.nodes.len();
        if live_nodes == 0 {
            return Ok(Vec::new());
        }

        let slot_cap = self.slot_capacity();
        let mut in_degree: Vec<usize> = vec![0; slot_cap];

        // Заполняем indegree по входящим спискам.
        // Важно: берём только живые узлы (iter по Arena).
        for (node, _) in self.nodes.iter() {
            let slot = node.slot() as usize;
            let deg = self
                .adjacency_in
                .get(slot)
                .map(|v| v.len())
                .unwrap_or(0);
            in_degree[slot] = deg;
        }

        // Очередь узлов без входящих рёбер: Vec + head быстрее VecDeque.
        let mut queue: Vec<Index> = Vec::with_capacity(live_nodes);
        for (node, _) in self.nodes.iter() {
            let slot = node.slot() as usize;
            if in_degree[slot] == 0 {
                queue.push(node);
            }
        }

        let mut result: Vec<Index> = Vec::with_capacity(live_nodes);
        let mut head = 0usize;

        while head < queue.len() {
            let node = queue[head];
            head += 1;

            result.push(node);

            let node_slot = node.slot() as usize;
            if let Some(edges) = self.adjacency_out.get(node_slot) {
                for &edge_idx in edges {
                    let Some(edge) = self.edges.get(edge_idx) else { continue; };

                    let to = edge.to;
                    // На всякий: если пользователь как-то оставил ребро на несуществующий узел.
                    if self.nodes.get(to).is_none() {
                        continue;
                    }

                    let to_slot = to.slot() as usize;

                    // indegree должен быть > 0, но защищаемся от underflow.
                    let deg = &mut in_degree[to_slot];
                    if *deg == 0 {
                        continue;
                    }
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(to);
                    }
                }
            }
        }

        if result.len() == live_nodes {
            Ok(result)
        } else {
            Err(GraphError::CycleDetected)
        }
    }

    /// BFS обход с заданного узла.
    ///
    /// Оптимизация: visited = Vec<bool> по slot-space (быстрее HashSet).
    pub fn bfs(&self, start: Index) -> Vec<Index> {
        if self.nodes.get(start).is_none() {
            return Vec::new();
        }

        let slot_cap = self.slot_capacity();
        let mut visited = vec![false; slot_cap];

        let mut queue: Vec<Index> = Vec::new();
        let mut head = 0usize;

        let start_slot = start.slot() as usize;
        visited[start_slot] = true;
        queue.push(start);

        let mut result = Vec::new();

        while head < queue.len() {
            let node = queue[head];
            head += 1;
            result.push(node);

            let node_slot = node.slot() as usize;
            if let Some(edges) = self.adjacency_out.get(node_slot) {
                for &edge_idx in edges {
                    let Some(edge) = self.edges.get(edge_idx) else { continue; };
                    let succ = edge.to;
                    if self.nodes.get(succ).is_none() {
                        continue;
                    }
                    let succ_slot = succ.slot() as usize;
                    if succ_slot >= visited.len() {
                        continue;
                    }
                    if !visited[succ_slot] {
                        visited[succ_slot] = true;
                        queue.push(succ);
                    }
                }
            }
        }

        result
    }

    /// DFS обход с заданного узла (итеративный, без рекурсии).
    pub fn dfs(&self, start: Index) -> Vec<Index> {
        if self.nodes.get(start).is_none() {
            return Vec::new();
        }

        let slot_cap = self.slot_capacity();
        let mut visited = vec![false; slot_cap];
        let mut stack: Vec<Index> = Vec::new();
        stack.push(start);

        let mut result = Vec::new();

        while let Some(node) = stack.pop() {
            let slot = node.slot() as usize;
            if slot >= visited.len() || visited[slot] {
                continue;
            }
            visited[slot] = true;
            result.push(node);

            // Чтобы порядок был ближе к рекурсивному DFS,
            // добавляем successors в обратном порядке.
            if let Some(edges) = self.adjacency_out.get(slot) {
                for &edge_idx in edges.iter().rev() {
                    let Some(edge) = self.edges.get(edge_idx) else { continue; };
                    let succ = edge.to;
                    if self.nodes.get(succ).is_none() {
                        continue;
                    }
                    let succ_slot = succ.slot() as usize;
                    if succ_slot < visited.len() && !visited[succ_slot] {
                        stack.push(succ);
                    }
                }
            }
        }

        result
    }

    /// Параллельные уровни — группы узлов которые можно выполнять одновременно.
    ///
    /// Использует кэшированную топологическую сортировку (через topological_sort()).
    pub fn parallel_levels(&mut self) -> Result<Vec<Vec<Index>>, GraphError> {
        // Клонируем срез в Vec<Index>, чтобы разорвать borrow
        let sorted = self.topological_sort()?.to_vec(); // Vec<Index>

        let slot_cap = self.slot_capacity();
        let mut level: Vec<usize> = vec![0; slot_cap];

        for &node in &sorted {
            let node_slot = node.slot() as usize;

            let mut max_pred_level = 0usize;

            if let Some(edges) = self.adjacency_in.get(node_slot) {
                for &edge_idx in edges {
                    let Some(edge) = self.edges.get(edge_idx) else { continue; };
                    let pred = edge.from;
                    if self.nodes.get(pred).is_none() {
                        continue;
                    }
                    let pred_slot = pred.slot() as usize;
                    if pred_slot < level.len() {
                        max_pred_level = max_pred_level.max(level[pred_slot]);
                    }
                }
            }

            level[node_slot] = max_pred_level + 1;
        }

        let mut max_level = 0usize;
        for &node in &sorted {
            let slot = node.slot() as usize;
            max_level = max_level.max(level.get(slot).copied().unwrap_or(0));
        }

        let mut levels: Vec<Vec<Index>> = vec![Vec::new(); max_level.max(1)];
        for &node in &sorted {
            let slot = node.slot() as usize;
            let lvl = level[slot];
            if lvl > 0 {
                levels[lvl - 1].push(node);
            }
        }

        // Если граф пустой (sorted пуст), вернём пустой Vec
        if sorted.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(levels)
        }
    }

    /// Проверка наличия цикла.
    pub fn has_cycle(&mut self) -> bool {
        self.topological_sort().is_err()
    }

    /// Все узлы достижимые из start.
    pub fn reachable_from(&self, start: Index) -> FxHashSet<Index> {
        self.bfs(start).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustc_hash::FxHashMap;

    #[test]
    fn test_has_path_exists() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        let c = g.add_node("C");
        g.add_edge(a, b, ());
        g.add_edge(b, c, ());
        assert!(g.has_path(a, c));
        assert!(g.has_path(a, b));
        assert!(!g.has_path(b, a));
        assert!(!g.has_path(c, a));
    }

    #[test]
    fn test_has_path_self_loop() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        assert!(g.has_path(a, a));
    }

    #[test]
    fn test_has_path_nonexistent_nodes() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        // nodes exist but no edges
        assert!(!g.has_path(a, b));
        assert!(!g.has_path(b, a));
    }


    #[test]
    fn test_topological_sort_chain() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        let c = g.add_node("C");

        // A → B → C
        g.add_edge(a, b, ());
        g.add_edge(b, c, ());

        let sorted = g.topological_sort().unwrap();
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
        g.add_edge(b, a, ()); // цикл

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
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].len(), 2); // A,B
        assert_eq!(levels[1].len(), 1); // C
        assert_eq!(levels[2].len(), 1); // D
    }

    #[test]
    fn toposort_after_node_removal_is_correct() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        let c = g.add_node("C");

        g.add_edge(a, b, ());
        g.add_edge(b, c, ());

        // Удаляем B: должны исчезнуть оба ребра, останутся A и C без связей
        assert!(g.remove_node(b).is_some());

        let sorted = g.compute_topological_sort().unwrap();
        assert_eq!(sorted.len(), 2);
        assert!(sorted.contains(&a));
        assert!(sorted.contains(&c));
    }
}