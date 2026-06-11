/// Entity — generational index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Entity {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl Entity {
    /// Sentinel value — guaranteed to never match a real entity.
    pub const PLACEHOLDER: Entity = Entity {
        index: u32::MAX,
        generation: u32::MAX,
    };

    /// Construct an `Entity` from raw parts. Only for bridge/ECS infrastructure.
    #[inline]
    pub const fn from_raw_parts(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    #[inline]
    pub fn index(self) -> u32 {
        self.index
    }
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

impl std::fmt::Display for Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Entity({}v{})", self.index, self.generation)
    }
}

const NO_LOCATION: u64 = u64::MAX;

#[derive(Clone, Copy, Debug)]
pub struct EntityLocation {
    pub archetype_id: crate::archetype::ArchetypeId,
    pub row: u32,
}

struct EntityRecord {
    generation: u32,
    encoded_location: u64,
}

impl EntityRecord {
    #[inline]
    fn location(&self) -> Option<EntityLocation> {
        if self.encoded_location == NO_LOCATION {
            None
        } else {
            let row = (self.encoded_location & 0xFFFF_FFFF) as u32;
            let archetype_id = (self.encoded_location >> 32) as u32;
            Some(EntityLocation {
                archetype_id: crate::archetype::ArchetypeId(archetype_id),
                row,
            })
        }
    }

    #[inline]
    fn set_location(&mut self, loc: EntityLocation) {
        self.encoded_location = (loc.row as u64) | ((loc.archetype_id.0 as u64) << 32);
    }

    #[inline]
    fn clear_location(&mut self) {
        self.encoded_location = NO_LOCATION;
    }

    #[inline]
    fn has_location(&self) -> bool {
        self.encoded_location != NO_LOCATION
    }
}

/// Менеджер entity — generational IDs с batch API.
pub struct EntityAllocator {
    next_index: u32,
    records: Vec<EntityRecord>,
    free_list: Vec<u32>,
}

impl EntityAllocator {
    pub fn new() -> Self {
        Self {
            next_index: 0,
            records: Vec::new(),
            free_list: Vec::new(),
        }
    }

    /// Выделить одну entity.
    pub fn allocate(&mut self) -> Entity {
        if let Some(index) = self.free_list.pop() {
            let gen = self.records[index as usize].generation;
            Entity {
                index,
                generation: gen,
            }
        } else {
            let index = self.next_index;
            self.next_index += 1;
            self.records.push(EntityRecord {
                generation: 0,
                encoded_location: NO_LOCATION,
            });
            Entity {
                index,
                generation: 0,
            }
        }
    }

    /// Выделить N entity за один проход — batch API.
    ///
    /// Алгоритм:
    /// 1. Сначала берём из free_list (переиспользование индексов)
    /// 2. Потом один `Vec::resize_with` для новых записей
    ///
    /// Возвращает `Vec<Entity>` — один heap alloc вместо N.
    pub fn allocate_batch(&mut self, count: usize) -> Vec<Entity> {
        let mut entities = Vec::with_capacity(count);

        // 1. Дренируем free_list
        let from_free = count.min(self.free_list.len());
        for _ in 0..from_free {
            let index = self.free_list.pop().unwrap();
            let gen = self.records[index as usize].generation;
            entities.push(Entity {
                index,
                generation: gen,
            });
        }

        // 2. Новые записи одним resize_with
        let remaining = count - from_free;
        if remaining > 0 {
            let start = self.next_index as usize;
            self.next_index += remaining as u32;
            self.records
                .resize_with(start + remaining, || EntityRecord {
                    generation: 0,
                    encoded_location: NO_LOCATION,
                });
            for i in 0..remaining {
                entities.push(Entity {
                    index: (start + i) as u32,
                    generation: 0,
                });
            }
        }

        entities
    }

    /// Batch set_location — один проход по Vec без повторных bounds checks.
    ///
    /// Вызывается из `spawn_many` после batch allocate.
    /// `entities[i]` получает `EntityLocation { archetype_id, row: start_row + i }`.
    pub fn set_locations_batch(
        &mut self,
        entities: &[Entity],
        archetype_id: crate::archetype::ArchetypeId,
        start_row: u32,
    ) {
        for (i, entity) in entities.iter().enumerate() {
            let record = &mut self.records[entity.index as usize];
            // Проверяем generation только в debug
            debug_assert_eq!(record.generation, entity.generation);
            record.set_location(EntityLocation {
                archetype_id,
                row: start_row + i as u32,
            });
        }
    }

    pub fn free(&mut self, entity: Entity) -> bool {
        let record = match self.records.get_mut(entity.index as usize) {
            Some(r) => r,
            None => return false,
        };
        if record.generation != entity.generation {
            return false;
        }
        record.generation = record.generation.wrapping_add(1);
        record.clear_location();
        // W3-3: защита от ABA при wrap'е generation. free_list — LIFO, т.е.
        // churn концентрируется на ОДНОМ слоте (spawn+despawn временной entity
        // каждый кадр ≈ 2³² переиспользований за ~198 дней @250Hz). Слот,
        // дошедший до u32::MAX, РЕТИРУЕТСЯ (не возвращается в free_list):
        // ни одно значение generation не выдаётся дважды → застрявший хэндл
        // прошлой жизни никогда не «оживёт» на чужой entity. Цена — одна
        // запись EntityRecord на 2³² переиспользований (PLACEHOLDER использует
        // generation == u32::MAX — он и так никогда не жив).
        if record.generation != u32::MAX {
            self.free_list.push(entity.index);
        }
        true
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.records
            .get(entity.index as usize)
            .map(|r| r.generation == entity.generation && r.has_location())
            .unwrap_or(false)
    }

