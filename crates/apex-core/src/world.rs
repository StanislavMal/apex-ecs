use std::cell::UnsafeCell;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    archetype::{Archetype, ArchetypeId},
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry, Tick, Serializable},
    entity::{EntityAllocator, EntityLocation, Entity},
    events::EventRegistry,
    query::{QueryBuilder, WorldQuery},
    relations::{IdIndex, RelationRegistry, SubjectIndex},
    resources::ResourceMap,
    system_param::{Res, ResMut, EventReader, EventWriter, WorldQuerySystemAccess},
};

// ── QueryCache ─────────────────────────────────────────────────

struct CacheEntry {
    arch_indices: Vec<usize>,
    version:      u32,
}

pub(crate) struct QueryCache {
    entries: UnsafeCell<FxHashMap<Vec<ComponentId>, CacheEntry>>,
    version: u32,
}

unsafe impl Sync for QueryCache {}

impl QueryCache {
    pub fn new() -> Self {
        Self { entries: UnsafeCell::new(FxHashMap::default()), version: 0 }
    }

    pub unsafe fn get_or_compute(
        &self,
        key:           &[ComponentId],
        world_version: u32,
        archetypes:    &[Archetype],
        matches:       impl Fn(&Archetype) -> bool,
    ) -> &[usize] {
        let map   = &mut *self.entries.get();
        let entry = map.entry(key.to_vec()).or_insert(CacheEntry {
            arch_indices: Vec::new(),
            version:      u32::MAX,
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

    pub fn invalidate(&mut self) { self.version = self.version.wrapping_add(1); }
    pub fn version(&self) -> u32 { self.version }
}

// ── DeferredCommand (deferred structural changes) ──────────────

/// Отложенные structural changes — накапливаются во время итерации,
/// применяются batch'ем после завершения системы.
///
/// Это устраняет необходимость прерывать итерацию для каждого insert/remove.
enum DeferredCommand {
    Despawn(Entity),
    InsertRaw {
        entity:       Entity,
        component_id: ComponentId,
        data:         Vec<u8>,
        tick:         Tick,
    },
    RemoveRaw {
        entity:       Entity,
        component_id: ComponentId,
    },
}

/// Очередь отложенных команд.
///
/// Используется когда нужно избежать borrow conflicts во время итерации.
/// `apply` выполняет все команды за один проход.
pub struct DeferredQueue {
    commands: Vec<DeferredCommand>,
}

impl DeferredQueue {
    pub fn new() -> Self { Self { commands: Vec::new() } }
    pub fn with_capacity(cap: usize) -> Self { Self { commands: Vec::with_capacity(cap) } }

    pub fn despawn(&mut self, entity: Entity) {
        self.commands.push(DeferredCommand::Despawn(entity));
    }

    pub fn insert_raw(
        &mut self,
        entity:       Entity,
        component_id: ComponentId,
        data:         Vec<u8>,
        tick:         Tick,
    ) {
        self.commands.push(DeferredCommand::InsertRaw { entity, component_id, data, tick });
    }

    pub fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        self.commands.push(DeferredCommand::RemoveRaw { entity, component_id });
    }

    pub fn len(&self) -> usize { self.commands.len() }
    pub fn is_empty(&self) -> bool { self.commands.is_empty() }

    /// Применить все отложенные команды к миру за один проход.
    pub fn apply(&mut self, world: &mut World) {
        for cmd in self.commands.drain(..) {
            match cmd {
                DeferredCommand::Despawn(e) => { world.despawn(e); }
                DeferredCommand::InsertRaw { entity, component_id, data, tick } => {
                    world.insert_raw(entity, component_id, data, tick);
                }
                DeferredCommand::RemoveRaw { entity, component_id } => {
                    world.remove_raw(entity, component_id);
                }
            }
        }
    }

    pub fn clear(&mut self) { self.commands.clear(); }
}

impl Default for DeferredQueue {
    fn default() -> Self { Self::new() }
}

// ── World ──────────────────────────────────────────────────────

pub struct World {
    pub(crate) entities:        EntityAllocator,
    pub(crate) registry:        ComponentRegistry,
    pub(crate) archetypes:      Vec<Archetype>,
    pub(crate) archetype_index: FxHashMap<Vec<ComponentId>, ArchetypeId>,
    pub(crate) current_tick:    Tick,
    pub(crate) query_cache:     QueryCache,
    pub(crate) relations:       RelationRegistry,
    pub(crate) id_index:        IdIndex,
    pub(crate) subject_index:   SubjectIndex,
    pub        resources:       ResourceMap,
    pub(crate) events:          EventRegistry,
}

impl World {
    pub fn new() -> Self {
        let mut world = Self {
            entities:        EntityAllocator::new(),
            registry:        ComponentRegistry::new(),
            archetypes:      Vec::new(),
            archetype_index: FxHashMap::default(),
            current_tick:    Tick(1),
            query_cache:     QueryCache::new(),
            relations:       RelationRegistry::new(),
            id_index:        IdIndex::default(),
            subject_index:   SubjectIndex::new(),
            resources:       ResourceMap::new(),
            events:          EventRegistry::new(),
        };
        world.archetypes.push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world.archetype_index.insert(Vec::new(), ArchetypeId::EMPTY);
        world
    }

    pub fn tick(&mut self) {
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
        self.events.update_all();
    }

    pub fn current_tick(&self)    -> Tick  { self.current_tick }
    pub fn entity_count(&self)    -> usize { self.entities.len() }
    pub fn archetype_count(&self) -> usize { self.archetypes.len() }
    pub fn resource_count(&self)  -> usize { self.resources.len() }

    pub fn register_component<T: Component>(&mut self) -> ComponentId {
        self.registry.register::<T>()
    }

    pub fn register_component_serde<T: crate::component::Serializable>(&mut self) -> ComponentId {
        self.registry.register_serde::<T>()
    }

    pub fn registry(&self) -> &ComponentRegistry { &self.registry }

    pub fn archetypes(&self) -> &[Archetype] { &self.archetypes }

    pub fn relation_registry(&self) -> &RelationRegistry { &self.relations }

    pub fn relation_registry_mut(&mut self) -> &mut RelationRegistry { &mut self.relations }

    pub fn subject_index_raw(&self, entity_index: u32) -> &[u32] {
        self.subject_index.get_all(entity_index)
    }

    pub fn spawn_empty(&mut self) -> Entity {
        let entity = self.entities.allocate();
        let row    = unsafe { self.archetypes[0].allocate_row(entity) };
        self.entities.set_location(entity, EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        });
        entity
    }

    pub fn insert_relation_raw(&mut self, subject: Entity, relation_id: ComponentId, _target: Entity) {
        self.ensure_relation_component(relation_id);
        self.subject_index.add(subject.index, relation_id);
        self.insert_relation_component(subject, relation_id);
    }

    /// Публичная обёртка над pub(crate) insert_raw — для apex-serialization.
    ///
    /// Вставить raw байты компонента в entity. Используется при restore
    /// когда тип компонента неизвестен статически.
    #[inline]
    pub fn insert_raw_pub(
        &mut self,
        entity:       Entity,
        component_id: ComponentId,
        data:         Vec<u8>,
        tick:         Tick,
    ) {
        self.insert_raw(entity, component_id, data, tick);
    }

    // ── Параллельный доступ ────────────────────────────────────

    /// # Safety
    /// Вызывающий гарантирует отсутствие structural changes
    /// и корректность AccessDescriptor всех параллельных систем.
    pub unsafe fn as_parallel_world(&self) -> ParallelWorld<'_> {
        ParallelWorld {
            world:   self as *const World,
            _marker: std::marker::PhantomData,
        }
    }

