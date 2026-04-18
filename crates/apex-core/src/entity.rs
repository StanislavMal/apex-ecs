use std::sync::atomic::{AtomicU32, Ordering};

/// Entity — просто ID
/// Старшие 32 бит = поколение (защита от use-after-free)
/// Младшие 32 бит = индекс
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Entity {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl Entity {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }
}

impl std::fmt::Display for Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Entity({}v{})", self.index, self.generation)
    }
}

/// Запись об entity в хранилище
#[derive(Debug)]
pub(crate) struct EntityRecord {
    pub generation: u32,
    pub location: Option<EntityLocation>,
}

/// Где находятся данные entity
#[derive(Clone, Copy, Debug)]
pub struct EntityLocation {
    pub archetype_id: crate::archetype::ArchetypeId,
    pub row: usize,
}

/// Менеджер entity — выделяет и освобождает ID
pub struct EntityAllocator {
    /// Следующий свободный индекс
    next_index: AtomicU32,
    /// Записи (index → generation + location)
    records: Vec<EntityRecord>,
    /// Переиспользуемые индексы
    free_list: Vec<u32>,
}

impl EntityAllocator {
    pub fn new() -> Self {
        Self {
            next_index: AtomicU32::new(0),
            records: Vec::new(),
            free_list: Vec::new(),
        }
    }

    /// Выделить новый Entity
    pub fn allocate(&mut self) -> Entity {
        if let Some(index) = self.free_list.pop() {
            // Переиспользуем старый индекс с новым поколением
            let record = &self.records[index as usize];
            Entity {
                index,
                generation: record.generation,
            }
        } else {
            // Новый индекс
            let index = self.next_index.fetch_add(1, Ordering::Relaxed);
            self.records.push(EntityRecord {
                generation: 0,
                location: None,
            });
            Entity { index, generation: 0 }
        }
    }

    /// Освободить Entity
    pub fn free(&mut self, entity: Entity) -> bool {
        let record = match self.records.get_mut(entity.index as usize) {
            Some(r) => r,
            None => return false,
        };

        if record.generation != entity.generation {
            return false; // Уже устаревший entity
        }

        // Увеличиваем поколение — старые ссылки станут невалидными
        record.generation += 1;
        record.location = None;
        self.free_list.push(entity.index);
        true
    }

    /// Проверить валидность entity
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.records
            .get(entity.index as usize)
            .map(|r| r.generation == entity.generation)
            .unwrap_or(false)
    }

    /// Получить местоположение entity
    pub fn get_location(&self, entity: Entity) -> Option<EntityLocation> {
        self.records
            .get(entity.index as usize)
            .filter(|r| r.generation == entity.generation)
            .and_then(|r| r.location)
    }

    /// Установить местоположение entity
    pub fn set_location(&mut self, entity: Entity, location: EntityLocation) {
        if let Some(record) = self.records.get_mut(entity.index as usize) {
            if record.generation == entity.generation {
                record.location = Some(location);
            }
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

    #[test]
    fn test_allocate_free() {
        let mut alloc = EntityAllocator::new();
        let e1 = alloc.allocate();
        let e2 = alloc.allocate();

        assert_ne!(e1, e2);
        assert!(alloc.is_alive(e1));
        assert!(alloc.is_alive(e2));

        alloc.free(e1);
        assert!(!alloc.is_alive(e1));

        // Новый entity может получить тот же индекс но другое поколение
        let e3 = alloc.allocate();
        assert_ne!(e1, e3); // Разные поколения!
        assert!(alloc.is_alive(e3));
    }
}