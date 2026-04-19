use std::any::TypeId;

/// Декларация Read/Write доступа системы к данным мира.
///
/// Используется планировщиком для построения графа зависимостей.
/// Правила конфликтов — аналог Rust borrow checker:
/// - Write + Read  → конфликт
/// - Write + Write → конфликт  
/// - Read  + Read  → нет конфликта (параллельны)
#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    pub reads:  Vec<TypeId>,
    pub writes: Vec<TypeId>,
}

impl AccessDescriptor {
    pub fn new() -> Self { Self::default() }

    pub fn read<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.reads.contains(&tid) { self.reads.push(tid); }
        self
    }

    pub fn write<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.writes.contains(&tid) { self.writes.push(tid); }
        self
    }

    pub fn merge(mut self, other: &AccessDescriptor) -> Self {
        for &tid in &other.reads  { if !self.reads.contains(&tid)  { self.reads.push(tid);  } }
        for &tid in &other.writes { if !self.writes.contains(&tid) { self.writes.push(tid); } }
        self
    }

    pub fn conflicts_with(&self, other: &AccessDescriptor) -> bool {
        for w in &self.writes {
            if other.reads.contains(w) || other.writes.contains(w) { return true; }
        }
        for w in &other.writes {
            if self.reads.contains(w) || self.writes.contains(w) { return true; }
        }
        false
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.writes.is_empty()
    }
}