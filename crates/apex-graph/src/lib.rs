pub mod algorithms;

use smallvec::SmallVec;
use thunderdome::{Arena, Index};
use thiserror::Error;

/// Универсальный направленный граф.
///
/// - N: данные узла
/// - W: вес ребра (или любые данные ребра)
///
/// Важное свойство реализации:
/// - `Index::slot()` используется как адресация в adjacency-векторах.
/// - Узлы/рёбра можно удалять: образуются "дырки" (sparse slots).
///   Алгоритмы должны работать в slot-space, а не через `nodes.len()`.
pub struct Graph<N, W> {
    pub(crate) nodes: Arena<N>,
    pub(crate) edges: Arena<EdgeData<W>>,

    /// Исходящие рёбра для каждого slot узла (храним индексы рёбер).
    pub(crate) adjacency_out: Vec<SmallVec<[Index; 4]>>,
    /// Входящие рёбра для каждого slot узла (храним индексы рёбер).
    pub(crate) adjacency_in: Vec<SmallVec<[Index; 4]>>,

    /// Кэш топологической сортировки.
    pub(crate) cached_topological: Option<Vec<Index>>,
    /// Флаг изменения графа.
    pub(crate) dirty: bool,
}

/// Данные ребра.
#[derive(Clone)]
pub struct EdgeData<W> {
    pub from: Index,
    pub to: Index,
    pub weight: W,
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("Cycle detected in graph")]
    CycleDetected,
    #[error("Node not found")]
    NodeNotFound,
    #[error("Edge not found")]
    EdgeNotFound,
}

impl<N, W> Default for Graph<N, W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N, W> Graph<N, W> {
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

    #[inline]
    fn index_to_usize(&self, idx: Index) -> usize {
        idx.slot() as usize
    }

    #[inline]
    fn ensure_slot_capacity(&mut self, slot: usize) {
        if slot >= self.adjacency_out.len() {
            self.adjacency_out.resize(slot + 1, SmallVec::new());
            self.adjacency_in.resize(slot + 1, SmallVec::new());
        }
    }

