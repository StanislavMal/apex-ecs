use std::any::TypeId;

/// Битовая маска компонентов — до 128 компонентов (достаточно для любой игры).
///
/// Заменяет `Vec<TypeId>` в AccessDescriptor для O(1) операций:
/// - `contains` → бит-проверка vs O(N) linear scan
/// - `conflicts_with` → битовый AND vs двойной linear scan
/// - `merge` → битовый OR vs dedup loop
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ComponentMask {
    lo: u64, // компоненты 0..63
    hi: u64, // компоненты 64..127
}

impl ComponentMask {
    pub const EMPTY: Self = Self { lo: 0, hi: 0 };

    #[inline]
    pub fn set(&mut self, idx: u8) {
        if idx < 64 {
            self.lo |= 1u64 << idx;
        } else {
            self.hi |= 1u64 << (idx - 64);
        }
    }

    #[inline]
    pub fn get(&self, idx: u8) -> bool {
        if idx < 64 {
            self.lo & (1u64 << idx) != 0
        } else {
            self.hi & (1u64 << (idx - 64)) != 0
        }
    }

    #[inline]
    pub fn and(&self, other: &Self) -> Self {
        Self { lo: self.lo & other.lo, hi: self.hi & other.hi }
    }

    #[inline]
    pub fn or(&self, other: &Self) -> Self {
        Self { lo: self.lo | other.lo, hi: self.hi | other.hi }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.lo == 0 && self.hi == 0
    }

    /// Пересекается ли маска с другой?
    #[inline]
    pub fn overlaps(&self, other: &Self) -> bool {
        !self.and(other).is_empty()
    }
}

/// Декларация Read/Write доступа системы к данным мира.
///
/// Использует два уровня представления:
/// - `TypeId` вектора — для регистрации компонентов (до первого compile)
/// - `ComponentMask` — для O(1) проверки конфликтов после назначения индексов
///
/// Правила конфликтов — аналог Rust borrow checker:
/// - Write + Read  → конфликт
/// - Write + Write → конфликт
/// - Read  + Read  → нет конфликта (параллельны)
#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    pub reads:  Vec<TypeId>,
    pub writes: Vec<TypeId>,
    /// Битовые маски — заполняются планировщиком через `assign_masks`.
    pub read_mask:  ComponentMask,
    pub write_mask: ComponentMask,
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
        // Маски сливаем битовым OR
        self.read_mask  = self.read_mask.or(&other.read_mask);
        self.write_mask = self.write_mask.or(&other.write_mask);
        self
    }

    /// Назначить битовые маски на основе маппинга TypeId → индекс компонента.
    ///
    /// Вызывается планировщиком один раз после регистрации всех компонентов.
    /// После этого `conflicts_with_fast` даёт O(1) проверку.
    pub fn assign_masks(&mut self, type_to_idx: &std::collections::HashMap<TypeId, u8>) {
        self.read_mask  = ComponentMask::EMPTY;
        self.write_mask = ComponentMask::EMPTY;
        for tid in &self.reads  {
            if let Some(&idx) = type_to_idx.get(tid) { self.read_mask.set(idx); }
        }
        for tid in &self.writes {
            if let Some(&idx) = type_to_idx.get(tid) { self.write_mask.set(idx); }
        }
    }

    /// O(1) проверка конфликта через битовые маски.
    ///
    /// Требует предварительного вызова `assign_masks`.
    #[inline]
    pub fn conflicts_with_fast(&self, other: &AccessDescriptor) -> bool {
        // Write(self) ∩ (Read(other) | Write(other)) != ∅
        // или Write(other) ∩ Read(self) != ∅
        self.write_mask.overlaps(&other.read_mask)
            || self.write_mask.overlaps(&other.write_mask)
            || other.write_mask.overlaps(&self.read_mask)
    }

    /// Fallback O(N) проверка — используется если маски не назначены.
    pub fn conflicts_with(&self, other: &AccessDescriptor) -> bool {
        // Быстрый путь через маски если они назначены
        if !self.write_mask.is_empty() || !other.write_mask.is_empty() {
            return self.conflicts_with_fast(other);
        }
        // Fallback: linear scan
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