    #[inline]
    pub fn get_location(&self, entity: Entity) -> Option<EntityLocation> {
        self.records
            .get(entity.index as usize)
            .filter(|r| r.generation == entity.generation)
            .and_then(|r| r.location())
    }

    #[inline]
    pub fn set_location(&mut self, entity: Entity, location: EntityLocation) {
        if let Some(record) = self.records.get_mut(entity.index as usize) {
            if record.generation == entity.generation {
                record.set_location(location);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.records.len() - self.free_list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_by_index(&self, index: u32) -> Option<Entity> {
        let record = self.records.get(index as usize)?;
        if record.has_location() {
            Some(Entity {
                index,
                generation: record.generation,
            })
        } else {
            None
        }
    }
}

impl Default for EntityAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archetype::ArchetypeId;

    fn make_loc() -> EntityLocation {
        EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row: 0,
        }
    }

    #[test]
    fn allocate_free_reuse() {
        let mut alloc = EntityAllocator::new();
        let e1 = alloc.allocate();
        let e2 = alloc.allocate();
        assert_ne!(e1, e2);

        alloc.set_location(e1, make_loc());
        alloc.set_location(e2, make_loc());
        assert!(alloc.is_alive(e1));

        alloc.free(e1);
        assert!(!alloc.is_alive(e1));

        let e3 = alloc.allocate();
        assert_eq!(e3.index, e1.index);
        assert_ne!(e3.generation, e1.generation);
        alloc.set_location(e3, make_loc());
        assert!(alloc.is_alive(e3));
    }

    #[test]
    fn allocate_batch_basic() {
        let mut alloc = EntityAllocator::new();
        let batch = alloc.allocate_batch(100);
        assert_eq!(batch.len(), 100);
        // Все индексы уникальны
        let mut indices: Vec<u32> = batch.iter().map(|e| e.index).collect();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(indices.len(), 100);
    }

    #[test]
    fn allocate_batch_uses_free_list() {
        let mut alloc = EntityAllocator::new();
        // Создаём 5, освобождаем 2, batch 5 — должен переиспользовать 2
        let entities: Vec<Entity> = (0..5)
            .map(|_| {
                let e = alloc.allocate();
                alloc.set_location(e, make_loc());
                e
            })
            .collect();

        alloc.free(entities[1]);
        alloc.free(entities[3]);

        let batch = alloc.allocate_batch(5);
        assert_eq!(batch.len(), 5);

        // Два из batch должны иметь те же индексы что освобождённые
        let batch_indices: std::collections::HashSet<u32> = batch.iter().map(|e| e.index).collect();
        assert!(batch_indices.contains(&entities[1].index));
        assert!(batch_indices.contains(&entities[3].index));
    }

    // ── W3-3: generation-wrap / ABA ────────────────────────────

    #[test]
    fn slot_at_max_generation_is_retired_not_reused() {
        let mut alloc = EntityAllocator::new();
        let e = alloc.allocate();
        alloc.set_location(e, make_loc());

        // Симулируем 2³²−2 переиспользований слота: догоняем generation
        // до предпоследнего значения (поля приватны — тест в том же модуле).
        alloc.records[e.index as usize].generation = u32::MAX - 1;
        let last_life = Entity::from_raw_parts(e.index, u32::MAX - 1);
        alloc.set_location(last_life, make_loc());
        assert!(alloc.is_alive(last_life));

        // free доводит generation до MAX → слот ретирован.
        assert!(alloc.free(last_life));
        assert!(!alloc.is_alive(last_life));
        assert!(
            alloc.free_list.is_empty(),
            "слот на границе generation не возвращается в free_list"
        );

        // Следующий allocate берёт НОВЫЙ индекс, а не ретированный слот.
        let fresh = alloc.allocate();
        assert_ne!(fresh.index, e.index);

        // Стародавний хэндл первой жизни слота (generation 0) мёртв навсегда —
        // ABA невозможен: ни is_alive, ни get_location не дают чужую entity.
        assert!(!alloc.is_alive(e));
        assert!(alloc.get_location(e).is_none());
    }

    #[test]
    fn get_by_index_dead_slot_returns_none() {
        let mut alloc = EntityAllocator::new();
        let e = alloc.allocate();
        alloc.set_location(e, make_loc());
        assert_eq!(alloc.get_by_index(e.index), Some(e));
        alloc.free(e);
        assert_eq!(
            alloc.get_by_index(e.index),
            None,
            "слот без location (свободный/ретированный) не выдаёт entity"
        );
    }

    #[test]
    fn set_locations_batch() {
        let mut alloc = EntityAllocator::new();
        let entities = alloc.allocate_batch(10);
        let arch_id = ArchetypeId(42);
        alloc.set_locations_batch(&entities, arch_id, 0);

        for (i, entity) in entities.iter().enumerate() {
            let loc = alloc.get_location(*entity).unwrap();
            assert_eq!(loc.archetype_id.0, 42);
            assert_eq!(loc.row as usize, i);
        }
    }
}
