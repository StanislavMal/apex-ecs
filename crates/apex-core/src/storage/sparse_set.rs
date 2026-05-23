use rustc_hash::FxHashMap;

/// Порог переключения: если entity_index > len(dense) * SPARSITY_THRESHOLD,
/// переключаемся на HashMap-backend.
const SPARSITY_THRESHOLD: usize = 4;

/// Adaptive Sparse Set.
///
/// При малой разреженности (entity index плотный) использует Dense backend:
/// sparse[entity_index] → позиция в dense.
///
/// При высокой разреженности (entity index намного больше количества элементов)
/// автоматически переключается на FxHashMap.
pub struct SparseSet<T> {
    inner: SparseSetInner<T>,
}

enum SparseSetInner<T> {
    Dense {
        sparse: Vec<u32>,
        dense: Vec<u32>,
        data: Vec<T>,
    },
    Sparse {
        map: FxHashMap<u32, T>,
        dense: Vec<u32>, // для сохранения порядка итерации
    },
}

impl<T> SparseSet<T> {
    pub fn new() -> Self {
        Self {
            inner: SparseSetInner::Dense {
                sparse: Vec::new(),
                dense: Vec::new(),
                data: Vec::new(),
            },
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: SparseSetInner::Dense {
                sparse: Vec::new(),
                dense: Vec::with_capacity(capacity),
                data: Vec::with_capacity(capacity),
            },
        }
    }

    pub fn insert(&mut self, entity_index: u32, value: T) {
        match &mut self.inner {
            SparseSetInner::Dense {
                sparse,
                dense,
                data,
            } => {
                let idx = entity_index as usize;

                // Проверяем необходимость перехода на HashMap-backend
                if idx > dense.len().saturating_mul(SPARSITY_THRESHOLD).max(64) && idx > 1024 {
                    // Конвертируем в HashMap
                    let mut map = FxHashMap::default();
                    let existing_dense = std::mem::take(dense);
                    let existing_data = std::mem::take(data);
                    let dense_copy = existing_dense.clone();
                    for (entity, val) in existing_dense.into_iter().zip(existing_data.into_iter()) {
                        map.insert(entity, val);
                    }
                    map.insert(entity_index, value);
                    let mut new_dense = dense_copy;
                    new_dense.push(entity_index);
                    self.inner = SparseSetInner::Sparse {
                        map,
                        dense: new_dense,
                    };
                    return;
                }

                // Обычный Dense путь
                if idx >= sparse.len() {
                    sparse.resize(idx + 1, u32::MAX);
                }
                let pos = sparse[idx];
                if pos != u32::MAX {
                    data[pos as usize] = value;
                } else {
                    let new_pos = dense.len() as u32;
                    sparse[idx] = new_pos;
                    dense.push(entity_index);
                    data.push(value);
                }
            }
            SparseSetInner::Sparse { map, dense } => {
                if !map.contains_key(&entity_index) {
                    dense.push(entity_index);
                }
                map.insert(entity_index, value);
            }
        }
    }

    pub fn remove(&mut self, entity_index: u32) -> Option<T> {
        match &mut self.inner {
            SparseSetInner::Dense {
                sparse,
                dense,
                data,
            } => {
                let idx = entity_index as usize;
                if idx >= sparse.len() {
                    return None;
                }
                let pos = sparse[idx];
                if pos == u32::MAX {
                    return None;
                }

                let last_entity = *dense.last().unwrap();
                let pos_usize = pos as usize;
                let last_pos = dense.len() - 1;

                dense.swap(pos_usize, last_pos);
                data.swap(pos_usize, last_pos);

                sparse[last_entity as usize] = pos;
                sparse[idx] = u32::MAX;

                dense.pop();
                Some(data.pop().unwrap())
            }
            SparseSetInner::Sparse { map, dense } => {
                if let Some(val) = map.remove(&entity_index) {
                    if let Some(pos) = dense.iter().position(|&e| e == entity_index) {
                        dense.swap_remove(pos);
                    }
                    Some(val)
                } else {
                    None
                }
            }
        }
    }

    pub fn get(&self, entity_index: u32) -> Option<&T> {
        match &self.inner {
            SparseSetInner::Dense { sparse, data, .. } => {
                let idx = entity_index as usize;
                if idx >= sparse.len() {
                    return None;
                }
                let pos = sparse[idx];
                if pos == u32::MAX {
                    None
                } else {
                    Some(&data[pos as usize])
                }
            }
            SparseSetInner::Sparse { map, .. } => map.get(&entity_index),
        }
    }

    pub fn get_mut(&mut self, entity_index: u32) -> Option<&mut T> {
        match &mut self.inner {
            SparseSetInner::Dense { sparse, data, .. } => {
                let idx = entity_index as usize;
                if idx >= sparse.len() {
                    return None;
                }
                let pos = sparse[idx];
                if pos == u32::MAX {
                    None
                } else {
                    Some(&mut data[pos as usize])
                }
            }
            SparseSetInner::Sparse { map, .. } => map.get_mut(&entity_index),
        }
    }

