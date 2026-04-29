use std::cell::UnsafeCell;
use std::sync::OnceLock;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    archetype::{Archetype, ArchetypeId},
    commands::Commands,
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry, Tick, Serializable},
    entity::{EntityAllocator, EntityLocation, Entity},
    events::EventRegistry,
    query::{QueryBuilder, WorldQuery},
    relations::{IdIndex, RelationRegistry, SubjectIndex},
    resources::Resources,
    system_param::{Res, ResMut, EventReader, EventWriter, WorldQuerySystemAccess},
    template::TemplateRegistry,
};

// ── QueryCache ─────────────────────────────────────────────────

struct CacheEntry {
    arch_indices: Vec<usize>,
    version:      u32,
}

/// Обёртка над SmallVec для zero-copy lookup через Borrow<[ComponentId]>.
#[derive(Clone, PartialEq, Eq, Hash)]
struct QueryCacheKey(SmallVec<[ComponentId; 8]>);

impl std::borrow::Borrow<[ComponentId]> for QueryCacheKey {
    fn borrow(&self) -> &[ComponentId] {
        &self.0
    }
}

pub(crate) struct QueryCache {
    entries: UnsafeCell<FxHashMap<QueryCacheKey, CacheEntry>>,
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
        // SAFETY: весь метод unsafe, caller гарантирует отсутствие других &self доступа.
        // Управляем временем жизни заимствований через raw pointer, чтобы
        // избежать конфликта mutable borrow'ов между hit-paths (get_mut) и miss-paths (insert).
        let raw = self.entries.get();

        // Hit path — lookup по &[ComponentId] через Borrow, zero-copy, без аллокации
        // При hit или stale — немедленный return, borrow заканчивается.
        {
            let map = &mut *raw;
            if let Some(entry) = map.get_mut(key) {
                if entry.version == world_version {
                    return &entry.arch_indices;
                }
                // Cache stale — обновляем на месте, не создаём новую запись
                entry.arch_indices = archetypes
                    .iter()
                    .enumerate()
                    .filter(|(_, arch)| !arch.is_empty() && matches(arch))
                    .map(|(i, _)| i)
                    .collect();
                entry.version = world_version;
                return &entry.arch_indices;
            }
        }

        // Miss — вставляем новую запись (аллокация QueryCacheKey только здесь).
        // Используем сырой указатель, т.к. предыдущий &mut из `raw` уже не используется.
        let query_key = QueryCacheKey(key.iter().copied().collect());
        let arch_indices = archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| !arch.is_empty() && matches(arch))
            .map(|(i, _)| i)
            .collect();
        (*raw).insert(query_key, CacheEntry { arch_indices, version: world_version });

        // Возвращаем ссылку на вставленные данные через сырой указатель
        &(*raw).get(key).unwrap().arch_indices
    }

    pub fn invalidate(&mut self) { self.version = self.version.wrapping_add(1); }

    /// Инвалидировать только записи кеша, затрагивающие данный компонент.
    /// Позволяет сохранить кеш для несвязанных запросов.
    pub fn invalidate_for(&mut self, changed_cid: ComponentId) {
        let map = unsafe { &mut *self.entries.get() };
        // Удаляем только те ключи (списки компонентов запроса),
        // которые содержат изменённый ComponentId.
        map.retain(|key, _| !key.0.contains(&changed_cid));
    }

    pub fn version(&self) -> u32 { self.version }
}

// ── ArchetypeKey ───────────────────────────────────────────────

/// Ключ для archetype_index — хэшируется без heap-аллокации.
/// Внутри хранит компоненты inline до 12 штук через SmallVec.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ArchetypeKey(SmallVec<[ComponentId; 12]>);

impl From<&[ComponentId]> for ArchetypeKey {
    fn from(ids: &[ComponentId]) -> Self {
        Self(ids.iter().copied().collect())
    }
}

/// Zero-copy lookup: позволяет `archetype_index.get(components)` работать
/// напрямую с `&[ComponentId]` без создания временного ArchetypeKey.
impl std::borrow::Borrow<[ComponentId]> for ArchetypeKey {
    fn borrow(&self) -> &[ComponentId] {
        &self.0
    }
}

// ── World ──────────────────────────────────────────────────────

pub struct World {
    pub(crate) entities:             EntityAllocator,
    pub(crate) registry:             ComponentRegistry,
    pub(crate) archetypes:           Vec<Archetype>,
    pub(crate) archetype_index:      FxHashMap<ArchetypeKey, ArchetypeId>,
    /// Индекс компонент → список архетипов, содержащих этот компонент.
    /// Используется в Query::new_with_tick для O(1) поиска архетипов-кандидатов
    /// вместо линейного обхода всех архетипов.
    pub(crate) component_arch_index: FxHashMap<ComponentId, SmallVec<[ArchetypeId; 16]>>,
    pub(crate) current_tick:         Tick,
    pub(crate) query_cache:          QueryCache,
    pub(crate) relations:            RelationRegistry,
    pub(crate) id_index:             IdIndex,
    pub(crate) subject_index:        SubjectIndex,
    pub        resources:       Resources,
    pub(crate) events:          EventRegistry,
    /// Коллбэки, вызываемые при записи компонента (вызове get_mut).
    /// Функция-указатель (Copy), чтобы избежать borrow conflict с self.
    /// Ключ — ComponentId.
    pub(crate) write_hooks:     FxHashMap<ComponentId, fn(Entity, &mut World)>,
    /// Реестр именованных шаблонов (EntityTemplate).
    pub(crate) templates:       TemplateRegistry,
}