    #[inline]
    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.cached_topological = None;
    }

    // ── Узлы ─────────────────────────────────────────────────────────────

    /// Добавить узел, вернуть его Index.
    pub fn add_node(&mut self, data: N) -> Index {
        self.mark_dirty();
        let idx = self.nodes.insert(data);
        let slot = self.index_to_usize(idx);
        self.ensure_slot_capacity(slot);

        // Если slot переиспользован после удаления — гарантируем чистоту adjacency.
        self.adjacency_out[slot].clear();
        self.adjacency_in[slot].clear();

        idx
    }

    /// Удалить узел и все инцидентные рёбра.
    ///
    /// Возвращает данные узла (если существовал).
    pub fn remove_node(&mut self, node: Index) -> Option<N> {
        if self.nodes.get(node).is_none() {
            return None;
        }

        self.mark_dirty();

        let slot = self.index_to_usize(node);
        self.ensure_slot_capacity(slot);

        // Собираем все инцидентные рёбра. Возможны дубликаты (self-loop),
        // поэтому делаем dedup через FxHashSet.
        use rustc_hash::FxHashSet;
        let mut incident: FxHashSet<Index> = FxHashSet::default();

        for &e in &self.adjacency_out[slot] {
            incident.insert(e);
        }
        for &e in &self.adjacency_in[slot] {
            incident.insert(e);
        }

        // Удаляем рёбра.
        for e in incident {
            let _ = self.remove_edge(e);
        }

        // Очищаем adjacency списки slot.
        self.adjacency_out[slot].clear();
        self.adjacency_in[slot].clear();

        // Удаляем сам узел.
        self.nodes.remove(node)
    }

    #[inline]
    pub fn contains_node(&self, node: Index) -> bool {
        self.nodes.get(node).is_some()
    }

    pub fn node_data(&self, idx: Index) -> Option<&N> {
        self.nodes.get(idx)
    }

    pub fn node_data_mut(&mut self, idx: Index) -> Option<&mut N> {
        self.mark_dirty();
        self.nodes.get_mut(idx)
    }

    // ── Рёбра ────────────────────────────────────────────────────────────

    /// Безопасный вариант: возвращает ошибку если `from/to` не существуют.
    pub fn try_add_edge(&mut self, from: Index, to: Index, weight: W) -> Result<Index, GraphError> {
        if self.nodes.get(from).is_none() || self.nodes.get(to).is_none() {
            return Err(GraphError::NodeNotFound);
        }

        self.mark_dirty();

        let edge = self.edges.insert(EdgeData { from, to, weight });

        let from_slot = self.index_to_usize(from);
        let to_slot = self.index_to_usize(to);
        self.ensure_slot_capacity(from_slot.max(to_slot));

        self.adjacency_out[from_slot].push(edge);
        self.adjacency_in[to_slot].push(edge);

        Ok(edge)
    }

    /// Добавить направленное ребро from → to.
    ///
    /// Совместимо со старым API (panic при неверных узлах).
    pub fn add_edge(&mut self, from: Index, to: Index, weight: W) -> Index {
        self.try_add_edge(from, to, weight).expect("add_edge: node not found")
    }

    /// Удалить ребро.
    pub fn remove_edge(&mut self, edge: Index) -> Option<EdgeData<W>> {
        // Сначала копируем данные ребра, чтобы разорвать borrow
        let (from, to) = {
            let e = self.edges.get(edge)?;
            (e.from, e.to)
        };
        self.mark_dirty();

        let from_slot = self.index_to_usize(from);
        let to_slot = self.index_to_usize(to);

        if from_slot < self.adjacency_out.len() {
            remove_edge_from_list(&mut self.adjacency_out[from_slot], edge);
        }
        if to_slot < self.adjacency_in.len() {
            remove_edge_from_list(&mut self.adjacency_in[to_slot], edge);
        }

        self.edges.remove(edge)
    }

    /// Обновить endpoints ребра (from/to), с корректным обновлением adjacency.
    pub fn update_edge_endpoints(&mut self, edge: Index, new_from: Index, new_to: Index) -> Result<(), GraphError> {
        if self.edges.get(edge).is_none() {
            return Err(GraphError::EdgeNotFound);
        }
        if self.nodes.get(new_from).is_none() || self.nodes.get(new_to).is_none() {
            return Err(GraphError::NodeNotFound);
        }

        self.mark_dirty();

        // Старые значения.
        let (old_from, old_to) = {
            let e = self.edges.get(edge).ok_or(GraphError::EdgeNotFound)?;
            (e.from, e.to)
        };

        let old_from_slot = self.index_to_usize(old_from);
        let old_to_slot = self.index_to_usize(old_to);

        if old_from_slot < self.adjacency_out.len() {
            remove_edge_from_list(&mut self.adjacency_out[old_from_slot], edge);
        }
        if old_to_slot < self.adjacency_in.len() {
            remove_edge_from_list(&mut self.adjacency_in[old_to_slot], edge);
        }

        let new_from_slot = self.index_to_usize(new_from);
        let new_to_slot = self.index_to_usize(new_to);
        self.ensure_slot_capacity(new_from_slot.max(new_to_slot));

        self.adjacency_out[new_from_slot].push(edge);
        self.adjacency_in[new_to_slot].push(edge);

        // Обновляем данные edge в арене.
        let e_mut = self.edges.get_mut(edge).ok_or(GraphError::EdgeNotFound)?;
        e_mut.from = new_from;
        e_mut.to = new_to;

        Ok(())
    }

    /// Обновить вес ребра.
    pub fn update_edge_weight(&mut self, edge: Index, new_weight: W) -> Result<(), GraphError> {
        self.mark_dirty();
        let e = self.edges.get_mut(edge).ok_or(GraphError::EdgeNotFound)?;
        e.weight = new_weight;
        Ok(())
    }

    #[inline]
    pub fn contains_edge(&self, edge: Index) -> bool {
        self.edges.get(edge).is_some()
    }

    pub fn edge_data(&self, idx: Index) -> Option<&EdgeData<W>> {
        self.edges.get(idx)
    }

    pub fn edge_data_mut(&mut self, idx: Index) -> Option<&mut EdgeData<W>> {
        self.mark_dirty();
        self.edges.get_mut(idx)
    }

    pub fn edge_weight(&self, idx: Index) -> Option<&W> {
        self.edges.get(idx).map(|e| &e.weight)
    }

    pub fn edge_weight_mut(&mut self, idx: Index) -> Option<&mut W> {
        self.mark_dirty();
        self.edges.get_mut(idx).map(|e| &mut e.weight)
    }

    // ── Навигация ─────────────────────────────────────────────────────────

    /// Узлы из которых есть ребро в данный.
    pub fn predecessors(&self, node: Index) -> impl Iterator<Item = Index> + '_ {
        let slot = self.index_to_usize(node);
        self.adjacency_in
            .get(slot)
            .into_iter()
            .flat_map(|edges| edges.iter().copied())
            .filter_map(|edge_idx| self.edges.get(edge_idx).map(|e| e.from))
            .filter(move |&pred| self.nodes.get(pred).is_some())
    }

    /// Узлы в которые есть ребро из данного.
    pub fn successors(&self, node: Index) -> impl Iterator<Item = Index> + '_ {
        let slot = self.index_to_usize(node);
        self.adjacency_out
            .get(slot)
            .into_iter()
            .flat_map(|edges| edges.iter().copied())
            .filter_map(|edge_idx| self.edges.get(edge_idx).map(|e| e.to))
            .filter(move |&succ| self.nodes.get(succ).is_some())
    }

    // ── Статистика/итераторы ──────────────────────────────────────────────

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Итерация по всем живым узлам.
    pub fn nodes(&self) -> impl Iterator<Item = (Index, &N)> {
        self.nodes.iter()
    }

    /// Итерация по всем живым рёбрам.
    pub fn edges(&self) -> impl Iterator<Item = (Index, &EdgeData<W>)> {
        self.edges.iter()
    }

    /// Текущая "ёмкость" slot-space (в т.ч. дырки).
    #[inline]
    pub fn slot_capacity(&self) -> usize {
        self.adjacency_out.len().max(self.adjacency_in.len())
    }
}