    pub fn contains(&self, entity_index: u32) -> bool {
        self.get(entity_index).is_some()
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            SparseSetInner::Dense { dense, .. } => dense.len(),
            SparseSetInner::Sparse { map, .. } => map.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        struct EitherIter<L, R>(EitherEnum<L, R>);
        enum EitherEnum<L, R> {
            Left(L),
            Right(R),
        }
        impl<L: Iterator, R: Iterator<Item = L::Item>> Iterator for EitherIter<L, R> {
            type Item = L::Item;
            fn next(&mut self) -> Option<Self::Item> {
                match &mut self.0 {
                    EitherEnum::Left(l) => l.next(),
                    EitherEnum::Right(r) => r.next(),
                }
            }
        }

        match &self.inner {
            SparseSetInner::Dense { dense, data, .. } => {
                EitherIter(EitherEnum::Left(dense.iter().copied().zip(data.iter())))
            }
            SparseSetInner::Sparse { map, dense, .. } => EitherIter(EitherEnum::Right(
                dense
                    .iter()
                    .copied()
                    .filter_map(move |e| map.get(&e).map(|v| (e, v))),
            )),
        }
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (u32, &mut T)> {
        struct EitherIterMut<L, R>(EitherEnum<L, R>);
        enum EitherEnum<L, R> {
            Left(L),
            Right(R),
        }
        impl<L: Iterator, R: Iterator<Item = L::Item>> Iterator for EitherIterMut<L, R> {
            type Item = L::Item;
            fn next(&mut self) -> Option<Self::Item> {
                match &mut self.0 {
                    EitherEnum::Left(l) => l.next(),
                    EitherEnum::Right(r) => r.next(),
                }
            }
        }

        match &mut self.inner {
            SparseSetInner::Dense { dense, data, .. } => {
                EitherIterMut(EitherEnum::Left(dense.iter().copied().zip(data.iter_mut())))
            }
            SparseSetInner::Sparse { map, dense, .. } => {
                let keys = std::mem::take(dense);
                EitherIterMut(EitherEnum::Right(SparseIterMut {
                    map_ptr: map as *mut FxHashMap<u32, T>,
                    keys,
                    pos: 0,
                    _marker: std::marker::PhantomData,
                }))
            }
        }
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        struct EitherIterV<L, R>(EitherEnum<L, R>);
        enum EitherEnum<L, R> {
            Left(L),
            Right(R),
        }
        impl<L: Iterator, R: Iterator<Item = L::Item>> Iterator for EitherIterV<L, R> {
            type Item = L::Item;
            fn next(&mut self) -> Option<Self::Item> {
                match &mut self.0 {
                    EitherEnum::Left(l) => l.next(),
                    EitherEnum::Right(r) => r.next(),
                }
            }
        }

        match &self.inner {
            SparseSetInner::Dense { data, .. } => EitherIterV(EitherEnum::Left(data.iter())),
            SparseSetInner::Sparse { map, .. } => EitherIterV(EitherEnum::Right(map.values())),
        }
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        struct EitherIterVM<L, R>(EitherEnum<L, R>);
        enum EitherEnum<L, R> {
            Left(L),
            Right(R),
        }
        impl<L: Iterator, R: Iterator<Item = L::Item>> Iterator for EitherIterVM<L, R> {
            type Item = L::Item;
            fn next(&mut self) -> Option<Self::Item> {
                match &mut self.0 {
                    EitherEnum::Left(l) => l.next(),
                    EitherEnum::Right(r) => r.next(),
                }
            }
        }

        match &mut self.inner {
            SparseSetInner::Dense { data, .. } => EitherIterVM(EitherEnum::Left(data.iter_mut())),
            SparseSetInner::Sparse { map, .. } => EitherIterVM(EitherEnum::Right(map.values_mut())),
        }
    }
}

/// Итератор для Sparse варианта SparseSet (возвращает &mut T).
struct SparseIterMut<'a, T> {
    map_ptr: *mut FxHashMap<u32, T>,
    keys: Vec<u32>,
    pos: usize,
    _marker: std::marker::PhantomData<&'a mut FxHashMap<u32, T>>,
}

impl<'a, T> Iterator for SparseIterMut<'a, T> {
    type Item = (u32, &'a mut T);

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.keys.len() {
            let entity = self.keys[self.pos];
            self.pos += 1;
            // SAFETY:
            // - map_ptr указывает на FxHashMap, который живёт не меньше 'a
            // - каждый entity встречается в keys не более одного раза
            // - мы никогда не отдаём две mutable ссылки на один и тот же элемент
            let map = unsafe { &mut *self.map_ptr };
            if let Some(value) = map.get_mut(&entity) {
                let extended = unsafe { &mut *(value as *mut T) };
                return Some((entity, extended));
            }
        }
        None
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

        assert_eq!(set.get(0), Some(&100));
        assert_eq!(set.get(10), Some(&300));
    }

    #[test]
    fn test_sparse_set_adaptive_switch() {
        let mut set: SparseSet<i32> = SparseSet::new();
        // Вставляем элементы с плотными индексами (остаёмся в Dense)
        for i in 0..10 {
            set.insert(i, i as i32 * 10);
        }
        assert_eq!(set.len(), 10);

        // Вставляем с очень разреженным индексом — должен переключиться на Sparse
        set.insert(5000, 999);
        assert_eq!(set.len(), 11);
        assert_eq!(set.get(5000), Some(&999));

        // Проверяем что старые элементы на месте
        assert_eq!(set.get(0), Some(&0));
        assert_eq!(set.get(9), Some(&90));

        // Проверяем remove в Sparse режиме
        let removed = set.remove(5000);
        assert_eq!(removed, Some(999));
        assert_eq!(set.get(5000), None);
        assert_eq!(set.len(), 10);
    }

    #[test]
    fn test_sparse_set_iter_values() {
        let mut set: SparseSet<i32> = SparseSet::new();
        set.insert(0, 10);
        set.insert(1, 20);
        set.insert(2, 30);

        let items: std::collections::HashSet<i32> = set.iter().map(|(_, v)| *v).collect();
        assert!(items.contains(&10));
        assert!(items.contains(&20));
        assert!(items.contains(&30));
        assert_eq!(items.len(), 3);

        // values
        let vals: Vec<&i32> = set.values().collect();
        assert_eq!(vals.len(), 3);
    }
}
