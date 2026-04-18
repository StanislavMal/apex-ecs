/// Sparse Set — быстрое добавление/удаление, хорошая итерация
/// Используется для редко встречающихся компонентов
///
/// sparse[entity_index] → позиция в dense
/// dense[pos] → entity_index
/// data[pos] → компонент

pub struct SparseSet<T> {
    /// sparse[entity_idx] = позиция в dense (или u32::MAX если нет)
    sparse: Vec<u32>,
    /// dense[pos] = entity_idx
    dense: Vec<u32>,
    /// data[pos] = значение компонента
    data: Vec<T>,
}

impl<T> SparseSet<T> {
    pub fn new() -> Self {
        Self {
            sparse: Vec::new(),
            dense: Vec::new(),
            data: Vec::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            sparse: Vec::new(),
            dense: Vec::with_capacity(capacity),
            data: Vec::with_capacity(capacity),
        }
    }

    /// Вставить или обновить значение для entity
    pub fn insert(&mut self, entity_index: u32, value: T) {
        let idx = entity_index as usize;

        // Расширяем sparse если нужно
        if idx >= self.sparse.len() {
            self.sparse.resize(idx + 1, u32::MAX);
        }

        let pos = self.sparse[idx];

        if pos != u32::MAX {
            // Обновляем существующее
            self.data[pos as usize] = value;
        } else {
            // Новый элемент
            let new_pos = self.dense.len() as u32;
            self.sparse[idx] = new_pos;
            self.dense.push(entity_index);
            self.data.push(value);
        }
    }

    /// Удалить элемент (swap with last для O(1))
    pub fn remove(&mut self, entity_index: u32) -> Option<T> {
        let idx = entity_index as usize;
        if idx >= self.sparse.len() {
            return None;
        }

        let pos = self.sparse[idx];
        if pos == u32::MAX {
            return None;
        }

        let last_entity = *self.dense.last().unwrap();
        let pos_usize = pos as usize;
        let last_pos = self.dense.len() - 1;

        // Swap с последним
        self.dense.swap(pos_usize, last_pos);
        self.data.swap(pos_usize, last_pos);

        // Обновляем sparse для перемещённого элемента
        self.sparse[last_entity as usize] = pos;
        // Помечаем удалённый как отсутствующий
        self.sparse[idx] = u32::MAX;

        self.dense.pop();
        Some(self.data.pop().unwrap())
    }

    pub fn get(&self, entity_index: u32) -> Option<&T> {
        let idx = entity_index as usize;
        if idx >= self.sparse.len() {
            return None;
        }
        let pos = self.sparse[idx];
        if pos == u32::MAX {
            None
        } else {
            Some(&self.data[pos as usize])
        }
    }

    pub fn get_mut(&mut self, entity_index: u32) -> Option<&mut T> {
        let idx = entity_index as usize;
        if idx >= self.sparse.len() {
            return None;
        }
        let pos = self.sparse[idx];
        if pos == u32::MAX {
            None
        } else {
            Some(&mut self.data[pos as usize])
        }
    }

    pub fn contains(&self, entity_index: u32) -> bool {
        let idx = entity_index as usize;
        idx < self.sparse.len() && self.sparse[idx] != u32::MAX
    }

    pub fn len(&self) -> usize {
        self.dense.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dense.is_empty()
    }

    /// Итерация по всем (entity_index, &value)
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.dense.iter().copied().zip(self.data.iter())
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (u32, &mut T)> {
        self.dense.iter().copied().zip(self.data.iter_mut())
    }

    pub fn values(&self) -> &[T] {
        &self.data
    }

    pub fn values_mut(&mut self) -> &mut [T] {
        &mut self.data
    }
}

impl<T> Default for SparseSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sparse_set() {
        let mut set: SparseSet<i32> = SparseSet::new();

        set.insert(0, 100);
        set.insert(5, 200);
        set.insert(10, 300);

        assert_eq!(set.get(0), Some(&100));
        assert_eq!(set.get(5), Some(&200));
        assert_eq!(set.get(10), Some(&300));
        assert_eq!(set.get(1), None);

        let removed = set.remove(5);
        assert_eq!(removed, Some(200));
        assert_eq!(set.get(5), None);
        assert_eq!(set.len(), 2);

        // Остальные на месте
        assert_eq!(set.get(0), Some(&100));
        assert_eq!(set.get(10), Some(&300));
    }
}