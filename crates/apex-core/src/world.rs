use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    archetype::{Archetype, ArchetypeId},
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry},
    entity::{EntityAllocator, EntityLocation, Entity},
    query::QueryBuilder,
    storage::SparseSet,
};

/// Мир — центральное хранилище всего
pub struct World {
    pub(crate) entities: EntityAllocator,
    pub(crate) registry: ComponentRegistry,
    pub(crate) archetypes: Vec<Archetype>,
    pub(crate) archetype_index: FxHashMap<Vec<ComponentId>, ArchetypeId>,
    pub(crate) entity_locations: SparseSet<EntityLocation>,
}

impl World {
    pub fn new() -> Self {
        let mut world = Self {
            entities: EntityAllocator::new(),
            registry: ComponentRegistry::new(),
            archetypes: Vec::new(),
            archetype_index: FxHashMap::default(),
            entity_locations: SparseSet::new(),
        };

        // Пустой архетип (entity без компонентов)
        world.archetypes.push(Archetype::new(
            ArchetypeId::EMPTY,
            SmallVec::new(),
            &[],
        ));
        world.archetype_index.insert(Vec::new(), ArchetypeId::EMPTY);

        world
    }

    /// Зарегистрировать тип компонента
    pub fn register_component<T: Component>(&mut self) -> ComponentId {
        self.registry.register::<T>()
    }

    /// Создать новый Entity
    pub fn spawn(&mut self) -> EntityBuilder<'_> {
        let entity = self.entities.allocate();

        // Помещаем в пустой архетип
        let row = unsafe { self.archetypes[0].allocate_row(entity) };

