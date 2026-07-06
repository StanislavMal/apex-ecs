use rustc_hash::FxHashSet;
use thunderdome::Index;

use crate::{Graph, GraphError};

impl<N, W> Graph<N, W> {
    /// Checks whether a path exists from `from` to `to` (BFS).
    ///
    /// Used to prevent creating cycles when adding edges.
    /// Checks whether a path exists from `from` to `to` (BFS).
    ///
    /// Used to prevent creating cycles when adding edges.
    /// Optimization: reuses the `bfs_visited` and `bfs_queue` buffers from Graph
    /// instead of allocating on each call.
    pub fn has_path(&mut self, from: Index, to: Index) -> bool {
        if from == to {
            return true;
        }
        if self.nodes.get(from).is_none() || self.nodes.get(to).is_none() {
            return false;
        }

        let slot_cap = self.slot_capacity();

        // Reuse buffers: resize instead of vec![] — avoids allocation
        self.bfs_visited.clear();
        self.bfs_visited.resize(slot_cap, false);

        self.bfs_queue.clear();
        let mut head = 0usize;

        let start_slot = from.slot() as usize;
        if start_slot < self.bfs_visited.len() {
            self.bfs_visited[start_slot] = true;
        }
        self.bfs_queue.push(from);

        while head < self.bfs_queue.len() {
            let node = self.bfs_queue[head];
            head += 1;

            let node_slot = node.slot() as usize;
            if let Some(edges) = self.adjacency_out.get(node_slot) {
                for &edge_idx in edges {
                    let Some(edge) = self.edges.get(edge_idx) else {
                        continue;
                    };
                    let succ = edge.to;
                    if self.nodes.get(succ).is_none() {
                        continue;
                    }
                    if succ == to {
                        return true;
                    }
                    let succ_slot = succ.slot() as usize;
                    if succ_slot < self.bfs_visited.len() && !self.bfs_visited[succ_slot] {
                        self.bfs_visited[succ_slot] = true;
                        self.bfs_queue.push(succ);
                    }
                }
            }
        }

        false
    }

    /// Topological sort (Kahn's algorithm) with caching.
    ///
    /// Returns nodes in dependency order.
    /// Correct in the presence of removals (holes in slot-space).
    pub fn topological_sort(&mut self) -> Result<&[Index], GraphError> {
        if self.dirty || self.cached_topological.is_none() {
            let result = self.compute_topological_sort()?;
            self.cached_topological = Some(result);
            self.dirty = false;
        }
        Ok(self.cached_topological.as_ref().unwrap())
    }

