pub mod algorithms;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use thunderdome::{Arena, Index};
use thiserror::Error;

/// Универсальный направленный граф
/// N - данные узла
/// E - данные ребра
pub struct Graph<N, E> {
    pub(crate) nodes: Arena<N>,
    pub(crate) edges: Arena<EdgeData<E>>,
    // Исходящие рёбра для каждого узла
    pub(crate) adjacency_out: FxHashMap<Index, SmallVec<[Index; 4]>>,
    // Входящие рёбра для каждого узла
    pub(crate) adjacency_in: FxHashMap<Index, SmallVec<[Index; 4]>>,
}

pub struct EdgeData<E> {
    pub from: Index,
    pub to: Index,
    pub payload: E,
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("Cycle detected in graph")]
    CycleDetected,
    #[error("Node not found")]
    NodeNotFound,
}

impl<N, E> Default for Graph<N, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N, E> Graph<N, E> {
    pub fn new() -> Self {
        Self {
            nodes: Arena::new(),
            edges: Arena::new(),
            adjacency_out: FxHashMap::default(),
            adjacency_in: FxHashMap::default(),
        }
    }

    /// Добавить узел, вернуть его Index
    pub fn add_node(&mut self, data: N) -> Index {
        let idx = self.nodes.insert(data);
        self.adjacency_out.insert(idx, SmallVec::new());
        self.adjacency_in.insert(idx, SmallVec::new());
        idx
    }

    /// Добавить направленное ребро from → to
    pub fn add_edge(&mut self, from: Index, to: Index, data: E) -> Index {
        let edge = self.edges.insert(EdgeData {
            from,
            to,
            payload: data,
        });
        self.adjacency_out.entry(from).or_default().push(edge);
        self.adjacency_in.entry(to).or_default().push(edge);
        edge
    }

    pub fn node_data(&self, idx: Index) -> Option<&N> {
        self.nodes.get(idx)
    }

    pub fn node_data_mut(&mut self, idx: Index) -> Option<&mut N> {
        self.nodes.get_mut(idx)
    }

    pub fn edge_data(&self, idx: Index) -> Option<&EdgeData<E>> {
        self.edges.get(idx)
    }

    /// Узлы из которых есть ребро в данный
    pub fn predecessors(&self, node: Index) -> impl Iterator<Item = Index> + '_ {
        self.adjacency_in
            .get(&node)
            .into_iter()
            .flat_map(|edges| edges.iter())
            .filter_map(|&edge_idx| {
                self.edges.get(edge_idx).map(|e| e.from)
            })
    }

    /// Узлы в которые есть ребро из данного
    pub fn successors(&self, node: Index) -> impl Iterator<Item = Index> + '_ {
        self.adjacency_out
            .get(&node)
            .into_iter()
            .flat_map(|edges| edges.iter())
            .filter_map(|&edge_idx| {
                self.edges.get(edge_idx).map(|e| e.to)
            })
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Итерация по всем узлам
    pub fn nodes(&self) -> impl Iterator<Item = (Index, &N)> {
        self.nodes.iter()
    }
}