#[inline]
fn remove_edge_from_list(list: &mut SmallVec<[Index; 4]>, edge: Index) {
    if let Some(pos) = list.iter().position(|&e| e == edge) {
        list.swap_remove(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_edge() {
        let mut g: Graph<&str, i32> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");

        let e = g.add_edge(a, b, 10);
        assert_eq!(g.edge_count(), 1);
        assert!(g.contains_edge(e));
        assert_eq!(g.edge_weight(e), Some(&10));

        let removed = g.remove_edge(e).unwrap();
        assert_eq!(removed.weight, 10);
        assert_eq!(g.edge_count(), 0);
        assert!(!g.contains_edge(e));
    }

    #[test]
    fn remove_node_removes_incident_edges() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        let c = g.add_node("C");

        let _e1 = g.add_edge(a, b, ());
        let _e2 = g.add_edge(b, c, ());
        let _e3 = g.add_edge(a, c, ());
        assert_eq!(g.edge_count(), 3);

        let removed = g.remove_node(b);
        assert_eq!(removed, Some("B"));
        // Осталось только ребро A->C
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn update_edge_endpoints_updates_adjacency() {
        let mut g: Graph<&str, ()> = Graph::new();
        let a = g.add_node("A");
        let b = g.add_node("B");
        let c = g.add_node("C");

        let e = g.add_edge(a, b, ());
        assert_eq!(g.successors(a).collect::<Vec<_>>(), vec![b]);

        g.update_edge_endpoints(e, a, c).unwrap();

        assert_eq!(g.successors(a).collect::<Vec<_>>(), vec![c]);
        assert_eq!(g.predecessors(c).collect::<Vec<_>>(), vec![a]);
        assert_eq!(g.predecessors(b).collect::<Vec<_>>(), Vec::<Index>::new());
    }
}