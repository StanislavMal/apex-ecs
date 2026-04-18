use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::{
    archetype::{Archetype, ArchetypeId},
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry},
    entity::{EntityAllocator, EntityLocation, Entity},
    query::QueryBuilder,
};

/// Мир — центральное хранилище всего состояния
pub struct World {
    pub(crate) entities: EntityAllocator,
    pub(crate) registry: ComponentRegistry,
    pub(crate) archetypes: Vec<Archetype>,
    pub(crate) archetype_index: FxHashMap<Vec<ComponentId>, ArchetypeId>,
}

impl World {
    pub fn new() -> Self {
        let mut world = Self {
            entities: EntityAllocator::new(),
            registry: ComponentRegistry::new(),
            archetypes: Vec::new(),
            archetype_index: FxHashMap::default(),
        };
        // Пустой архетип (entity без компонентов)
        world.archetypes.push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world.archetype_index.insert(Vec::new(), ArchetypeId::EMPTY);
        world
    }

    pub fn register_component<T: Component>(&mut self) -> ComponentId {
        self.registry.register::<T>()
    }

    // ── Spawn ──────────────────────────────────────────────────

    /// Создать entity без компонентов, вернуть builder
    pub fn spawn(&mut self) -> EntityBuilder<'_> {
        let entity = self.entities.allocate();
        let row = unsafe { self.archetypes[0].allocate_row(entity) };
        self.entities.set_location(entity, EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        });
        EntityBuilder { world: self, entity }
    }

    /// Создать entity из Bundle — один архетипный переход, без промежуточных архетипов
    pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Entity {
        let ids = bundle.component_ids(&mut self.registry);
        let archetype_id = self.get_or_create_archetype(ids.clone());

        let entity = self.entities.allocate();
        let row = self.archetypes[archetype_id.0 as usize].entities.len();
        self.archetypes[archetype_id.0 as usize].entities.push(entity);

        // Выделяем место в колонках
        for &cid in &ids {
            let col_idx = self.archetypes[archetype_id.0 as usize].column_index(cid).unwrap();
            let col = &mut self.archetypes[archetype_id.0 as usize].columns[col_idx];
            if col.len >= col.capacity {
                col.grow();
            }
            if col.item_size > 0 {
                col.len += 1; // место зарезервировано, данные запишет bundle
            } else {
                col.len += 1;
            }
        }

        // Записываем данные компонентов
        bundle.write_components(self, archetype_id, row);

        self.entities.set_location(entity, EntityLocation { archetype_id, row });
        entity
    }

    // ── Component ops ──────────────────────────────────────────

    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) {
        let component_id = self.registry.get_or_register::<T>();
        self.insert_erased(entity, component_id, component);
    }

    fn insert_erased<T: Component>(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        component: T,
    ) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return,
        };

        let current_idx = location.archetype_id.0 as usize;

        // Уже есть — просто обновляем значение
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

        let new_archetype_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_archetype_id);

        unsafe {
            let src = &component as *const T as *const u8;
            self.archetypes[new_archetype_id.0 as usize]
                .write_component(new_row, component_id, src);
        }
        std::mem::forget(component);

        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_archetype_id,
            row: new_row,
        });
    }

    pub fn remove<T: Component>(&mut self, entity: Entity) -> bool {
        let component_id = match self.registry.get_id::<T>() {
            Some(id) => id,
            None => return false,
        };
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return false,
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return false;
        }

        let new_archetype_id = self.find_or_create_archetype_without(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_archetype_id);

        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_archetype_id,
            row: new_row,
        });
        true
    }

    pub fn despawn(&mut self, entity: Entity) -> bool {
        if !self.entities.is_alive(entity) {
            return false;
        }
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return false,
        };

        let arch_idx = location.archetype_id.0 as usize;
        unsafe {
            if let Some(displaced) = self.archetypes[arch_idx].remove_row(location.row) {
                self.entities.set_location(displaced, EntityLocation {
                    archetype_id: location.archetype_id,
                    row: location.row,
                });
            }
        }
        self.entities.free(entity);
        true
    }

    // ── Read / Write ───────────────────────────────────────────

    #[inline]
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let component_id = self.registry.get_id::<T>()?;
        let location = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component::<T>(location.row, component_id)
        }
    }

    #[inline]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let component_id = self.registry.get_id::<T>()?;
        let location = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component_mut::<T>(location.row, component_id)
        }
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.entities.is_alive(entity)
    }

    pub fn query(&self) -> QueryBuilder<'_> {
        QueryBuilder::new(self)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
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
        if let Some(&id) = self.archetypes[current.0 as usize].add_edges.get(&add) {
            return id;
        }
        let mut new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids.iter().copied().collect();
        new_components.push(add);
        new_components.sort_unstable();

        let new_id = self.get_or_create_archetype(new_components);
        self.archetypes[current.0 as usize].add_edges.insert(add, new_id);
        self.archetypes[new_id.0 as usize].remove_edges.insert(add, current);
        new_id
    }

    fn find_or_create_archetype_without(
        &mut self,
        current: ArchetypeId,
        remove: ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize].remove_edges.get(&remove) {
            return id;
        }
        let new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids.iter().copied().filter(|&id| id != remove).collect();

        let new_id = self.get_or_create_archetype(new_components);
        self.archetypes[current.0 as usize].remove_edges.insert(remove, new_id);
        self.archetypes[new_id.0 as usize].add_edges.insert(remove, current);
        new_id
    }

    pub(crate) fn get_or_create_archetype(&mut self, components: Vec<ComponentId>) -> ArchetypeId {
        if let Some(&id) = self.archetype_index.get(&components) {
            return id;
        }
        let id = ArchetypeId(self.archetypes.len() as u32);
        let infos: Vec<&ComponentInfo> = components
            .iter()
            .filter_map(|&cid| self.registry.get_info(cid))
            .collect();
        self.archetypes.push(Archetype::new(id, components.iter().copied().collect(), &infos));
        self.archetype_index.insert(components, id);
        id
    }

    /// Переместить entity из одного архетипа в другой, вернуть новый row.
    fn move_entity(
        &mut self,
        entity: Entity,
        from_location: EntityLocation,
        to_archetype_id: ArchetypeId,
    ) -> usize {
        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx = to_archetype_id.0 as usize;
        let from_row = from_location.row;

        // Общие компоненты — O(n) один раз, результат в HashSet для O(1) lookup ниже
        let common: FxHashSet<ComponentId> = self.archetypes[from_idx]
            .component_ids.iter()
            .filter(|id| self.archetypes[to_idx].has_component(**id))
            .copied()
            .collect();

        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        // Копируем общие компоненты from → to
        for &comp_id in &common {
            let from_col = self.archetypes[from_idx].column_index(comp_id).unwrap();
            let to_col = self.archetypes[to_idx].column_index(comp_id).unwrap();

            unsafe {
                let item_size = self.archetypes[from_idx].columns[from_col].item_size;
                if item_size > 0 {
                    if self.archetypes[to_idx].columns[to_col].len
                        >= self.archetypes[to_idx].columns[to_col].capacity
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

        // Удаляем из from
        unsafe {
            let from_last = self.archetypes[from_idx].entities.len() - 1;

            for col in &mut self.archetypes[from_idx].columns {
                if common.contains(&col.component_id) {
                    col.swap_remove_no_drop(from_row);
                } else {
                    col.swap_remove_and_drop(from_row);
                }
            }

            if from_row != from_last {
                let displaced = self.archetypes[from_idx].entities[from_last];
                self.archetypes[from_idx].entities.swap(from_row, from_last);
                self.archetypes[from_idx].entities.pop();
                self.entities.set_location(displaced, EntityLocation {
                    archetype_id: from_location.archetype_id,
                    row: from_row,
                });
            } else {
                self.archetypes[from_idx].entities.pop();
            }
        }

        to_row
    }
}

impl Default for World {
    fn default() -> Self { Self::new() }
}

// ── Bundle ─────────────────────────────────────────────────────

/// Набор компонентов для атомарного spawn без промежуточных архетипов.
///
/// Реализован вручную для кортежей. Макрос расширяет до (A,), (A,B), ..., (A,B,C,D,E,F,G,H).
pub trait Bundle: Sized {
    fn component_ids(self: &Self, registry: &mut ComponentRegistry) -> Vec<ComponentId>;
    fn write_components(self, world: &mut World, archetype_id: ArchetypeId, row: usize);
}

macro_rules! impl_bundle {
    ($($T:ident),+) => {
        #[allow(non_snake_case)]
        impl<$($T: Component),+> Bundle for ($($T,)+) {
            fn component_ids(&self, registry: &mut ComponentRegistry) -> Vec<ComponentId> {
                let mut ids = vec![$( registry.get_or_register::<$T>() ),+];
                ids.sort_unstable();
                ids
            }

            fn write_components(self, world: &mut World, archetype_id: ArchetypeId, row: usize) {
                let ($($T,)+) = self;
                $(
                    {
                        let cid = world.registry.get_or_register::<$T>();
                        unsafe {
                            let arch = &mut world.archetypes[archetype_id.0 as usize];
                            if let Some(col_idx) = arch.column_index(cid) {
                                let col = &mut arch.columns[col_idx];
                                // row уже зарезервирован в spawn_bundle, перезаписываем
                                if col.item_size > 0 {
                                    let dst = col.get_ptr(row);
                                    std::ptr::copy_nonoverlapping(
                                        &$T as *const $T as *const u8,
                                        dst,
                                        col.item_size,
                                    );
                                }
                            }
                        }
                        std::mem::forget($T);
                    }
                )+
            }
        }
    };
}

impl_bundle!(A);
impl_bundle!(A, B);
impl_bundle!(A, B, C);
impl_bundle!(A, B, C, D);
impl_bundle!(A, B, C, D, E);
impl_bundle!(A, B, C, D, E, F);
impl_bundle!(A, B, C, D, E, F, G);
impl_bundle!(A, B, C, D, E, F, G, H);

// ── EntityBuilder ──────────────────────────────────────────────

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
