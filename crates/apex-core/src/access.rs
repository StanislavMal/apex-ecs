use std::any::TypeId;
use std::collections::HashSet;

/// Битовая маска компонентов — до 256 компонентов.
///
/// Заменяет `Vec<TypeId>` в AccessDescriptor для O(1) операций:
/// - `contains` → бит-проверка vs O(N) linear scan
/// - `conflicts_with` → битовый AND vs двойной linear scan
/// - `merge` → битовый OR vs dedup loop
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ComponentMask {
    bits: [u64; 4], // 4 × 64 = 256 бит
}

impl ComponentMask {
    pub const EMPTY: Self = Self { bits: [0u64; 4] };

    const MASK_64: u64 = 0x3F; // маска для idx % 64

    #[inline]
    fn word_idx(idx: u8) -> usize {
        (idx >> 6) as usize // idx / 64
    }

    #[inline]
    fn bit_idx(idx: u8) -> u64 {
        1u64 << (idx as u64 & Self::MASK_64)
    }

    #[inline]
    pub fn set(&mut self, idx: u8) {
        self.bits[Self::word_idx(idx)] |= Self::bit_idx(idx);
    }

    #[inline]
    pub fn get(&self, idx: u8) -> bool {
        self.bits[Self::word_idx(idx)] & Self::bit_idx(idx) != 0
    }

    #[inline]
    pub fn and(&self, other: &Self) -> Self {
        Self {
            bits: [
                self.bits[0] & other.bits[0],
                self.bits[1] & other.bits[1],
                self.bits[2] & other.bits[2],
                self.bits[3] & other.bits[3],
            ],
        }
    }

    #[inline]
    pub fn or(&self, other: &Self) -> Self {
        Self {
            bits: [
                self.bits[0] | other.bits[0],
                self.bits[1] | other.bits[1],
                self.bits[2] | other.bits[2],
                self.bits[3] | other.bits[3],
            ],
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bits[0] == 0 && self.bits[1] == 0 && self.bits[2] == 0 && self.bits[3] == 0
    }

    /// Пересекается ли маска с другой?
    #[inline]
    pub fn overlaps(&self, other: &Self) -> bool {
        (self.bits[0] & other.bits[0]) != 0
            || (self.bits[1] & other.bits[1]) != 0
            || (self.bits[2] & other.bits[2]) != 0
            || (self.bits[3] & other.bits[3]) != 0
    }
}

/// Битовая маска архетипов — до 1024 архетипов (16 × u64).
///
/// Позволяет O(1) проверять, какие архетипы соответствуют AccessDescriptor системы.
/// Заполняется планировщиком в `compile()` после того, как все архетипы созданы.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ArchetypeMask {
    bits: [u64; 16],
}

impl ArchetypeMask {
    pub const EMPTY: Self = Self { bits: [0u64; 16] };

    #[inline]
    pub fn set(&mut self, idx: usize) {
        if idx < 1024 {
            self.bits[idx / 64] |= 1u64 << (idx % 64);
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> bool {
        if idx < 1024 {
            self.bits[idx / 64] & (1u64 << (idx % 64)) != 0
        } else {
            false
        }
    }

    #[inline]
    pub fn and(&self, other: &Self) -> Self {
        let mut bits = [0u64; 16];
        for i in 0..16 {
            bits[i] = self.bits[i] & other.bits[i];
        }
        Self { bits }
    }

    #[inline]
    pub fn or(&self, other: &Self) -> Self {
        let mut bits = [0u64; 16];
        for i in 0..16 {
            bits[i] = self.bits[i] | other.bits[i];
        }
        Self { bits }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&b| b == 0)
    }

    /// Пересекается ли маска с другой?
    #[inline]
    pub fn overlaps(&self, other: &Self) -> bool {
        for i in 0..16 {
            if self.bits[i] & other.bits[i] != 0 {
                return true;
            }
        }
        false
    }

    /// Количество установленных битов.
    #[inline]
    pub fn count(&self) -> u32 {
        self.bits.iter().map(|&b| b.count_ones()).sum()
    }

    /// Итерация по установленным индексам.
    pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.bits.iter().enumerate().flat_map(|(chunk_i, &chunk)| {
            (0..64).filter_map(move |bit| {
                if chunk & (1u64 << bit) != 0 {
                    Some(chunk_i * 64 + bit)
                } else {
                    None
                }
            })
        })
    }
}

/// Декларация Read/Write доступа системы к данным мира.
///
/// Использует два уровня представления:
/// - `TypeId` вектора — для регистрации компонентов/событий (до первого compile)
/// - `ComponentMask` — для O(1) проверки конфликтов после назначения индексов
///
/// Правила конфликтов — аналог Rust borrow checker:
/// - Write + Read  → конфликт
/// - Write + Write → конфликт
/// - Read  + Read  → нет конфликта (параллельны)
///
/// Также поддерживает декларацию доступа к событиям (events):
/// - `read_event<T>()` / `write_event<T>()` — декларация чтения/записи событий
/// - Два писателя одного типа событий конфликтуют (WriteWrite)
/// - Писатель и читатель одного типа событий конфликтуют (WriteRead)
/// - Два читателя одного типа событий — НЕ конфликтуют
#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    pub reads:  Vec<TypeId>,
    pub writes: Vec<TypeId>,
    /// Типы событий, которые система читает.
    pub reads_event:  Vec<TypeId>,
    /// Типы событий, которые система пишет.
    pub writes_event: Vec<TypeId>,
    /// Битовые маски — заполняются планировщиком через `assign_masks`.
    pub read_mask:  ComponentMask,
    pub write_mask: ComponentMask,
    /// Маска архетипов — заполняется планировщиком в compile().
    /// Определяет, какие архетипы нужны этой системе.
    pub archetype_mask: ArchetypeMask,
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

    /// Декларировать чтение событий типа T.
    pub fn read_event<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.reads_event.contains(&tid) { self.reads_event.push(tid); }
        self
    }

    /// Декларировать запись событий типа T.
    pub fn write_event<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.writes_event.contains(&tid) { self.writes_event.push(tid); }
        self
    }

    pub fn merge(mut self, other: &AccessDescriptor) -> Self {
        // O(N+M) дедупликация через HashSet вместо O(N²) contains+push
        Self::dedup_push(&mut self.reads, &other.reads);
        Self::dedup_push(&mut self.writes, &other.writes);
        Self::dedup_push(&mut self.reads_event, &other.reads_event);
        Self::dedup_push(&mut self.writes_event, &other.writes_event);
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
        // Если маски не пусты — используем быстрый путь по битовым маскам
        if !self.write_mask.is_empty() || !other.write_mask.is_empty() {
            return self.conflicts_with_fast(other);
        }
        // Если маски пусты, но есть writes в векторах — значит assign_masks() не вызывался,
        // используем fallback (linear scan)
        if !self.writes.is_empty() || !other.writes.is_empty() {
            for w in &self.writes {
                if other.reads.contains(w) || other.writes.contains(w) { return true; }
            }
            for w in &other.writes {
                if self.reads.contains(w) || self.writes.contains(w) { return true; }
            }
        }
        // Нет writes — нет конфликта
        false
    }

    /// O(N+M) дедупликация через HashSet — заменяет O(N²) contains+push в merge.
    fn dedup_push(vec: &mut Vec<TypeId>, items: &[TypeId]) {
        if items.is_empty() {
            return;
        }
        let mut set: HashSet<TypeId> = vec.iter().cloned().collect();
        for &item in items {
            if set.insert(item) {
                vec.push(item);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.writes.is_empty()
            && self.reads_event.is_empty() && self.writes_event.is_empty()
    }
}