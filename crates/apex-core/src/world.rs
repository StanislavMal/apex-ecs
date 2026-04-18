use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    archetype::{Archetype, ArchetypeId},
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry, Tick},
    entity::{EntityAllocator, EntityLocation, Entity},
    query::{Query, QueryBuilder, WorldQuery},
};

// ── QueryCache ─────────────────────────────────────────────────

/// Кеш matching архетипов для конкретного набора ComponentId.
/// Хранится в World и инвалидируется при добавлении нового архетипа.
///
/// Это устраняет O(archetypes) scan при каждом Query::new в hot loop.
#[derive(Default)]
#[allow(dead_code)]
pub(crate) struct QueryCache {
    /// key: sorted Vec<ComponentId> запроса → список matching arch indices
    entries: FxHashMap<Vec<ComponentId>, CacheEntry>,
    /// Версия кеша — инкрементируется при добавлении нового архетипа
    version: u32,
}

struct CacheEntry {
    arch_indices: Vec<usize>,
    /// Версия World на момент заполнения
    version: u32,
}

impl QueryCache {
    pub fn get_or_insert(
        &mut self,
        key: Vec<ComponentId>,
        world_version: u32,
        archetypes: &[Archetype],
        matches: impl Fn(&Archetype) -> bool,
    ) -> &[usize] {
        let entry = self.entries.entry(key).or_insert(CacheEntry {
            arch_indices: Vec::new(),
            version: u32::MAX,
        });

        if entry.version != world_version {
            entry.arch_indices = archetypes
                .iter()
                .enumerate()
                .filter(|(_, arch)| !arch.is_empty() && matches(arch))
                .map(|(i, _)| i)
                .collect();
            entry.version = world_version;
        }

        &entry.arch_indices
    }

    pub fn invalidate(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn version(&self) -> u32 {
        self.version
    }
}

// ── World ──────────────────────────────────────────────────────

pub struct World {
    pub(crate) entities: EntityAllocator,
    pub(crate) registry: ComponentRegistry,
    pub(crate) archetypes: Vec<Archetype>,
    pub(crate) archetype_index: FxHashMap<Vec<ComponentId>, ArchetypeId>,
    /// Текущий тик — инкрементируется через World::tick()
    current_tick: Tick,
    /// Кеш matching архетипов
    pub(crate) query_cache: QueryCache,
}

impl World {
    pub fn new() -> Self {
        let mut world = Self {
            entities: EntityAllocator::new(),
            registry: ComponentRegistry::new(),
            archetypes: Vec::new(),
            archetype_index: FxHashMap::default(),
            current_tick: Tick(1),
            query_cache: QueryCache::default(),
        };
        world.archetypes.push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world.archetype_index.insert(Vec::new(), ArchetypeId::EMPTY);
        world
    }

    /// Инкрементировать тик мира. Вызывать раз в кадр.
    /// После этого Changed<T> будет видеть только изменения текущего кадра.
    pub fn tick(&mut self) {
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
    }

    pub fn current_tick(&self) -> Tick {
        self.current_tick
    }

    pub fn register_component<T: Component>(&mut self) -> ComponentId {
        self.registry.register::<T>()
    }

    // ── Spawn ──────────────────────────────────────────────────

