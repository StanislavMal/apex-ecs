/// Entity — generational index
/// index: позиция в таблице, generation: защита от use-after-free
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Entity {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl Entity {
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

/// Где находятся данные entity в хранилище
#[derive(Clone, Copy, Debug)]
pub struct EntityLocation {
    pub archetype_id: crate::archetype::ArchetypeId,
    pub row: usize,
}

/// Запись в таблице entity
struct EntityRecord {
    generation: u32,
    /// None = entity мёртв
    location: Option<EntityLocation>,
}

/// Менеджер entity — выделяет и освобождает generational ID
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

    pub fn allocate(&mut self) -> Entity {
        if let Some(index) = self.free_list.pop() {
            let gen = self.records[index as usize].generation;
            Entity { index, generation: gen }
        } else {
            let index = self.next_index;
            self.next_index += 1;
            self.records.push(EntityRecord { generation: 0, location: None });
            Entity { index, generation: 0 }
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
        record.location = None;
        self.free_list.push(entity.index);
        true
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.records
            .get(entity.index as usize)
            .map(|r| r.generation == entity.generation && r.location.is_some())
            .unwrap_or(false)
    }

    #[inline]
    pub fn get_location(&self, entity: Entity) -> Option<EntityLocation> {
        self.records
            .get(entity.index as usize)
            .filter(|r| r.generation == entity.generation)
            .and_then(|r| r.location)
    }

    #[inline]
    pub fn set_location(&mut self, entity: Entity, location: EntityLocation) {
        if let Some(record) = self.records.get_mut(entity.index as usize) {
            if record.generation == entity.generation {
                record.location = Some(location);
            }
        }
    }

    pub fn len(&self) -> usize {
        // живые = все записи минус свободные
        self.records.len() - self.free_list.len()
    }

    /// Получить живой Entity по индексу (для восстановления из relation_id)
    pub fn get_by_index(&self, index: u32) -> Option<Entity> {
        let record = self.records.get(index as usize)?;
        if record.location.is_some() {
            Some(Entity { index, generation: record.generation })
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
        EntityLocation { archetype_id: ArchetypeId::EMPTY, row: 0 }
    }

    #[test]
    fn allocate_free_reuse() {
        let mut alloc = EntityAllocator::new();
        let e1 = alloc.allocate();
        let e2 = alloc.allocate();
        assert_ne!(e1, e2);

        // is_alive требует location — устанавливаем
        alloc.set_location(e1, make_loc());
        alloc.set_location(e2, make_loc());
        assert!(alloc.is_alive(e1));
        assert!(alloc.is_alive(e2));

        alloc.free(e1);
        assert!(!alloc.is_alive(e1));

        let e3 = alloc.allocate();
        assert_eq!(e3.index, e1.index);
        assert_ne!(e3.generation, e1.generation);
        // e3 ещё без location — не alive
        assert!(!alloc.is_alive(e3));
        alloc.set_location(e3, make_loc());
        assert!(alloc.is_alive(e3));
    }
}