    pub(crate) unsafe fn archetype_ptr(&self, idx: usize) -> *mut Archetype {
        &self.archetypes[idx] as *const Archetype as *mut Archetype
    }

    // ── Resources ──────────────────────────────────────────────

    pub fn insert_resource<T: Send + Sync + 'static>(&mut self, value: T) {
        self.resources.insert(value);
    }

    #[track_caller]
    pub fn resource<T: Send + Sync + 'static>(&self) -> &T {
        self.resources.get::<T>()
    }

    #[track_caller]
    pub fn resource_mut<T: Send + Sync + 'static>(&mut self) -> &mut T {
        self.resources.get_mut::<T>()
    }

    pub fn try_resource<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.resources.try_get::<T>()
    }

    pub fn try_resource_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.resources.try_get_mut::<T>()
    }

    pub fn remove_resource<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.resources.remove::<T>()
    }

    pub fn has_resource<T: Send + Sync + 'static>(&self) -> bool {
        self.resources.contains::<T>()
    }

    // ── Events ─────────────────────────────────────────────────

    pub fn add_event<T: Send + Sync + 'static>(&mut self) {
        self.events.register::<T>();
    }

    #[track_caller]
    pub fn events<T: Send + Sync + 'static>(&self) -> &crate::events::EventQueue<T> {
        self.events.get::<T>()
    }

    #[track_caller]
    pub fn events_mut<T: Send + Sync + 'static>(&mut self) -> &mut crate::events::EventQueue<T> {
        self.events.get_mut::<T>()
    }

    #[track_caller]
    pub fn send_event<T: Send + Sync + 'static>(&mut self, event: T) {
        self.events.get_mut::<T>().send(event);
    }

    pub fn event_queue_ptr<T: Send + Sync + 'static>(
        &self,
    ) -> Option<*mut crate::events::EventQueue<T>> {
        self.events.get_raw_ptr::<T>()
    }

    // ── Spawn ──────────────────────────────────────────────────

    pub fn spawn(&mut self) -> EntityBuilder<'_> {
        let entity = self.entities.allocate();
        let row    = unsafe { self.archetypes[0].allocate_row(entity) };
        self.entities.set_location(entity, EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row,
        });
        EntityBuilder { world: self, entity }
    }

    pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Entity {
        let ids          = bundle.component_ids(&mut self.registry);
        let archetype_id = self.get_or_create_archetype(ids);
        let entity       = self.entities.allocate();
        let row          = self.archetypes[archetype_id.0 as usize].entities.len();
        let tick         = self.current_tick;
        self.archetypes[archetype_id.0 as usize].entities.push(entity);
        bundle.write_into(self, archetype_id, row, tick);
        self.entities.set_location(entity, EntityLocation { archetype_id, row });
        entity
    }

    pub fn spawn_many<B, F>(&mut self, count: usize, mut make_bundle: F) -> Vec<Entity>
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        if count == 0 { return Vec::new(); }

        let probe        = make_bundle(0);
        let ids          = probe.component_ids(&mut self.registry);
        drop(probe);

        let archetype_id = self.get_or_create_archetype(ids);
        let arch_idx     = archetype_id.0 as usize;
        let start_row    = self.archetypes[arch_idx].entities.len();
        let tick         = self.current_tick;

        self.archetypes[arch_idx].entities.reserve(count);
        let target_cap = start_row + count;
        for col in &mut self.archetypes[arch_idx].columns {
            while col.capacity < target_cap { col.grow(); }
        }

        let entities = self.entities.allocate_batch(count);

        for (i, &entity) in entities.iter().enumerate() {
            let row    = start_row + i;
            let bundle = make_bundle(i);
            self.archetypes[arch_idx].entities.push(entity);
            bundle.write_into(self, archetype_id, row, tick);
        }

        self.entities.set_locations_batch(&entities, archetype_id, start_row);
        entities
    }

    pub fn spawn_many_silent<B, F>(&mut self, count: usize, mut make_bundle: F)
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        if count == 0 { return; }

        let probe        = make_bundle(0);
        let ids          = probe.component_ids(&mut self.registry);
        drop(probe);

        let archetype_id = self.get_or_create_archetype(ids);
        let arch_idx     = archetype_id.0 as usize;
        let start_row    = self.archetypes[arch_idx].entities.len();
        let tick         = self.current_tick;

        self.archetypes[arch_idx].entities.reserve(count);
        let target_cap = start_row + count;
        for col in &mut self.archetypes[arch_idx].columns {
            while col.capacity < target_cap { col.grow(); }
        }

        let entities = self.entities.allocate_batch(count);

        for (i, &entity) in entities.iter().enumerate() {
            let row    = start_row + i;
            let bundle = make_bundle(i);
            self.archetypes[arch_idx].entities.push(entity);
            bundle.write_into(self, archetype_id, row, tick);
        }

        self.entities.set_locations_batch(&entities, archetype_id, start_row);
    }

    // ── Component ops ──────────────────────────────────────────

    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) {
        let component_id = self.registry.get_or_register::<T>();
        let location     = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return,
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

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row     = self.move_entity(entity, location, new_arch_id);
        let tick        = self.current_tick;
        unsafe {
            self.archetypes[new_arch_id.0 as usize]
                .write_component(new_row, component_id, &component as *const T as *const u8, tick);
        }
        std::mem::forget(component);
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row,
        });
    }

    /// Вставить компонент по raw данным — используется DeferredQueue.
    pub(crate) fn insert_raw(
        &mut self,
        entity:       Entity,
        component_id: ComponentId,
        data:         Vec<u8>,
        tick:         Tick,
    ) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return,
        };
        let current_idx = location.archetype_id.0 as usize;

        if self.archetypes[current_idx].has_component(component_id) {
            if !data.is_empty() {
                unsafe {
                    if let Some(col_idx) = self.archetypes[current_idx].column_index(component_id) {
                        let col = &mut self.archetypes[current_idx].columns[col_idx];
                        col.write_at(location.row, data.as_ptr(), tick);
                    }
                }
            }
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row     = self.move_entity(entity, location, new_arch_id);
        if !data.is_empty() {
            unsafe {
                self.archetypes[new_arch_id.0 as usize]
                    .write_component(new_row, component_id, data.as_ptr(), tick);
            }
        }
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row,
        });
    }

    /// Удалить компонент по raw ComponentId — используется DeferredQueue.
    pub(crate) fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return,
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return;
        }
        let new_arch_id = self.find_or_create_archetype_without(
            location.archetype_id,
            component_id,
        );
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row,
        });
    }

    pub fn remove<T: Component>(&mut self, entity: Entity) -> bool {
        let component_id = match self.registry.get_id::<T>() {
            Some(id) => id,
            None     => return false,
        };
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return false,
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return false;
        }
        let new_arch_id = self.find_or_create_archetype_without(
            location.archetype_id,
            component_id,
        );
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row,
        });
        true
    }

    pub fn despawn(&mut self, entity: Entity) -> bool {
        if !self.entities.is_alive(entity) { return false; }
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None      => return false,
        };
        self.subject_index.clear_entity(entity.index);
        let arch_idx = location.archetype_id.0 as usize;
        unsafe {
            if let Some(displaced) = self.archetypes[arch_idx].remove_row(location.row) {
                self.entities.set_location(displaced, EntityLocation {
                    archetype_id: location.archetype_id,
                    row:          location.row,
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
        let location     = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component::<T>(location.row, component_id)
        }
    }

    #[inline]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let component_id = self.registry.get_id::<T>()?;
        let location     = self.entities.get_location(entity)?;
        let tick         = self.current_tick;
        let arch         = &mut self.archetypes[location.archetype_id.0 as usize];
        let col_idx      = arch.column_index(component_id)?;
        if location.row < arch.columns[col_idx].change_ticks.len() {
            arch.columns[col_idx].change_ticks[location.row] = tick;
        }
        unsafe { Some(arch.columns[col_idx].get_mut::<T>(location.row)) }
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool { self.entities.is_alive(entity) }

    // ── Query API ──────────────────────────────────────────────

    pub fn query_typed<Q: WorldQuery>(&self) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, Tick::ZERO)
    }

    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, last_run)
    }

    pub fn query(&self) -> QueryBuilder<'_> { QueryBuilder::new(self) }

    // ── Внутренние методы ──────────────────────────────────────

    pub(crate) fn find_or_create_archetype_with(
        &mut self,
        current: ArchetypeId,
        add:     ComponentId,
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

    pub(crate) fn find_or_create_archetype_without(
        &mut self,
        current: ArchetypeId,
        remove:  ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize].remove_edges.get(&remove) {
            return id;
        }
        let new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids.iter().copied()
            .filter(|&id| id != remove)
            .collect();
        let new_id = self.get_or_create_archetype(new_components);
        self.archetypes[current.0 as usize].remove_edges.insert(remove, new_id);
        self.archetypes[new_id.0 as usize].add_edges.insert(remove, current);
        new_id
    }

    pub(crate) fn get_or_create_archetype(
        &mut self,
        components: Vec<ComponentId>,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetype_index.get(&components) { return id; }
        let id    = ArchetypeId(self.archetypes.len() as u32);
        let infos: Vec<&ComponentInfo> = components.iter()
            .filter_map(|&cid| self.registry.get_info(cid))
            .collect();
        let arch  = Archetype::new(id, components.iter().copied().collect(), &infos);
        for &cid in &arch.component_ids { self.id_index.register_archetype(cid, id); }
        self.archetypes.push(arch);
        self.archetype_index.insert(components, id);
        self.query_cache.invalidate();
        id
    }

    pub(crate) fn move_entity(
        &mut self,
        entity:          Entity,
        from_location:   EntityLocation,
        to_archetype_id: ArchetypeId,
    ) -> usize {
        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx   = to_archetype_id.0 as usize;
        let from_row = from_location.row;

        let from_len = self.archetypes[from_idx].columns.len();
        let mut is_common: SmallVec<[bool; 32]> = SmallVec::from_elem(false, from_len);
        for i in 0..from_len {
            let cid      = self.archetypes[from_idx].columns[i].component_id;
            is_common[i] = self.archetypes[to_idx].has_component(cid);
        }

        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        for i in 0..from_len {
            if !is_common[i] { continue; }
            let cid    = self.archetypes[from_idx].columns[i].component_id;
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
                let src_tick = self.archetypes[from_idx].columns[i].get_tick(from_row);
                self.archetypes[to_idx].columns[to_col].change_ticks.push(src_tick);
                self.archetypes[to_idx].columns[to_col].len += 1;
            }
        }

        unsafe {
            let from_last = self.archetypes[from_idx].entities.len() - 1;
            for (i, col) in self.archetypes[from_idx].columns.iter_mut().enumerate() {
                if is_common[i] { col.swap_remove_no_drop(from_row); }
                else            { col.swap_remove_and_drop(from_row); }
            }
            if from_row != from_last {
                let displaced = self.archetypes[from_idx].entities[from_last];
                self.archetypes[from_idx].entities.swap(from_row, from_last);
                self.archetypes[from_idx].entities.pop();
                self.entities.set_location(displaced, EntityLocation {
                    archetype_id: from_location.archetype_id,
                    row:          from_row,
                });
            } else {
                self.archetypes[from_idx].entities.pop();
            }
        }
        to_row
    }
}