        let location = EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        };
        self.entity_locations.insert(entity.index, location);
        self.entities.set_location(entity, location);

        EntityBuilder {
            world: self,
            entity,
        }
    }

    pub fn is_alive(&self, entity: Entity) -> bool {
        self.entities.is_alive(entity)
    }

    /// Добавить компонент к entity
    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) {
        let component_id = self.registry.get_or_register::<T>();
        self.insert_component(entity, component_id, component);
    }

    fn insert_component<T: Component>(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        component: T,
    ) {
        let location = match self.entity_locations.get(entity.index).copied() {
            Some(loc) => loc,
            None => return,
        };

        let current_idx = location.archetype_id.0 as usize;

        // Уже есть этот компонент — просто обновляем
        if self.archetypes[current_idx].has_component(component_id) {
            unsafe {
                if let Some(dst) = self.archetypes[current_idx]
                    .get_component_mut::<T>(location.row, component_id)
                {
                    *dst = component;
                }
            }
            return;
        }

        // Находим целевой архетип
        let new_archetype_id =
            self.find_or_create_archetype_with(location.archetype_id, component_id);

        // Перемещаем entity
        let new_row = self.move_entity(entity, location, new_archetype_id);

        // Записываем новый компонент
        unsafe {
            let src = &component as *const T as *const u8;
            let arch = &mut self.archetypes[new_archetype_id.0 as usize];
            arch.write_component(new_row, component_id, src);
        }
        // Данные скопированы побайтово — не вызываем drop
        std::mem::forget(component);

        let new_location = EntityLocation {
            archetype_id: new_archetype_id,
            row: new_row,
        };
        self.entity_locations.insert(entity.index, new_location);
        self.entities.set_location(entity, new_location);
    }

    /// Удалить компонент у entity
    pub fn remove<T: Component>(&mut self, entity: Entity) -> bool {
        let component_id = match self.registry.get_id::<T>() {
            Some(id) => id,
            None => return false,
        };

        let location = match self.entity_locations.get(entity.index).copied() {
            Some(loc) => loc,
            None => return false,
        };

        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return false;
        }

        let new_archetype_id =
            self.find_or_create_archetype_without(location.archetype_id, component_id);

        let new_row = self.move_entity(entity, location, new_archetype_id);

        let new_location = EntityLocation {
            archetype_id: new_archetype_id,
            row: new_row,
        };
        self.entity_locations.insert(entity.index, new_location);
        self.entities.set_location(entity, new_location);

        true
    }

    /// Уничтожить entity
    pub fn despawn(&mut self, entity: Entity) -> bool {
        if !self.entities.is_alive(entity) {
            return false;
        }

        let location = match self.entity_locations.get(entity.index).copied() {
            Some(loc) => loc,
            None => return false,
        };

        let arch_idx = location.archetype_id.0 as usize;

        unsafe {
            let displaced = self.archetypes[arch_idx].remove_row(location.row);

            if let Some(displaced_entity) = displaced {
                let displaced_loc = EntityLocation {
                    archetype_id: location.archetype_id,
                    row: location.row,
                };
                self.entity_locations
                    .insert(displaced_entity.index, displaced_loc);
                self.entities.set_location(displaced_entity, displaced_loc);
            }
        }

        self.entity_locations.remove(entity.index);
        self.entities.free(entity);

        true
    }

    /// Получить компонент (immutable)
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let component_id = self.registry.get_id::<T>()?;
        let location = self.entity_locations.get(entity.index)?;
        let arch = &self.archetypes[location.archetype_id.0 as usize];
        unsafe { arch.get_component::<T>(location.row, component_id) }
    }

    /// Получить компонент (mutable)
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let component_id = self.registry.get_id::<T>()?;
        let location = self.entity_locations.get(entity.index).copied()?;
        let arch = &mut self.archetypes[location.archetype_id.0 as usize];
        unsafe { arch.get_component_mut::<T>(location.row, component_id) }
    }

    /// Построить запрос
    pub fn query(&self) -> QueryBuilder<'_> {
        QueryBuilder::new(self)
    }

    pub fn entity_count(&self) -> usize {
        self.entity_locations.len()
    }

    pub fn archetype_count(&self) -> usize {
        self.archetypes.len()
    }

    // ── Внутренние методы ──────────────────────────────────────

    fn find_or_create_archetype_with(
        &mut self,
        current: ArchetypeId,
        add: ComponentId,
    ) -> ArchetypeId {
        // Кешированный edge
        if let Some(&id) = self.archetypes[current.0 as usize].add_edges.get(&add) {
            return id;
        }

        let mut new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids
            .iter()
            .copied()
            .collect();
        new_components.push(add);
        new_components.sort();

        let new_id = self.get_or_create_archetype(new_components);

        self.archetypes[current.0 as usize]
            .add_edges
            .insert(add, new_id);
        self.archetypes[new_id.0 as usize]
            .remove_edges
            .insert(add, current);

        new_id
    }

    fn find_or_create_archetype_without(
        &mut self,
        current: ArchetypeId,
        remove: ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize]
            .remove_edges
            .get(&remove)
        {
            return id;
        }

        let new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids
            .iter()
            .copied()
            .filter(|&id| id != remove)
            .collect();

        let new_id = self.get_or_create_archetype(new_components);

        self.archetypes[current.0 as usize]
            .remove_edges
            .insert(remove, new_id);
        self.archetypes[new_id.0 as usize]
            .add_edges
            .insert(remove, current);

        new_id
    }

    fn get_or_create_archetype(&mut self, components: Vec<ComponentId>) -> ArchetypeId {
        if let Some(&id) = self.archetype_index.get(&components) {
            return id;
        }

        let id = ArchetypeId(self.archetypes.len() as u32);

        let infos: Vec<&ComponentInfo> = components
            .iter()
            .filter_map(|&cid| self.registry.get_info(cid))
            .collect();

        let archetype = Archetype::new(
            id,
            components.iter().copied().collect(),
            &infos,
        );

        self.archetypes.push(archetype);
        self.archetype_index.insert(components, id);

        id
    }

    /// Переместить данные entity из одного архетипа в другой
    /// Возвращает новый row
    fn move_entity(
        &mut self,
        entity: Entity,
        from_location: EntityLocation,
        to_archetype_id: ArchetypeId,
    ) -> usize {
        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx = to_archetype_id.0 as usize;
        let from_row = from_location.row;

        // Новая строка в целевом архетипе
        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        // Находим общие компоненты
        let common: Vec<ComponentId> = self.archetypes[from_idx]
            .component_ids
            .iter()
            .filter(|id| self.archetypes[to_idx].has_component(**id))
            .copied()
            .collect();

        // Копируем общие компоненты из from → to
        for comp_id in &common {
            let from_col = self.archetypes[from_idx]
                .column_index(*comp_id)
                .unwrap();
            let to_col = self.archetypes[to_idx]
                .column_index(*comp_id)
                .unwrap();

            unsafe {
                let item_size = self.archetypes[from_idx].columns[from_col].item_size;

                if item_size > 0 {
                    // Убедимся что в to колонке есть место
                    if self.archetypes[to_idx].columns[to_col].len >= 
                       self.archetypes[to_idx].columns[to_col].capacity 
                    {
                        self.archetypes[to_idx].columns[to_col].grow();
                    }

                    let src = self.archetypes[from_idx].columns[from_col].get_ptr(from_row);
                    let dst = self.archetypes[to_idx].columns[to_col].get_ptr(to_row);
                    std::ptr::copy_nonoverlapping(src, dst, item_size);
                }

                self.archetypes[to_idx].columns[to_col].len += 1;
            }
        }

        // Удаляем из from без drop общих компонентов (они переехали)
        // Для компонента который удаляется (если remove операция) — нужен drop
        // Здесь делаем swap_remove_no_drop для общих
        unsafe {
            let from_last = self.archetypes[from_idx].entities.len() - 1;

            for col in &mut self.archetypes[from_idx].columns {
                if common.contains(&col.component_id) {
                    // Данные переехали — без drop
                    col.swap_remove_no_drop(from_row);
                } else {
                    // Компонент не переезжает (удаляется) — с drop
                    if col.len > from_row {
                        col.swap_remove_and_drop(from_row);
                    }
                }
            }

            if from_row != from_last {
                let displaced = self.archetypes[from_idx].entities[from_last];
                self.archetypes[from_idx].entities.swap(from_row, from_last);
                self.archetypes[from_idx].entities.pop();

                // Обновляем location вытесненного entity
                let displaced_loc = EntityLocation {
                    archetype_id: from_location.archetype_id,
                    row: from_row,
                };
                self.entity_locations
                    .insert(displaced.index, displaced_loc);
                self.entities.set_location(displaced, displaced_loc);
            } else {
                self.archetypes[from_idx].entities.pop();
            }
        }

        to_row
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder для удобного создания entity
pub struct EntityBuilder<'w> {
    world: &'w mut World,
    entity: Entity,
}

impl<'w> EntityBuilder<'w> {
    pub fn insert<T: Component>(self, component: T) -> Self {
        self.world.insert(self.entity, component);
        self
    }

    pub fn id(self) -> Entity {
        self.entity
    }
}