pub mod algorithms;

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
    // Используем Vec вместо HashMap так как индексы плотные (0..nodes.len())
    pub(crate) adjacency_out: Vec<SmallVec<[Index; 4]>>,
    // Входящие рёбра для каждого узла
    pub(crate) adjacency_in: Vec<SmallVec<[Index; 4]>>,
    // Кэш для topological_sort (инвалидируется при изменении графа)
    pub(crate) cached_topological: Option<Vec<Index>>,
    // Флаг изменения графа
    pub(crate) dirty: bool,
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
            adjacency_out: Vec::new(),
            adjacency_in: Vec::new(),
            cached_topological: None,
            dirty: false,
        }
    }

    /// Преобразует Index в usize для доступа к Vec
    /// Безопасно так как узлы только добавляются, не удаляются
    #[inline]
    fn index_to_usize(&self, idx: Index) -> usize {
        idx.slot() as usize
    }

    /// Добавить узел, вернуть его Index
    pub fn add_node(&mut self, data: N) -> Index {
        self.dirty = true; // Граф изменился
        let idx = self.nodes.insert(data);
        let slot = self.index_to_usize(idx);
        
        // Увеличиваем векторы если нужно
        if slot >= self.adjacency_out.len() {
            self.adjacency_out.resize(slot + 1, SmallVec::new());
            self.adjacency_in.resize(slot + 1, SmallVec::new());
        }
        
        // Убедимся что слот пустой (должен быть, так как только что создали)
        debug_assert!(self.adjacency_out[slot].is_empty());
        debug_assert!(self.adjacency_in[slot].is_empty());
        
        idx
    }

    /// Добавить направленное ребро from → to
    pub fn add_edge(&mut self, from: Index, to: Index, data: E) -> Index {
        self.dirty = true; // Граф изменился
        let edge = self.edges.insert(EdgeData {
            from,
            to,
            payload: data,
        });
        
        let from_slot = self.index_to_usize(from);
        let to_slot = self.index_to_usize(to);
        
        // Гарантируем что векторы достаточно большие
        let max_slot = from_slot.max(to_slot);
        if max_slot >= self.adjacency_out.len() {
            self.adjacency_out.resize(max_slot + 1, SmallVec::new());
            self.adjacency_in.resize(max_slot + 1, SmallVec::new());
        }
        
        self.adjacency_out[from_slot].push(edge);
        self.adjacency_in[to_slot].push(edge);
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
        let slot = self.index_to_usize(node);
        self.adjacency_in
            .get(slot)
            .into_iter()
            .flat_map(|edges| edges.iter())
            .filter_map(|&edge_idx| {
                self.edges.get(edge_idx).map(|e| e.from)
            })
    }

    /// Узлы в которые есть ребро из данного
    pub fn successors(&self, node: Index) -> impl Iterator<Item = Index> + '_ {
        let slot = self.index_to_usize(node);
        self.adjacency_out
            .get(slot)
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