impl Default for World { fn default() -> Self { Self::new() } }

// ── SystemContext ──────────────────────────────────────────────

/// Размер чанка для par_for_each_component.
///
/// Разбивает архетип на блоки по N entity для параллельной обработки.
/// Слишком маленький → overhead rayon съедает выигрыш.
/// Слишком большой → мало задач, плохой load balancing.
///
/// 4096 — эмпирически оптимально для большинства компонентов.
pub const PAR_CHUNK_SIZE: usize = 4096;

pub struct SystemContext<'w> {
    pub(crate) world:   *const World,
    pub(crate) _marker: std::marker::PhantomData<&'w World>,
}

unsafe impl Send for SystemContext<'_> {}
unsafe impl Sync for SystemContext<'_> {}

impl<'w> SystemContext<'w> {
    pub fn new(world: &'w World) -> Self {
        Self { world: world as *const World, _marker: std::marker::PhantomData }
    }

    #[inline]
    pub fn query<Q: WorldQuery>(&self) -> crate::query::Query<'_, Q> {
        unsafe { crate::query::Query::new(&*self.world) }
    }

    #[inline]
    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> crate::query::Query<'_, Q> {
        unsafe { crate::query::Query::new_with_tick(&*self.world, last_run) }
    }

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(unsafe { (*self.world).resource::<T>() })
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = (*self.world)
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        EventReader(unsafe { (*self.world).events::<T>() })
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = (*self.world)
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered");
            EventWriter::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn entity_count(&self) -> usize {
        unsafe { (*self.world).entity_count() }
    }

    #[inline]
    pub fn for_each<Q, F>(&self, f: F)
    where
        Q: WorldQuery,
        F: FnMut(Entity, Q::Item<'_>),
    {
        self.query::<Q>().for_each(f);
    }

    #[inline]
    pub fn for_each_component<Q, F>(&self, f: F)
    where
        Q: WorldQuery,
        F: FnMut(Q::Item<'_>),
    {
        self.query::<Q>().for_each_component(f);
    }

    /// Параллельная итерация по компонентам с chunk-level параллелизмом.
    ///
    /// **Ключевое улучшение**: разбивает каждый архетип на чанки по `PAR_CHUNK_SIZE`
    /// entities и раздаёт их в Rayon work-stealing pool.
    ///
    /// Это даёт реальный speedup даже когда все entity в одном архетипе,
    /// в отличие от предыдущей версии которая параллелила только по архетипам.
    ///
    /// Реальный выигрыш когда:
    /// - Архетип содержит > PAR_CHUNK_SIZE entities
    /// - Вычисления CPU-bound (не memory-bandwidth bound)
    /// - Нет false sharing между чанками (каждый чанк = независимый диапазон строк)
    #[cfg(feature = "parallel")]
    pub fn par_for_each_component<Q, F>(&self, f: F)
    where
        Q: WorldQuery + Send,
        F: Fn(Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;

        let world = unsafe { &*self.world };
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        if ids.len() != Q::component_count() { return; }

        // Собираем все подходящие архетипы и разбиваем их на чанки.
        // Каждый чанк — независимый диапазон строк без aliasing.
        //
        // SAFETY:
        // - Каждый чанк обращается к непересекающемуся диапазону строк [start, end)
        // - Разные архетипы — разные буферы Column
        // - AccessDescriptor проверен compile() — нет Write-конфликтов между системами
        // - Structural changes запрещены во время выполнения систем
        // - Rayon work-stealing корректен при вложенных scope

        // Все чанки всех архетипов в одном flat Vec — лучший load balancing
        let chunks: Vec<(usize, usize, usize)> = world.archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| !arch.is_empty() && Q::matches_archetype(arch, &ids))
            .flat_map(|(arch_idx, arch)| {
                let total = arch.len();
                (0..(total + PAR_CHUNK_SIZE - 1) / PAR_CHUNK_SIZE).map(move |chunk_i| {
                    let start = chunk_i * PAR_CHUNK_SIZE;
                    let end   = (start + PAR_CHUNK_SIZE).min(total);
                    (arch_idx, start, end)
                })
            })
            .collect();

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch  = unsafe { &*world.archetypes.as_ptr().add(arch_idx) };
            let state = unsafe { Q::fetch_state(arch, &ids, Tick::ZERO) };
            for row in start..end {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(item);
                }
            }
        });
    }

    #[cfg(not(feature = "parallel"))]
    pub fn par_for_each_component<Q, F>(&self, f: F)
    where
        Q: WorldQuery,
        F: Fn(Q::Item<'_>),
    {
        self.for_each_component::<Q, _>(f);
    }

    /// Параллельная итерация с Entity.
    #[cfg(feature = "parallel")]
    pub fn par_for_each<Q, F>(&self, f: F)
    where
        Q: WorldQuery + Send,
        F: Fn(Entity, Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;

        let world = unsafe { &*self.world };
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        if ids.len() != Q::component_count() { return; }

        let chunks: Vec<(usize, usize, usize)> = world.archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| !arch.is_empty() && Q::matches_archetype(arch, &ids))
            .flat_map(|(arch_idx, arch)| {
                let total = arch.len();
                (0..(total + PAR_CHUNK_SIZE - 1) / PAR_CHUNK_SIZE).map(move |chunk_i| {
                    let start = chunk_i * PAR_CHUNK_SIZE;
                    let end   = (start + PAR_CHUNK_SIZE).min(total);
                    (arch_idx, start, end)
                })
            })
            .collect();

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch     = unsafe { &*world.archetypes.as_ptr().add(arch_idx) };
            let state    = unsafe { Q::fetch_state(arch, &ids, Tick::ZERO) };
            let entities = &arch.entities;
            for row in start..end {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entities[row], item);
                }
            }
        });
    }

    #[cfg(not(feature = "parallel"))]
    pub fn par_for_each<Q, F>(&self, f: F)
    where
        Q: WorldQuery,
        F: Fn(Entity, Q::Item<'_>),
    {
        self.for_each::<Q, _>(f);
    }
}