impl World {
    pub fn new() -> Self {
        let mut world = Self {
            entities:        EntityAllocator::new(),
            registry:        ComponentRegistry::new(),
            archetypes:      Vec::new(),
            archetype_index:      FxHashMap::default(),
            component_arch_index: FxHashMap::default(),
            current_tick:    Tick(1),
            query_cache:     QueryCache::new(),
            relations:       RelationRegistry::new(),
            id_index:        IdIndex::default(),
            subject_index:   SubjectIndex::new(),
            resources:       Resources::new(),
            events:          EventRegistry::new(),
            write_hooks:     FxHashMap::default(),
            templates:       TemplateRegistry::new(),
        };
        world.archetypes.push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world.archetype_index.insert(ArchetypeKey(SmallVec::new()), ArchetypeId::EMPTY);
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

    /// Зарегистрировать компонент с поддержкой сериализации в JSON-формате.
    pub fn register_component_serde_json<T: crate::component::Serializable>(&mut self) -> ComponentId {
        self.registry.register_serde_json::<T>()
    }

    pub fn registry(&self) -> &ComponentRegistry { &self.registry }

    pub fn archetypes(&self) -> &[Archetype] { &self.archetypes }

    pub fn relation_registry(&self) -> &RelationRegistry { &self.relations }

    pub fn relation_registry_mut(&mut self) -> &mut RelationRegistry { &mut self.relations }

    pub fn subject_index_raw(&self, entity_index: u32) -> Vec<u32> {
        self.subject_index.get_all(entity_index)
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
    pub fn events<T: Send + Sync + 'static>(&self) -> &crate::events::Events<T> {
        self.events.get::<T>()
    }

    #[track_caller]
    pub fn events_mut<T: Send + Sync + 'static>(&mut self) -> &mut crate::events::Events<T> {
        self.events.get_mut::<T>()
    }