    pub fn spawn(&mut self) -> EntityBuilder<'_> {
        let entity = self.entities.allocate();
        let row = unsafe { self.archetypes[0].allocate_row(entity) };
        self.entities.set_location(entity, EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        });
        EntityBuilder { world: self, entity }
    }

    pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Entity {
        let ids = bundle.component_ids(&mut self.registry);
        let archetype_id = self.get_or_create_archetype(ids);
        let entity = self.entities.allocate();
        let row = self.archetypes[archetype_id.0 as usize].entities.len();
        self.archetypes[archetype_id.0 as usize].entities.push(entity);
        let tick = self.current_tick;
        bundle.write_into(self, archetype_id, row, tick);
        self.entities.set_location(entity, EntityLocation { archetype_id, row });
        entity
    }

    // ── Component ops ──────────────────────────────────────────

    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) {
        let component_id = self.registry.get_or_register::<T>();
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return,
        };
        let current_idx = location.archetype_id.0 as usize;

        if self.archetypes[current_idx].has_component(component_id) {
            let tick = self.current_tick;
            unsafe {
                if let Some(col_idx) = self.archetypes[current_idx].column_index(component_id) {
                    let col = &mut self.archetypes[current_idx].columns[col_idx];
                    col.write_at(location.row, &component as *const T as *const u8, tick);
                }
            }
            std::mem::forget(component);
            return;
        }

        let new_archetype_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_archetype_id);
        let tick = self.current_tick;
        unsafe {
            self.archetypes[new_archetype_id.0 as usize]
                .write_component(new_row, component_id, &component as *const T as *const u8, tick);
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
        if !self.entities.is_alive(entity) { return false; }
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
        let tick = self.current_tick;
        let arch = &mut self.archetypes[location.archetype_id.0 as usize];
        let col_idx = arch.column_index(component_id)?;
        // Обновляем тик изменения
        if location.row < arch.columns[col_idx].change_ticks.len() {
            arch.columns[col_idx].change_ticks[location.row] = tick;
        }
        unsafe { Some(arch.columns[col_idx].get_mut::<T>(location.row)) }
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.entities.is_alive(entity)
    }

    // ── Query API ──────────────────────────────────────────────

    /// Создать typed zero-cost query
    pub fn query_typed<Q: WorldQuery>(&self) -> Query<'_, Q> {
        Query::new(self)
    }

    /// Typed query с change detection (last_run = предыдущий тик)
    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> Query<'_, Q> {
        Query::new_with_tick(self, last_run)
    }

    /// Legacy QueryBuilder
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

    fn find_or_create_archetype_with(&mut self, current: ArchetypeId, add: ComponentId) -> ArchetypeId {
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

    fn find_or_create_archetype_without(&mut self, current: ArchetypeId, remove: ComponentId) -> ArchetypeId {
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
        // Инвалидируем кеш — появился новый архетип
        self.query_cache.invalidate();
        id
    }

    fn move_entity(
        &mut self,
        entity: Entity,
        from_location: EntityLocation,
        to_archetype_id: ArchetypeId,
    ) -> usize {
        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx = to_archetype_id.0 as usize;
        let from_row = from_location.row;

        let from_len = self.archetypes[from_idx].columns.len();
        let mut is_common: SmallVec<[bool; 32]> = SmallVec::from_elem(false, from_len);
        for i in 0..from_len {
            let cid = self.archetypes[from_idx].columns[i].component_id;
            is_common[i] = self.archetypes[to_idx].has_component(cid);
        }

        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        for i in 0..from_len {
            if !is_common[i] { continue; }
            let cid = self.archetypes[from_idx].columns[i].component_id;
            let to_col = self.archetypes[to_idx].column_index(cid).unwrap();

            unsafe {
                let item_size = self.archetypes[from_idx].columns[i].item_size;
                if item_size > 0 {
                    if self.archetypes[to_idx].columns[to_col].len
                        >= self.archetypes[to_idx].columns[to_col].capacity
                    {
                        self.archetypes[to_idx].columns[to_col].grow();
                    }
                    let src = self.archetypes[from_idx].columns[i].get_ptr(from_row);
                    let dst = self.archetypes[to_idx].columns[to_col].get_ptr(to_row);
                    std::ptr::copy_nonoverlapping(src, dst, item_size);
                }
                // Переносим тик изменения вместе с данными
                let src_tick = self.archetypes[from_idx].columns[i].get_tick(from_row);
                self.archetypes[to_idx].columns[to_col].change_ticks.push(src_tick);
                self.archetypes[to_idx].columns[to_col].len += 1;
            }
        }

        unsafe {
            let from_last = self.archetypes[from_idx].entities.len() - 1;
            for (i, col) in self.archetypes[from_idx].columns.iter_mut().enumerate() {
                if is_common[i] {
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

pub trait Bundle: Sized {
    fn component_ids(&self, registry: &mut ComponentRegistry) -> Vec<ComponentId>;
    fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick);
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

            fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick) {
                let ($($T,)+) = self;
                $(
                    {
                        let cid = world.registry.get_or_register::<$T>();
                        if let Some(col_idx) = world.archetypes[archetype_id.0 as usize].column_index(cid) {
                            unsafe {
                                let col = &mut world.archetypes[archetype_id.0 as usize].columns[col_idx];
                                if col.item_size > 0 {
                                    if col.len >= col.capacity { col.grow(); }
                                    let dst = col.get_ptr(row);
                                    std::ptr::copy_nonoverlapping(
                                        &$T as *const $T as *const u8,
                                        dst,
                                        col.item_size,
                                    );
                                }
                                col.change_ticks.push(tick);
                                col.len += 1;
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