// ── ParallelWorld ──────────────────────────────────────────────

pub struct ParallelWorld<'w> {
    pub(crate) world:   *const World,
    pub(crate) _marker: std::marker::PhantomData<&'w World>,
}

unsafe impl Send for ParallelWorld<'_> {}
unsafe impl Sync for ParallelWorld<'_> {}

impl<'w> ParallelWorld<'w> {
    #[inline]
    pub unsafe fn get(&self) -> &'w World { &*self.world }
}

// ── CachedQuery ────────────────────────────────────────────────

pub struct CachedQuery<'w, Q: WorldQuery> {
    world:        &'w World,
    arch_indices: &'w [usize],
    last_run:     Tick,
    _phantom:     std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> CachedQuery<'w, Q> {
    pub fn new(world: &'w World, last_run: Tick) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        let version      = world.query_cache.version();
        let arch_indices = if ids.len() == Q::component_count() {
            unsafe {
                world.query_cache.get_or_compute(
                    &ids, version, &world.archetypes,
                    |arch| Q::matches_archetype(arch, &ids),
                )
            }
        } else {
            &[]
        };

        Self { world, arch_indices, last_run, _phantom: std::marker::PhantomData }
    }

    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(self.world, &mut ids);
        for &arch_idx in self.arch_indices {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() { continue; }
            let state    = unsafe { Q::fetch_state(arch, &ids, self.last_run) };
            let entities = &arch.entities;
            for row in 0..arch.len() {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entities[row], item);
                }
            }
        }
    }

    #[inline]
    pub fn for_each_component<F: FnMut(Q::Item<'_>)>(&self, mut f: F) {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(self.world, &mut ids);
        for &arch_idx in self.arch_indices {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() { continue; }
            let state = unsafe { Q::fetch_state(arch, &ids, self.last_run) };
            for row in 0..arch.len() {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } { f(item); }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.arch_indices.iter().map(|&i| self.world.archetypes[i].len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.arch_indices.iter().all(|&i| self.world.archetypes[i].is_empty())
    }
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

            fn write_into(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
            ) {
                let ($($T,)+) = self;
                $(
                    {
                        let cid = world.registry.get_or_register::<$T>();
                        if let Some(col_idx) = world.archetypes[archetype_id.0 as usize]
                            .column_index(cid)
                        {
                            unsafe {
                                let col = &mut world.archetypes[archetype_id.0 as usize]
                                    .columns[col_idx];
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
    world:  &'w mut World,
    entity: Entity,
}

impl<'w> EntityBuilder<'w> {
    pub fn insert<T: Component>(self, component: T) -> Self {
        self.world.insert(self.entity, component);
        self
    }

    pub fn id(self) -> Entity { self.entity }
}

// ── Scripting API ──────────────────────────────────────────────────────────
//
// Публичные accessor'ы для apex-scripting.
// Отделены от основного impl World чтобы было ясно: это внешний API,
// не внутренняя логика мира.
 
impl World {
    /// Доступ к аллокатору entity — для получения Entity по index.
    ///
    /// Используется `despawn()` из Rhai-скриптов.
    #[inline]
    pub fn entity_allocator(&self) -> &crate::entity::EntityAllocator {
        &self.entities
    }
 
    /// Получить ComponentId по строковому имени типа.
    ///
    /// Используется `apex-scripting` для разрешения имён из скриптов.
    /// Поиск линейный (O(N) по числу зарегистрированных компонентов),
    /// но вызывается только при инициализации движка — не в hot path.
    pub fn component_id_by_name(&self, name: &str) -> Option<crate::component::ComponentId> {
        self.registry.iter().find(|info| info.name == name).map(|i| i.id)
    }
}