    /// Отправить событие.
    ///
    /// Если тип события ещё не зарегистрирован — регистрирует автоматически
    /// (вызов `world.add_event::<T>()` не требуется).
    pub fn send_event<T: Send + Sync + 'static>(&mut self, event: T) {
        self.events.get_or_register_mut::<T>().send(event);
    }

    /// Безопасная версия `send_event` — всегда успешна, так как
    /// при необходимости автоматически регистрирует тип.
    pub fn try_send_event<T: Send + Sync + 'static>(&mut self, event: T) -> bool {
        self.events.get_or_register_mut::<T>().send(event);
        true
    }

    /// Предварительно выделить capacity для событий указанного типа.
    ///
    /// Позволяет избежать многократных реаллокаций при массовой отправке
    /// событий в одном тике. Вызывать перед циклом отправки.
    pub fn event_reserve<T: Send + Sync + 'static>(&mut self, capacity: usize) {
        self.events.get_or_register_mut::<T>().reserve(capacity);
    }

    pub fn event_queue_ptr<T: Send + Sync + 'static>(
        &self,
    ) -> Option<*mut crate::events::Events<T>> {
        self.events.get_raw_ptr::<T>()
    }

    // ── Spawn ──────────────────────────────────────────────────

    /// Создать entity из Bundle.
    ///
    /// Для пустой сущности (без компонентов) используйте `spawn(())`.
    /// Для единичного компонента — `spawn((MyComponent,))`.
    /// Для нескольких — `spawn((A, B, C))`.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
        let ids = bundle.component_ids(&mut self.registry);
        if ids.is_empty() {
            // Быстрый путь для пустой entity (spawn(()))
            let entity = self.entities.allocate();
            let row    = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
            self.entities.set_location(entity, EntityLocation {
                archetype_id: ArchetypeId::EMPTY,
                row,
            });
            return entity;
        }
        // Обычный путь
        let archetype_id = self.get_or_create_archetype(&ids);
        let entity       = self.entities.allocate();
        let row          = self.archetypes[archetype_id.0 as usize].entities.len();
        let tick         = self.current_tick;
        self.archetypes[archetype_id.0 as usize].entities.push(entity);
        bundle.write_into(self, archetype_id, row, tick);
        self.entities.set_location(entity, EntityLocation { archetype_id, row: row as u32 });
        entity
    }

    /// Внутренний общий метод для `spawn_many` / `spawn_many_silent`.
    /// Всегда возвращает `Vec<Entity>`, а публичные обёртки решают,
    /// возвращать его или игнорировать.
    fn spawn_many_inner<B, F>(&mut self, count: usize, mut make_bundle: F) -> Vec<Entity>
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        if count == 0 { return Vec::new(); }

        let probe        = make_bundle(0);
        let ids          = probe.component_ids(&mut self.registry);
        drop(probe);

        let archetype_id = self.get_or_create_archetype(&ids);
        let arch_idx     = archetype_id.0 as usize;
        let start_row    = self.archetypes[arch_idx].entities.len();
        let tick         = self.current_tick;

        self.archetypes[arch_idx].entities.reserve(count);
        for col in &mut self.archetypes[arch_idx].columns {
            col.reserve(count);
        }

        let entities = self.entities.allocate_batch(count);

        // Предвычисляем column indices для всех компонентов бандла,
        // чтобы избежать повторных вызовов get_or_register и column_index
        // в write_into для каждой entity (экономит ~40k HashMap lookup'ов при 10k entity).
        // Храним только позиционный индекс колонки — без ComponentId,
        // так как порядок col_indices соответствует порядку ids.
        let col_indices: SmallVec<[usize; 8]> = ids.iter()
            .filter_map(|&id| {
                self.archetypes[arch_idx].column_index(id)
            })
            .collect();

        // Порог: для 1 компонента per-entity loop быстрее, чем bulk copy.
        // Для 2+ компонентов bulk copy из первой строки выигрывает за счёт
        // устранения 10,000 вызовов make_bundle и 40,000 поисков в col_indices.
        if col_indices.len() <= 1 {
            // Per-entity loop — старый подход, быстрее для малого числа компонентов.
            for (i, &entity) in entities.iter().enumerate() {
                let row    = start_row + i;
                let bundle = make_bundle(i);
                self.archetypes[arch_idx].entities.push(entity);
                bundle.write_into_batch(self, archetype_id, row, tick, &col_indices);
            }
        } else {
            // Первая entity — пишем через штатный write_into_batch (создаёт "шаблон" строки).
            // Для Copy-компонентов это позволяет нам затем bulk-копировать данные из первой
            // строки во все последующие, избегая 10,000 вызовов make_bundle и 40,000 поисков
            // в col_indices.iter().find() (как в Legion SOA подходе).
            let first_entity  = entities[0];
            let first_bundle  = make_bundle(0);
            self.archetypes[arch_idx].entities.push(first_entity);
            first_bundle.write_into_batch(self, archetype_id, start_row, tick, &col_indices);

            // Остальные count-1 entity — bulk copy из первой строки во все последующие.
            // Безопасность: данные уже записаны в первую строку через write_into_batch,
            // память зарезервирована через reserve(count), change_ticks также зарезервированы.
            for (i, &entity) in entities[1..].iter().enumerate() {
                let row = start_row + 1 + i;
                self.archetypes[arch_idx].entities.push(entity);
                for &col_idx in &col_indices {
                    unsafe {
                        let col = &mut self.archetypes[arch_idx].columns[col_idx];
                        if col.item_size > 0 {
                            let src = col.get_ptr(start_row);
                            let dst = col.get_ptr(row);
                            std::ptr::copy_nonoverlapping(src, dst, col.item_size);
                        }
                        col.change_ticks.push(tick);
                        col.len += 1;
                    }
                }
            }
        }

        self.entities.set_locations_batch(&entities, archetype_id, start_row as u32);
        entities
    }

    pub fn spawn_many<B, F>(&mut self, count: usize, make_bundle: F) -> Vec<Entity>
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        self.spawn_many_inner(count, make_bundle)
    }

    pub fn spawn_many_silent<B, F>(&mut self, count: usize, make_bundle: F)
    where
        B: Bundle,
        F: FnMut(usize) -> B,
    {
        self.spawn_many_inner(count, make_bundle);
    }

    /// Создать entity из итератора бандлов (как Bevy `spawn_batch`).
    ///
    /// Позволяет порождать entity с разными наборами компонентов в одной пачке:
    ///
    /// ```rust
    /// # use apex_core::prelude::*;
    /// # let mut world = World::new();
    /// # struct Health(f32);
    /// # struct Armor(f32);
    /// world.spawn_batch([
    ///     (Health(100.0), Armor(10.0)),
    ///     (Health(50.0),  Armor(5.0)),
    /// ]);
    /// ```
    ///
    /// Внутри собирает итератор в `Vec` и вызывает `spawn` для каждого элемента.
    /// Для массового спавна **одинаковых** бандлов используйте [`spawn_many`] —
    /// он оптимизирован через bulk-copy.
    pub fn spawn_batch<I>(&mut self, iter: I) -> Vec<Entity>
    where
        I: IntoIterator,
        I::Item: Bundle,
    {
        let items: Vec<I::Item> = iter.into_iter().collect();
        let mut entities = Vec::with_capacity(items.len());
        for bundle in items {
            entities.push(self.spawn(bundle));
        }
        entities
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
                    col.write_at(location.row as usize, &component as *const T as *const u8, tick);
                }
            }
            std::mem::forget(component);
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        self.query_cache.invalidate_for(component_id);
        let new_row     = self.move_entity(entity, location, new_arch_id);
        let tick        = self.current_tick;
        unsafe {
            self.archetypes[new_arch_id.0 as usize]
                .write_component(new_row as usize, component_id, &component as *const T as *const u8, tick);
        }
        std::mem::forget(component);
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
        });
    }

    /// Вставить компонент по raw данным.
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
                        col.write_at(location.row as usize, data.as_ptr(), tick);
                    }
                }
            }
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        self.query_cache.invalidate_for(component_id);
        let new_row     = self.move_entity(entity, location, new_arch_id);
        unsafe {
            self.archetypes[new_arch_id.0 as usize]
                .write_component(new_row as usize, component_id, data.as_ptr(), tick);
        }
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
        });
    }

    /// Удалить компонент по raw ComponentId.
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
        self.query_cache.invalidate_for(component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
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
        self.query_cache.invalidate_for(component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(entity, EntityLocation {
            archetype_id: new_arch_id,
            row:          new_row as u32,
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
            if let Some(displaced) = self.archetypes[arch_idx].remove_row(location.row as usize) {
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
                .get_component::<T>(location.row as usize, component_id)
        }
    }

    #[inline]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let component_id = self.registry.get_id::<T>()?;
        let location     = self.entities.get_location(entity)?;
        let tick         = self.current_tick;

        // 1. Обновляем tick изменения (change detection)
        {
            let arch = &mut self.archetypes[location.archetype_id.0 as usize];
            if let Some(col_idx) = arch.column_index(component_id) {
                if (location.row as usize) < arch.columns[col_idx].change_ticks.len() {
                    arch.columns[col_idx].change_ticks[location.row as usize] = tick;
                }
            }
        }

        // 2. Вызываем write_hook, если зарегистрирован.
        //    Хук может переместить entity в другой archetype (например, insert TransformDirty).
        //    fn pointer — Copy, поэтому копируем и отпускаем borrow на self.write_hooks.
        let hook_fn: Option<fn(Entity, &mut World)> = self.write_hooks.get(&component_id).copied();
        if let Some(hook) = hook_fn {
            hook(entity, self);
        }

        // 3. Перепроверяем location (хук мог изменить archetype) и получаем ссылку
        let location2 = self.entities.get_location(entity)?;
        let arch      = &mut self.archetypes[location2.archetype_id.0 as usize];
        let col_idx   = arch.column_index(component_id)?;
        unsafe { Some(arch.columns[col_idx].get_mut::<T>(location2.row as usize)) }
    }

    /// Зарегистрировать write_hook для компонента T.
    /// Хук вызывается при каждом вызове `get_mut::<T>()` ПОСЛЕ обновления tick'а изменения.
    ///
    /// Используется, например, для автоматической пометки TransformDirty
    /// при изменении LocalTransform.
    pub fn register_write_hook<T: Component>(
        &mut self,
        hook: fn(Entity, &mut World),
    ) {
        if let Some(cid) = self.registry.get_id::<T>() {
            self.write_hooks.insert(cid, hook);
        }
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
        let new_id = self.get_or_create_archetype(&new_components);
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
        let new_id = self.get_or_create_archetype(&new_components);
        self.archetypes[current.0 as usize].remove_edges.insert(remove, new_id);
        self.archetypes[new_id.0 as usize].add_edges.insert(remove, current);
        new_id
    }

    #[inline(never)]
    pub(crate) fn get_or_create_archetype(
        &mut self,
        components: &[ComponentId],
    ) -> ArchetypeId {
        // Borrow<[ComponentId]> — zero-copy lookup без создания ArchetypeKey
        if let Some(&id) = self.archetype_index.get(components) { return id; }
        let id    = ArchetypeId(self.archetypes.len() as u32);
        let infos: Vec<&ComponentInfo> = components.iter()
            .filter_map(|&cid| self.registry.get_info(cid))
            .collect();
        let arch  = Archetype::new(id, components.iter().copied().collect(), &infos);
        for &cid in &arch.component_ids {
            self.id_index.register_archetype(cid, id);
            self.component_arch_index
                .entry(cid)
                .or_default()
                .push(id);
        }
        self.archetypes.push(arch);
        self.archetype_index.insert(ArchetypeKey::from(components), id);
        self.query_cache.invalidate();
        id
    }

    pub(crate) fn move_entity(
        &mut self,
        entity:          Entity,
        from_location:   EntityLocation,
        to_archetype_id: ArchetypeId,
    ) -> u32 {
        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx   = to_archetype_id.0 as usize;
        let from_row = from_location.row as usize;

        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        // Единственный проход: для каждой колонки из исходного архетипа
        // определяем наличие в целевом и сразу копируем или дропаем.
        let from_len = self.archetypes[from_idx].columns.len();

        for i in 0..from_len {
            let cid       = self.archetypes[from_idx].columns[i].component_id;
            let item_size = self.archetypes[from_idx].columns[i].item_size;

            if let Some(to_col_idx) = self.archetypes[to_idx].column_index(cid) {
                // Компонент присутствует в обоих архетипах — копируем
                unsafe {
                    if item_size > 0 {
                        if self.archetypes[to_idx].columns[to_col_idx].len
                            >= self.archetypes[to_idx].columns[to_col_idx].capacity
                        {
                            self.archetypes[to_idx].columns[to_col_idx].grow();
                        }
                        let src = self.archetypes[from_idx].columns[i].get_ptr(from_row);
                        let dst = self.archetypes[to_idx].columns[to_col_idx].get_ptr(to_row);
                        std::ptr::copy_nonoverlapping(src, dst, item_size);
                    }
                    let tick = self.archetypes[from_idx].columns[i].get_tick(from_row);
                    self.archetypes[to_idx].columns[to_col_idx].change_ticks.push(tick);
                    self.archetypes[to_idx].columns[to_col_idx].len += 1;

                    // swap_remove без drop (данные перемещены в целевой архетип)
                    self.archetypes[from_idx].columns[i].swap_remove_no_drop(from_row);
                }
            } else {
                // Компонент отсутствует в целевом — дропаем
                unsafe {
                    self.archetypes[from_idx].columns[i].swap_remove_and_drop(from_row);
                }
            }
        }

        // Исправляем location для вытесненной entity (swap_remove)
        unsafe {
            let from_last = self.archetypes[from_idx].entities.len() - 1;
            if from_row != from_last {
                let displaced = self.archetypes[from_idx].entities[from_last];
                self.archetypes[from_idx].entities.swap(from_row, from_last);
                self.archetypes[from_idx].entities.pop();
                self.entities.set_location(displaced, EntityLocation {
                    archetype_id: from_location.archetype_id,
                    row:          from_row as u32,
                });
            } else {
                self.archetypes[from_idx].entities.pop();
            }
        }

        to_row as u32
    }

    // ── Оптимизация 4.1: add_relation_batch ───────────────────

    /// Batch-добавление одинаковой relation от множества субъектов к одному target.
    ///
    /// Оптимизирован для массового создания иерархий (тайловые карты, армии).
    /// Группирует subjects по текущему архетипу и делает один batch move
    /// для каждой группы вместо N отдельных move_entity.
    ///
    /// # Сложность
    /// O(S log S) где S = subjects.len() (группировка по архетипу).
    /// Против O(S) вызовов move_entity при наивном подходе.
    ///
    /// # Пример
    /// ```ignore
    /// // Создание иерархии 1000 тайлов за один batch
    /// world.add_relation_batch(&tiles, ChildOf, map_entity);
    /// ```
    pub fn add_relation_batch<R: crate::relations::RelationKind>(
        &mut self,
        subjects: &[Entity],
        _kind: R,
        target: Entity,
    ) {
        if subjects.is_empty() { return; }

        let kind_idx    = self.relations.get_or_register::<R>();
        let relation_id = crate::relations::encode_relation(kind_idx, target.index);
        self.ensure_relation_component(relation_id);

        // Группируем subjects по текущему архетипу
        let mut by_arch: FxHashMap<ArchetypeId, Vec<Entity>> = FxHashMap::default();
        for &entity in subjects {
            if let Some(loc) = self.entities.get_location(entity) {
                by_arch.entry(loc.archetype_id).or_default().push(entity);
            }
        }

        // Для каждой группы — batch move в целевой архетип
        let tick = self.current_tick;
        for (arch_id, group) in by_arch {
            let new_arch_id = self.find_or_create_archetype_with(arch_id, relation_id);

            for entity in group {
                if let Some(loc) = self.entities.get_location(entity) {
                    let new_row = self.move_entity(entity, loc, new_arch_id);
                    // После move_entity необходимо обновить relation-колонку
                    // (move_entity копирует только общие компоненты, relation добавляется впервые)
                    if let Some(col_idx) = self.archetypes[new_arch_id.0 as usize].column_index(relation_id) {
                        let col = &mut self.archetypes[new_arch_id.0 as usize].columns[col_idx];
                        col.change_ticks.push(tick);
                        col.len += 1;
                    }
                    self.entities.set_location(entity, EntityLocation {
                        archetype_id: new_arch_id,
                        row: new_row as u32,
                    });
                    self.subject_index.add(entity.index, relation_id);
                }
            }
        }
    }
}

impl Default for World { fn default() -> Self { Self::new() } }

// ── SystemContext ──────────────────────────────────────────────

/// Размер чанка для par_for_each.
///
/// Разбивает архетип на блоки по N entity для параллельной обработки.
/// Слишком маленький → overhead rayon съедает выигрыш.
/// Слишком большой → мало задач, плохой load balancing.
///
/// Пользовательский максимальный размер чанка для `adaptive_chunk_size`.
///
/// Если равен 0 (по умолчанию) — используется `DEFAULT_MAX_CHUNK_SIZE` (16384).
/// Можно переопределить через переменную окружения `APEX_PAR_CHUNK_SIZE`
/// или через `set_par_chunk_size()`.
pub static PAR_CHUNK_SIZE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// Константы заменены адаптивной логикой в adaptive_chunk_size.
// MIN_CHUNK_SIZE и MAX_CHUNK_SIZE больше не используются.
// Оставляем только для обратной совместимости, если нужно.

pub const DEFAULT_MAX_CHUNK_SIZE: usize = 16384;
/// Вычислить адаптивный размер чанка на основе количества entity.
///
/// Формула: `entity_count / num_threads` (1 чанк на поток),
/// но не более абсолютного потолка (по умолчанию 16384, или из `PAR_CHUNK_SIZE`).
/// Для малых миров динамически увеличивается минимальный размер чанка.
/// num_threads — количество потоков rayon (передаётся из вызывающего кода).
///
/// **Runtime-адаптация:** для малых нагрузок (< 100 entities) динамически
/// увеличиваем минимальный размер чанка до 128, чтобы избежать оверхеда
/// rayon на микро-задачах. Для средних (100–1000) — 32. Для больших — 64.
pub fn adaptive_chunk_size(entity_count: usize, num_threads: usize) -> usize {
    let n = num_threads.max(1);

    // 1. Базовый размер: поровну на каждый поток
    let mut chunk = entity_count / n;

    // 2. Абсолютный потолок: берём из PAR_CHUNK_SIZE (если задан и >0),
    //    иначе DEFAULT_MAX_CHUNK_SIZE (16384).
    let absolute_max = {
        let user = PAR_CHUNK_SIZE.load(std::sync::atomic::Ordering::Relaxed);
        if user > 0 { user } else { DEFAULT_MAX_CHUNK_SIZE }
    };
    if chunk > absolute_max {
        chunk = absolute_max;
    }

    // 3. Динамический минимум — чтобы не плодить микро-задачи
    let dynamic_min = if entity_count < 100 {
        128
    } else if entity_count < 1000 {
        32
    } else {
        // Для крупных миров — не меньше 64, но если world очень большой,
        // не стоит опускаться ниже absolute_max/256 (эвристика).
        // Пока оставим 64.
        64
    };
    if chunk < dynamic_min {
        chunk = dynamic_min;
    }

    // 4. Не больше entity_count (если dynamic_min перекрывает)
    chunk.min(entity_count)
}

/// Установить размер чанка для par_for_each.
/// Используется для экспериментов — позволяет менять CHUNK_SIZE без перекомпиляции.
pub fn set_par_chunk_size(chunk_size: usize) {
    PAR_CHUNK_SIZE.store(chunk_size, std::sync::atomic::Ordering::Relaxed);
}

/// Инициализировать PAR_CHUNK_SIZE из переменной окружения (если задана).
pub fn init_par_chunk_size_from_env() {
    if let Ok(val) = std::env::var("APEX_PAR_CHUNK_SIZE") {
        let trimmed = val.trim();
        if let Ok(chunk_size) = trimmed.parse::<usize>() {
            PAR_CHUNK_SIZE.store(chunk_size, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Обёртка для хранения `Commands` в static c `UnsafeCell`.
///
/// # SAFETY
/// `Commands` содержит `*mut u8` в `CommandArena` (owned pointer),
/// что формально не `Send`/`Sync`. Однако заглушка используется
/// **только** в single-thread контексте (sequential fallback или
/// когда `deferred_cmds` is null), поэтому гонок быть не может.
struct SyncCommands(UnsafeCell<Commands>);
unsafe impl Send for SyncCommands {}
unsafe impl Sync for SyncCommands {}

/// Статическая заглушка для `Commands` когда `deferred_cmds` is null.
/// Используется в `commands()` для безопасного возврата `&mut Commands`
/// когда thread-local команды не были предоставлены (например, в sequential режиме).
static DUMMY_COMMANDS: OnceLock<SyncCommands> = OnceLock::new();

fn dummy_commands() -> &'static mut Commands {
    let sc = DUMMY_COMMANDS.get_or_init(|| SyncCommands(UnsafeCell::new(Commands::new())));
    // SAFETY: заглушка инициализируется один раз и используется только
    // в single-thread контексте (sequential fallback или когда
    // deferred_cmds is null). Никакой другой код не имеет доступа
    // к этой памяти одновременно.
    unsafe { &mut *sc.0.get() }
}

pub struct SystemContext<'w> {
    /// SubWorld'ы, которые видит эта система.
    /// Обычно один SubWorld, но может быть несколько если система
    /// работает с несколькими группами архетипов.
    pub(crate) sub_worlds: &'w [crate::sub_world::SubWorld<'w>],
    /// Thread-local команды. Указатель на `Vec<Commands>` из Scheduler.
    /// Каждый поток rayon имеет свой `Commands` по индексу `current_thread_index()`.
    /// Если `null` — метод `commands()` возвращает заглушку.
    pub(crate) deferred_cmds: *mut Vec<Commands>,
}

unsafe impl Send for SystemContext<'_> {}
unsafe impl Sync for SystemContext<'_> {}

impl<'w> SystemContext<'w> {
    pub fn new(sub_worlds: &'w [crate::sub_world::SubWorld<'w>]) -> Self {
        Self { sub_worlds, deferred_cmds: std::ptr::null_mut() }
    }

    /// Создаёт SystemContext из одного SubWorld (наиболее частый случай).
    pub fn from_sub_world(sub_world: &'w crate::sub_world::SubWorld<'w>) -> Self {
        Self { sub_worlds: std::slice::from_ref(sub_world), deferred_cmds: std::ptr::null_mut() }
    }

    /// Создать контекст с thread-local командами.
    pub fn with_commands(
        sub_worlds: &'w [crate::sub_world::SubWorld<'w>],
        deferred_cmds: *mut Vec<Commands>,
    ) -> Self {
        Self { sub_worlds, deferred_cmds }
    }

    /// Получить `Commands` для текущего потока.
    /// Команды применяются планировщиком после завершения Stage.
    ///
    /// Если `deferred_cmds` is null (например, в sequential режиме),
    /// возвращает статическую заглушку — вызов не паникует.
    #[inline]
    pub fn commands(&self) -> &mut Commands {
        if self.deferred_cmds.is_null() {
            return dummy_commands();
        }
        #[cfg(feature = "parallel")]
        unsafe {
            let thread_idx = rayon::current_thread_index().unwrap_or(0);
            let vec = &mut *self.deferred_cmds;
            &mut vec[thread_idx]
        }
        #[cfg(not(feature = "parallel"))]
        {
            dummy_commands()
        }
    }

    /// Получить World (для обратной совместимости).
    /// Используется для query, resource, event доступа.
    fn world(&self) -> &'w World {
        self.sub_worlds[0].world
    }

    #[inline]
    pub fn query<Q: WorldQuery>(&self) -> crate::query::Query<'_, Q> {
        crate::query::Query::new(self.world())
    }

    #[inline]
    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> crate::query::Query<'_, Q> {
        crate::query::Query::new_with_tick(self.world(), last_run)
    }

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(self.world().resource::<T>())
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = self.world()
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn try_resource<T: Send + Sync + 'static>(&self) -> Option<Res<'_, T>> {
        self.world().try_resource::<T>().map(Res)
    }

    #[inline]
    pub fn try_resource_mut<T: Send + Sync + 'static>(&self) -> Option<ResMut<'_, T>> {
        unsafe {
            self.world()
                .resources
                .get_raw_ptr::<T>()
                .map(|ptr| ResMut::from_ptr(ptr))
        }
    }

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        unsafe {
            let ptr = self.world()
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self.world()
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered");
            EventWriter::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn entity_count(&self) -> usize {
        self.world().entity_count()
    }

    // Итерация только через ctx.query::<Q>().for_each(...)
    // или ctx.query::<Q>().par_for_each(...)
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
    cached_ids:   Vec<ComponentId>,
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

        Self {
            world,
            arch_indices,
            last_run,
            cached_ids: ids,
            _phantom: std::marker::PhantomData,
        }
    }

    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        let ids = &self.cached_ids;
        if ids.len() != Q::component_count() { return; }
        for &arch_idx in self.arch_indices {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() { continue; }
            let state    = unsafe { Q::fetch_state(arch, ids, self.last_run) };
            let entities = &arch.entities[..arch.len()];
            for (row, &entity) in entities.iter().enumerate() {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entity, item);
                }
            }
        }
    }

    /// Параллельная итерация.
    /// Работает только с `feature = "parallel"`.
    #[cfg(feature = "parallel")]
    pub fn par_for_each<F>(&self, f: F)
    where
        Q: Send,
        F: Fn(Entity, Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;
        use crate::par_utils::compute_par_chunks;
        let num_threads = rayon::current_num_threads();
        let ids = &self.cached_ids;
        if ids.len() != Q::component_count() { return; }

        let world    = self.world;
        let last_run = self.last_run;
        let chunks = compute_par_chunks(
            self.arch_indices
                .iter()
                .copied()
                .filter(|&arch_idx| world.archetypes[arch_idx].len() > 0)
                .map(|arch_idx| (arch_idx, world.archetypes[arch_idx].len())),
            num_threads,
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch     = &world.archetypes[arch_idx];
            let state    = unsafe { Q::fetch_state(arch, ids, last_run) };
            let entities = &arch.entities[start..end];
            for (row, &entity) in entities.iter().enumerate() {
                if let Some(item) = unsafe { Q::fetch_item(state, start + row) } {
                    f(entity, item);
                }
            }
        });
    }

    /// Параллельная итерация по (Entity, компонентам) — fallback для sequential.
    #[cfg(not(feature = "parallel"))]
    pub fn par_for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, f: F) {
        self.for_each(f);
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
    fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]>;
    fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick);

    /// Пакетная запись компонентов с предвычисленными column indices.
    ///
    /// По умолчанию вызывает `write_into`. Переопределяется в макросе `impl_bundle`
    /// для оптимизации: использует переданные `col_indices` вместо повторного
    /// вызова `get_or_register` и `column_index` для каждой entity.
    fn write_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        tick: Tick,
        _col_indices: &[usize],
    ) {
        self.write_into(world, archetype_id, row, tick);
    }
}