    /// Internal implementation of the topological sort (without caching).
    ///
    /// Optimization: in_degree is taken from adjacency_in[slot].len(),
    /// i.e. without scanning all edges.
    pub fn compute_topological_sort(&self) -> Result<Vec<Index>, GraphError> {
        let live_nodes = self.nodes.len();
        if live_nodes == 0 {
            return Ok(Vec::new());
        }

        let slot_cap = self.slot_capacity();
        let mut in_degree: Vec<usize> = vec![0; slot_cap];

        // Fill indegree from the incoming lists.
        // Important: take only live nodes (iter over the Arena).
        for (node, _) in self.nodes.iter() {
            let slot = node.slot() as usize;
            let deg = self.adjacency_in.get(slot).map(|v| v.len()).unwrap_or(0);
            in_degree[slot] = deg;
        }

        // Queue of nodes with no incoming edges: Vec + head is faster than VecDeque.
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
                    let Some(edge) = self.edges.get(edge_idx) else {
                        continue;
                    };

                    let to = edge.to;
                    // Just in case: if the user somehow left an edge to a nonexistent node.
                    if self.nodes.get(to).is_none() {
                        continue;
                    }

                    let to_slot = to.slot() as usize;

                    // indegree should be > 0, but guard against underflow.
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

    /// BFS traversal from the given node.
    ///
    /// Optimization: visited = Vec<bool> over slot-space (faster than HashSet).
    /// Reuses the `bfs_visited` and `bfs_queue` buffers from Graph.
    pub fn bfs(&mut self, start: Index) -> Vec<Index> {
        if self.nodes.get(start).is_none() {
            return Vec::new();
        }

        let slot_cap = self.slot_capacity();

        // Reuse the buffers from Graph
        self.bfs_visited.clear();
        self.bfs_visited.resize(slot_cap, false);
        self.bfs_queue.clear();
        let mut head = 0usize;

        let start_slot = start.slot() as usize;
        if start_slot < self.bfs_visited.len() {
            self.bfs_visited[start_slot] = true;
        }
        self.bfs_queue.push(start);

        let mut result = Vec::new();

        while head < self.bfs_queue.len() {
            let node = self.bfs_queue[head];
            head += 1;
            result.push(node);

            let node_slot = node.slot() as usize;
            if let Some(edges) = self.adjacency_out.get(node_slot) {
                for &edge_idx in edges {
                    let Some(edge) = self.edges.get(edge_idx) else {
                        continue;
                    };
                    let succ = edge.to;
                    if self.nodes.get(succ).is_none() {
                        continue;
                    }
                    let succ_slot = succ.slot() as usize;
                    if succ_slot < self.bfs_visited.len() && !self.bfs_visited[succ_slot] {
                        self.bfs_visited[succ_slot] = true;
                        self.bfs_queue.push(succ);
                    }
                }
            }
        }

        result
    }

    /// DFS traversal from the given node (iterative, no recursion).
    ///
    /// Reuses the `dfs_visited` and `dfs_stack` buffers from Graph.
    pub fn dfs(&mut self, start: Index) -> Vec<Index> {
        if self.nodes.get(start).is_none() {
            return Vec::new();
        }

        let slot_cap = self.slot_capacity();
        self.dfs_visited.clear();
        self.dfs_visited.resize(slot_cap, false);
        self.dfs_stack.clear();
        self.dfs_stack.push(start);

        let mut result = Vec::new();

        while let Some(node) = self.dfs_stack.pop() {
            let slot = node.slot() as usize;
            if slot >= self.dfs_visited.len() || self.dfs_visited[slot] {
                continue;
            }
            self.dfs_visited[slot] = true;
            result.push(node);

            // To keep the order closer to recursive DFS,
            // push successors in reverse order.
            if let Some(edges) = self.adjacency_out.get(slot) {
                for &edge_idx in edges.iter().rev() {
                    let Some(edge) = self.edges.get(edge_idx) else {
                        continue;
                    };
                    let succ = edge.to;
                    if self.nodes.get(succ).is_none() {
                        continue;
                    }
                    let succ_slot = succ.slot() as usize;
                    if succ_slot < self.dfs_visited.len() && !self.dfs_visited[succ_slot] {
                        self.dfs_stack.push(succ);
                    }
                }
            }
        }

        result
    }

    /// Parallel levels — groups of nodes that can be executed simultaneously.
    ///
    /// Uses the cached topological sort (via topological_sort()).
    pub fn parallel_levels(&mut self) -> Result<Vec<Vec<Index>>, GraphError> {
        // Clone the slice into a Vec<Index> to break the borrow
        let sorted = self.topological_sort()?.to_vec(); // Vec<Index>

        let slot_cap = self.slot_capacity();
        let mut level: Vec<usize> = vec![0; slot_cap];

        for &node in &sorted {
            let node_slot = node.slot() as usize;

            let mut max_pred_level = 0usize;

            if let Some(edges) = self.adjacency_in.get(node_slot) {
                for &edge_idx in edges {
                    let Some(edge) = self.edges.get(edge_idx) else {
                        continue;
                    };
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

        // If the graph is empty (sorted is empty), return an empty Vec
        if sorted.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(levels)
        }
    }

    /// Check whether a cycle is present.
    pub fn has_cycle(&mut self) -> bool {
        self.topological_sort().is_err()
    }

    /// All nodes reachable from start.
    pub fn reachable_from(&mut self, start: Index) -> FxHashSet<Index> {
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
        g.add_edge(b, a, ()); // cycle

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

        // Remove B: both edges should disappear, leaving A and C unconnected
        assert!(g.remove_node(b).is_some());

        let sorted = g.compute_topological_sort().unwrap();
        assert_eq!(sorted.len(), 2);
        assert!(sorted.contains(&a));
        assert!(sorted.contains(&c));
    }
}