macro_rules! impl_bundle {
    ($($T:ident),+) => {
        #[allow(non_snake_case)]
        impl<$($T: Component),+> Bundle for ($($T,)+) {
            fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
                let mut ids: SmallVec<[ComponentId; 8]> = smallvec::smallvec![
                    $( registry.get_or_register::<$T>() ),+
                ];
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

            fn write_into_batch(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
                col_indices:  &[usize],
            ) {
                let ($($T,)+) = self;
                #[allow(unused_assignments)]
                let mut i = 0;
                $(
                    {
                        // Прямой позиционный доступ — col_indices[i] уже вычислен
                        // в spawn_many_inner, устраняет O(K) поиск через find()
                        let col_idx = col_indices[i];
                        i += 1;
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
// ── impl Bundle for () ────────────────────────────────────────

impl Bundle for () {
    fn component_ids(&self, _registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
        SmallVec::new()
    }

    fn write_into(self, _world: &mut World, _archetype_id: ArchetypeId, _row: usize, _tick: Tick) {
        // ()
    }
}

// ── EntityRef ──────────────────────────────────────────────────

/// Facade для операций над одной entity: вставка, удаление, деспавн, чтение.
///
/// Создаётся через [`World::entity`].
pub struct EntityRef<'w> {
    world:  &'w mut World,
    entity: Entity,
}

impl<'w> EntityRef<'w> {
    /// Вернуть идентификатор entity.
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Проверить, жива ли entity.
    pub fn is_alive(&self) -> bool {
        self.world.entities.is_alive(self.entity)
    }

    /// Вставить компонент в entity.
    pub fn insert<T: Component>(&mut self, component: T) -> &mut Self {
        self.world.insert(self.entity, component);
        self
    }

    /// Удалить компонент типа T из entity.
    pub fn remove<T: Component>(&mut self) -> bool {
        self.world.remove::<T>(self.entity)
    }

    /// Деспавнить entity.
    pub fn despawn(&mut self) -> bool {
        self.world.despawn(self.entity)
    }

    /// Прочитать компонент T.
    pub fn get<T: Component>(&self) -> Option<&T> {
        self.world.get::<T>(self.entity)
    }

    /// Прочитать компонент T мутабельно.
    pub fn get_mut<T: Component>(&mut self) -> Option<&mut T> {
        self.world.get_mut::<T>(self.entity)
    }

    /// Добавить relation между этой entity и target.
    pub fn add_relation<R: crate::relations::RelationKind>(&mut self, kind: R, target: Entity) -> &mut Self {
        self.world.add_relation(self.entity, kind, target);
        self
    }

    /// Удалить relation.
    pub fn remove_relation<R: crate::relations::RelationKind>(&mut self, kind: R, target: Entity) -> &mut Self {
        self.world.remove_relation(self.entity, kind, target);
        self
    }

    /// Проверить наличие relation.
    pub fn has_relation<R: crate::relations::RelationKind>(&self, kind: R, target: Entity) -> bool {
        self.world.has_relation(self.entity, kind, target)
    }
}

impl<'w> World {
    /// Получить [`EntityRef`] для entity.
    pub fn entity(&mut self, entity: Entity) -> EntityRef<'_> {
        EntityRef { world: self, entity }
    }
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

    // ── EntityTemplate API ────────────────────────────────────────

    /// Зарегистрировать именованный шаблон сущности.
    pub fn register_template(&mut self, name: &str, template: impl crate::template::EntityTemplate + 'static) {
        self.templates.register(name, template);
    }

    /// Создать entity из зарегистрированного шаблона с параметрами.
    ///
    /// Если шаблон возвращает `Some(parent)` из [`EntityTemplate::parent()`],
    /// то после спавна автоматически устанавливается `ChildOf(parent)`.
    pub fn spawn_from_template(
        &mut self,
        name: &str,
        params: &crate::template::TemplateParams,
    ) -> Option<crate::entity::Entity> {
        // Используем raw pointer, чтобы избежать borrow conflict:
        // `self.templates` (immut) и `self` (mut) одновременно.
        let raw = self.templates.get_raw(name)?;
        // SAFETY: шаблон жив, пока жив World (мы его не удаляем),
        // и get_raw возвращает корректный указатель.
        unsafe {
            let template = &*raw;
            let entity = template.spawn(self, params);
            if let Some(parent) = template.parent() {
                self.add_relation(entity, crate::relations::ChildOf, parent);
            }
            Some(entity)
        }
    }

    /// Создать entity из шаблона с параметрами по умолчанию.
    pub fn spawn_template(&mut self, name: &str) -> Option<crate::entity::Entity> {
        self.spawn_from_template(name, &crate::template::TemplateParams::new())
    }

    /// Доступ к реестру шаблонов (только для чтения).
    pub fn template_registry(&self) -> &crate::template::TemplateRegistry {
        &self.templates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Score(u32);

    #[test]
    fn system_context_try_resource_some() {
        let mut world = World::new();
        world.insert_resource(Score(42));
        let sw = crate::sub_world::SubWorld::new(&world, &[]);
        let ctx = SystemContext::from_sub_world(&sw);

        let res = ctx.try_resource::<Score>();
        assert!(res.is_some());
        assert_eq!(*res.unwrap(), Score(42));
    }

    #[test]
    fn system_context_try_resource_none() {
        let world = World::new();
        let sw = crate::sub_world::SubWorld::new(&world, &[]);
        let ctx = SystemContext::from_sub_world(&sw);

        assert!(ctx.try_resource::<Score>().is_none());
    }

    #[test]
    fn system_context_try_resource_mut_some() {
        let mut world = World::new();
        world.insert_resource(Score(10));
        let sw = crate::sub_world::SubWorld::new(&world, &[]);
        let ctx = SystemContext::from_sub_world(&sw);

        let res_mut = ctx.try_resource_mut::<Score>();
        assert!(res_mut.is_some());
        assert_eq!(*res_mut.unwrap(), Score(10));
    }

    #[test]
    fn system_context_try_resource_mut_none() {
        let world = World::new();
        let sw = crate::sub_world::SubWorld::new(&world, &[]);
        let ctx = SystemContext::from_sub_world(&sw);

        assert!(ctx.try_resource_mut::<Score>().is_none());
    }

    #[test]
    fn adaptive_chunk_size_small_world() {
        // < 100 entities: dynamic_min = 128, но chunk не может быть > entity_count
        assert_eq!(adaptive_chunk_size(50, 8), 50);   // 50/8=6, min(128,50)=50
        assert_eq!(adaptive_chunk_size(50, 4), 50);   // 50/4=12, min(128,50)=50
        assert_eq!(adaptive_chunk_size(1, 8), 1);     // 1/8=0,  min(128,1)=1
        assert_eq!(adaptive_chunk_size(99, 8), 99);   // 99/8=12, min(128,99)=99
    }

    #[test]
    fn adaptive_chunk_size_medium_world() {
        // 100..1000 entities: dynamic_min = 32
        assert_eq!(adaptive_chunk_size(200, 8), 32);   // 200/8=25 < 32 → 32
        assert_eq!(adaptive_chunk_size(500, 8), 62);   // 500/8=62 >= 32 → 62
        assert_eq!(adaptive_chunk_size(100, 8), 32);   // 100/8=12 < 32 → 32
    }

    #[test]
    fn adaptive_chunk_size_large_world() {
        // >= 1000 entities: dynamic_min = 64
        assert_eq!(adaptive_chunk_size(1000, 8), 125);   // 1000/8=125 >= 64 → 125
        assert_eq!(adaptive_chunk_size(10000, 8), 1250); // 10000/8=1250 → 1250
    }

    #[test]
    fn adaptive_chunk_size_single_thread() {
        // num_threads = 1 → chunk = entity_count (или dynamic_min, если entity_count мал)
        assert_eq!(adaptive_chunk_size(50, 1), 50);   // 50/1=50, min(128,50)=50
        assert_eq!(adaptive_chunk_size(200, 1), 200); // 200 >= 32 → 200
        assert_eq!(adaptive_chunk_size(1000, 1), 1000); // 1000 >= 64 → 1000
    }

    #[test]
    fn adaptive_chunk_size_max_cap() {
        // chunk не превышает DEFAULT_MAX_CHUNK_SIZE (16384)
        assert_eq!(adaptive_chunk_size(DEFAULT_MAX_CHUNK_SIZE * 2, 1), DEFAULT_MAX_CHUNK_SIZE);
        // 8 threads: 32768/8=4096 <= 16384 → cap не срабатывает
        assert_eq!(adaptive_chunk_size(DEFAULT_MAX_CHUNK_SIZE * 2, 8), 4096);
    }

    #[test]
    fn adaptive_chunk_size_transition_points() {
        // entity_count=99 (<100) → dynamic_min=128, но capped entity_count
        assert_eq!(adaptive_chunk_size(99, 8), 99);   // min(128,99)=99
        // entity_count=100 (>=100) → dynamic_min=32
        assert_eq!(adaptive_chunk_size(100, 8), 32);  // 100/8=12 < 32 → 32

        // entity_count=999 → dynamic_min=32
        assert_eq!(adaptive_chunk_size(999, 8), 124); // 999/8=124 >= 32 → 124
        // entity_count=1000 → dynamic_min=64
        assert_eq!(adaptive_chunk_size(1000, 8), 125); // 1000/8=125 >= 64 → 125
    }
}