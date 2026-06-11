use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::any::TypeId;
use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::RwLock;

use crate::{
    archetype::{Archetype, ArchetypeId},
    commands::Commands,
    component::{Component, ComponentId, ComponentInfo, ComponentRegistry, Tick},
    entity::{Entity, EntityAllocator, EntityLocation},
    events::EventRegistry,
    query::{QueryBuilder, WorldQuery},
    relations::{RelationRegistry, SubjectIndex, TargetIndex},
    resources::Resources,
    sub_world::SubWorld,
    system_param::{EventReader, EventWriter, Res, ResMut},
    template::TemplateRegistry,
};

// ── QueryCache ─────────────────────────────────────────────────

struct CacheEntry {
    arch_indices: Arc<[usize]>,
    /// Сколько архетипов мира запись уже видела (архетипы append-only —
    /// дополняем список только хвостом `archetypes[seen_arch_count..]`).
    seen_arch_count: usize,
}

/// Ключ кэша запросов. ТОЛЬКО списка `ids` недостаточно: `(Read<A>, Read<B>)`
/// и `(Read<A>, Without<B>)` дают одинаковый `fill_ids`, но разную семантику
/// matches — ключ по одним ids отравлял бы кэш между ними. Тройка
/// (ids, positive, required) однозначно задаёт матч-семантику формы:
/// without-набор = ids − positive, optional-набор = positive − required.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct QueryCacheKey {
    ids: SmallVec<[ComponentId; 8]>,
    positive: SmallVec<[ComponentId; 8]>,
    required: SmallVec<[ComponentId; 8]>,
}

impl QueryCacheKey {
    pub fn for_query<Q: WorldQuery>(world: &World, ids: &[ComponentId]) -> Self {
        let mut positive = Vec::with_capacity(Q::component_count());
        Q::fill_positive_ids(world, &mut positive);
        let mut required = Vec::with_capacity(Q::component_count());
        Q::fill_required_ids(world, &mut required);
        Self {
            ids: ids.iter().copied().collect(),
            positive: positive.into_iter().collect(),
            required: required.into_iter().collect(),
        }
    }
}

/// Кэш списков архетипов для `CachedQuery` (инкрементальный, CR-M2).
///
/// Инварианты, на которых построен:
/// - архетипы append-only (никогда не удаляются и не меняют состав) →
///   запись дополняется ТОЛЬКО новыми архетипами с индекса `seen_arch_count`;
/// - перемещение entity между архетипами список НЕ инвалидирует: какие
///   архетипы матчат запрос — свойство состава архетипа, а не его строк;
/// - пустые архетипы ВКЛЮЧАЮТСЯ в список (потребитель пропускает их на
///   итерации) — иначе entity, въехавшая в опустевший архетип, терялась бы.
pub(crate) struct QueryCache {
    entries: RwLock<FxHashMap<QueryCacheKey, CacheEntry>>,
}

impl QueryCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(FxHashMap::default()),
        }
    }

    pub fn get_or_compute(
        &self,
        key: QueryCacheKey,
        archetypes: &[Archetype],
        matches: impl Fn(&Archetype) -> bool,
    ) -> Arc<[usize]> {
        let total = archetypes.len();

        // Hit path: запись актуальна, если видела все текущие архетипы.
        {
            let map = self.entries.read().unwrap();
            if let Some(entry) = map.get(&key) {
                if entry.seen_arch_count == total {
                    return entry.arch_indices.clone();
                }
            }
        }

        let mut map = self.entries.write().unwrap();
        // Двойная проверка: другой поток мог дополнить между read и write lock.
        let (mut indices, start) = match map.get(&key) {
            Some(entry) if entry.seen_arch_count == total => {
                return entry.arch_indices.clone();
            }
            Some(entry) => (entry.arch_indices.to_vec(), entry.seen_arch_count),
            None => (Vec::new(), 0),
        };

        // Дополняем только новыми архетипами (append-only инвариант).
        indices.extend(
            archetypes[start..]
                .iter()
                .enumerate()
                .filter(|(_, arch)| matches(arch))
                .map(|(i, _)| start + i),
        );

        let arch_indices: Arc<[usize]> = indices.into();
        map.insert(
            key,
            CacheEntry {
                arch_indices: arch_indices.clone(),
                seen_arch_count: total,
            },
        );

        arch_indices
    }

    /// Полная инвалидация. Не нужна в текущей модели (архетипы append-only);
    /// останется точкой подключения despawn-компакции (CR-M4), если та
    /// когда-нибудь появится.
    #[allow(dead_code)]
    pub fn invalidate(&self) {
        self.entries.write().unwrap().clear();
    }
}

// ── ArchetypeStats ─────────────────────────────────────────────

/// Сводка [`World::archetype_stats`]: число архетипов, пустых среди них,
/// суммарные живые строки и максимум строк в одном архетипе.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchetypeStats {
    pub archetypes: usize,
    pub empty_archetypes: usize,
    pub total_rows: usize,
    pub max_rows_in_archetype: usize,
}

// ── ArchetypeKey ───────────────────────────────────────────────

/// Ключ для archetype_index — хэшируется без heap-аллокации.
/// Внутри хранит компоненты inline до 12 штук через SmallVec.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ArchetypeKey(SmallVec<[ComponentId; 12]>);

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
    pub(crate) entities: EntityAllocator,
    pub(crate) registry: ComponentRegistry,
    pub archetypes: Vec<Archetype>,
    pub(crate) archetype_index: FxHashMap<ArchetypeKey, ArchetypeId>,
    /// Индекс компонент → список архетипов, содержащих этот компонент.
    /// Используется в Query::new_with_tick для O(1) поиска архетипов-кандидатов
    /// вместо линейного обхода всех архетипов.
    pub(crate) component_arch_index: FxHashMap<ComponentId, SmallVec<[ArchetypeId; 16]>>,
    pub(crate) current_tick: Tick,
    /// База change-detection для систем: тик предыдущей границы кадра.
    /// `Changed<T>` внутри систем сравнивает change-tick строки с этим значением
    /// (продвигается планировщиком в конце кадра / `advance_change_tick`).
    pub(crate) last_run_tick: Tick,
    pub(crate) query_cache: QueryCache,
    pub(crate) relations: RelationRegistry,
    pub(crate) subject_index: SubjectIndex,
    pub(crate) target_index: TargetIndex,
    pub resources: Resources,
    pub(crate) events: EventRegistry,
    /// Реестр именованных шаблонов (EntityTemplate).
    pub(crate) templates: TemplateRegistry,
    /// Конфигурация чанкования для параллельной итерации.
    pub(crate) chunk_config: ChunkConfig,
}

impl World {
    pub fn new() -> Self {
        let mut registry = ComponentRegistry::new();
        registry.register_all_auto();
        let mut world = Self {
            entities: EntityAllocator::new(),
            registry,
            archetypes: Vec::new(),
            archetype_index: FxHashMap::default(),
            component_arch_index: FxHashMap::default(),
            current_tick: Tick(1),
            last_run_tick: Tick::ZERO,
            query_cache: QueryCache::new(),
            relations: RelationRegistry::new(),
            subject_index: SubjectIndex::new(),
            target_index: TargetIndex::new(),
            resources: Resources::new(),
            events: EventRegistry::new(),
            templates: TemplateRegistry::new(),
            chunk_config: ChunkConfig::default(),
        };
        world
            .archetypes
            .push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world
            .archetype_index
            .insert(ArchetypeKey(SmallVec::new()), ArchetypeId::EMPTY);
        world
    }

    /// Продвигает глобальный tick. **Не делает flush событий** — это ответственность Scheduler.
    /// Для использования без Scheduler вызывайте [`flush_all_events()`](Self::flush_all_events) вручную.
    pub fn tick(&mut self) {
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
    }

    /// Продвинуть change-tick на границе кадра: запомнить текущий тик как базу
    /// `Changed<T>` для следующего кадра и инкрементировать `current_tick`.
    ///
    /// Вызывается планировщиком в конце `run()`/`run_sequential()`. После этого
    /// `Changed<T>` внутри систем достоверно детектирует мутации **этого** кадра
    /// (а не «всё подряд»). На первом кадре база = `Tick::ZERO` (всё новое видно).
    #[inline]
    pub fn advance_change_tick(&mut self) {
        self.last_run_tick = self.current_tick;
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
    }

    /// База change-detection для систем (тик предыдущей границы кадра).
    #[inline]
    pub fn last_run_tick(&self) -> Tick {
        self.last_run_tick
    }

    /// Flush конкретных типов событий (по TypeId). Используется Scheduler для per-Stage flush.
    pub fn flush_events_by_type(&mut self, type_ids: &[std::any::TypeId]) {
        self.events.flush_by_type_id(type_ids);
    }

    /// Flush всех событий. Используется при работе без Scheduler.
    pub fn flush_all_events(&mut self) {
        self.events.flush_all();
    }

    /// Завершить кадр: **флаш всех событий + продвижение change-tick**.
    ///
    /// Самодостаточная замена ручной паре `flush_all_events()` + `tick()` при
    /// работе **без планировщика** (#9). Вызывайте один раз в конце каждой
    /// итерации игрового цикла:
    ///
    /// ```ignore
    /// loop {
    ///     // ... мутации, отправка событий ...
    ///     world.advance_frame(); // события видны на следующем кадре, change-tick++
    /// }
    /// ```
    ///
    /// Планировщик делает per-stage флаш сам; там это звать не нужно.
    pub fn advance_frame(&mut self) {
        self.flush_all_events();
        self.advance_change_tick();
    }

    pub fn current_tick(&self) -> Tick {
        self.current_tick
    }
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
    pub fn archetype_count(&self) -> usize {
        self.archetypes.len()
    }
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Получить текущую конфигурацию чанкования.
    #[inline]
    pub fn chunk_config(&self) -> &ChunkConfig {
        &self.chunk_config
    }

    /// Установить конфигурацию чанкования.
    #[inline]
    pub fn set_chunk_config(&mut self, config: ChunkConfig) {
        self.chunk_config = config;
    }

    /// Удалить все сущности, сохранив ресурсы, зарегистрированные компоненты и события.
    ///
    /// Аналог `World::clear()` в Bevy. Полезно для перезапуска уровня или сброса симуляции.
    /// После вызова `entity_count()` вернёт 0, но ресурсы и компоненты останутся.
    pub fn clear_entities(&mut self) {
        // Collect all entity IDs first to avoid borrow issues
        let entities: Vec<Entity> = self
            .archetypes
            .iter()
            .flat_map(|a| a.entities.iter().copied())
            .collect();
        for entity in entities {
            self.despawn(entity);
        }
    }

    pub fn register_component<T: Component>(&mut self) -> ComponentId {
        self.registry.register::<T>()
    }

    pub fn register_component_serde<T: crate::component::Serializable>(&mut self) -> ComponentId {
        self.registry.register_serde::<T>()
    }

    /// Зарегистрировать компонент с поддержкой сериализации в JSON-формате.
    pub fn register_component_serde_json<T: crate::component::Serializable>(
        &mut self,
    ) -> ComponentId {
        self.registry.register_serde_json::<T>()
    }

    pub fn registry(&self) -> &ComponentRegistry {
        &self.registry
    }

    /// Мутабельный доступ к реестру компонентов.
    pub fn registry_mut(&mut self) -> &mut ComponentRegistry {
        &mut self.registry
    }

    pub fn archetypes(&self) -> &[Archetype] {
        &self.archetypes
    }

    /// Сводка по архетипам — дебаг/профилирование (CR-M4).
    ///
    /// Пустые архетипы не переиспользуются под другой состав и не компактируются
    /// (append-only инвариант дешевле; слот по СОВПАДАЮЩЕМУ составу переиспользуется
    /// через archetype_index). Эта сводка — инструмент наблюдения за их числом.
    pub fn archetype_stats(&self) -> ArchetypeStats {
        let mut stats = ArchetypeStats {
            archetypes: self.archetypes.len(),
            ..Default::default()
        };
        for arch in &self.archetypes {
            let rows = arch.len();
            stats.total_rows += rows;
            if rows == 0 {
                stats.empty_archetypes += 1;
            }
            stats.max_rows_in_archetype = stats.max_rows_in_archetype.max(rows);
        }
        stats
    }

    pub fn relation_registry(&self) -> &RelationRegistry {
        &self.relations
    }

    pub fn relation_registry_mut(&mut self) -> &mut RelationRegistry {
        &mut self.relations
    }

    /// Публичная обёртка над pub(crate) insert_raw — для apex-serialization.
    ///
    /// Вставить raw байты компонента в entity. Используется при restore
    /// когда тип компонента неизвестен статически.
    #[inline]
    pub fn insert_raw_pub(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        data: Vec<u8>,
        tick: Tick,
    ) {
        self.insert_raw(entity, component_id, data, tick);
    }

    // ── Параллельный доступ ────────────────────────────────────

    /// # Safety
    /// Вызывающий гарантирует отсутствие structural changes
    /// и корректность AccessDescriptor всех параллельных систем.
    pub unsafe fn as_parallel_world(&self) -> ParallelWorld<'_> {
        ParallelWorld {
            world: self as *const World,
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

    /// Зарезервировать capacity для событий по TypeId.
    ///
    /// Вызывается планировщиком на основе `AccessDescriptor::event_reserve()`.
    pub fn event_reserve_by_type(&mut self, type_id: TypeId, capacity: usize) {
        self.events.reserve_by_type(type_id, capacity);
    }

    pub fn event_queue_ptr<T: Send + Sync + 'static>(
        &self,
    ) -> Option<*mut crate::events::Events<T>> {
        self.events.get_raw_ptr::<T>()
    }

    /// Создать читатель событий с per-reader курсором.
    ///
    /// Аналог `EventReader::new(world.events_mut::<T>())`.
    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        unsafe {
            let ptr = self
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    /// Создать писатель событий.
    ///
    /// Аналог `EventWriter::from_ptr(...)`.
    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered");
            EventWriter::from_ptr(ptr)
        }
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
            let row = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
            self.entities.set_location(
                entity,
                EntityLocation {
                    archetype_id: ArchetypeId::EMPTY,
                    row,
                },
            );
            return entity;
        }
        // Обычный путь
        let archetype_id = self.get_or_create_archetype(&ids);
        let entity = self.entities.allocate();
        let row = self.archetypes[archetype_id.0 as usize].entities.len();
        let tick = self.current_tick;
        self.archetypes[archetype_id.0 as usize]
            .entities
            .push(entity);
        bundle.write_into(self, archetype_id, row, tick);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id,
                row: row as u32,
            },
        );
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
        if count == 0 {
            return Vec::new();
        }

        let probe = make_bundle(0);
        let ids = probe.component_ids(&mut self.registry);
        drop(probe);

        let archetype_id = self.get_or_create_archetype(&ids);
        let arch_idx = archetype_id.0 as usize;
        let start_row = self.archetypes[arch_idx].entities.len();
        let tick = self.current_tick;

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
        let col_indices: SmallVec<[usize; 8]> = ids
            .iter()
            .filter_map(|&id| self.archetypes[arch_idx].column_index(id))
            .collect();

        // Порог: для 1 компонента per-entity loop быстрее, чем bulk copy.
        // Для 2+ компонентов bulk copy из первой строки выигрывает за счёт
        // устранения 10,000 вызовов make_bundle и 40,000 поисков в col_indices.
        // SAFETY: bulk-copy через copy_nonoverlapping допустим только для типов,
        // не имеющих Drop (эквивалентно Copy). Для типов с Drop (String, Vec<T>, Arc<T>)
        // используется per-entity цикл во избежание двойного освобождения.
        let needs_drop = B::needs_drop();
        if col_indices.len() <= 1 || needs_drop {
            // Per-entity loop — старый подход, быстрее для малого числа компонентов.
            for (i, &entity) in entities.iter().enumerate() {
                let row = start_row + i;
                let bundle = make_bundle(i);
                self.archetypes[arch_idx].entities.push(entity);
                bundle.write_into_batch(self, archetype_id, row, tick, &col_indices);
            }
        } else {
            // Первая entity — пишем через штатный write_into_batch (создаёт "шаблон" строки).
            // Для Copy-компонентов это позволяет нам затем bulk-копировать данные из первой
            // строки во все последующие, избегая 10,000 вызовов make_bundle и 40,000 поисков
            // в col_indices.iter().find() (как в Legion SOA подходе).
            let first_entity = entities[0];
            let first_bundle = make_bundle(0);
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

        self.entities
            .set_locations_batch(&entities, archetype_id, start_row as u32);
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
    /// # #[derive(Component)] struct Health(f32);
    /// # #[derive(Component)] struct Armor(f32);
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
                    col.write_at(
                        location.row as usize,
                        &component as *const T as *const u8,
                        tick,
                    );
                }
            }
            std::mem::forget(component);
            return;
        }

        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        let tick = self.current_tick;
        unsafe {
            self.archetypes[new_arch_id.0 as usize].write_component(
                new_row as usize,
                component_id,
                &component as *const T as *const u8,
                tick,
            );
        }
        std::mem::forget(component);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
    }

    /// Вставить компонент по raw данным.
    pub(crate) fn insert_raw(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        data: Vec<u8>,
        tick: Tick,
    ) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return,
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
        let new_row = self.move_entity(entity, location, new_arch_id);
        unsafe {
            self.archetypes[new_arch_id.0 as usize].write_component(
                new_row as usize,
                component_id,
                data.as_ptr(),
                tick,
            );
        }
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
    }

    /// Удалить компонент по raw ComponentId.
    pub(crate) fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return,
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(component_id) {
            return;
        }
        let new_arch_id =
            self.find_or_create_archetype_without(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
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
        let new_arch_id =
            self.find_or_create_archetype_without(location.archetype_id, component_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype_id: new_arch_id,
                row: new_row,
            },
        );
        true
    }

    /// Удалить entity и ВСЕ её связи (как subject и как target).
    ///
    /// Для видов связи с `cascade_delete_on_target_despawn()` (например,
    /// `ChildOf`) subjects деспавнятся каскадом — итеративно, без рекурсии.
    /// Для остальных видов пары вычищаются из индексов: ни одна связь не
    /// переживает свой target (generation-честность TargetIndex).
    pub fn despawn(&mut self, entity: Entity) -> bool {
        if !self.entities.is_alive(entity) {
            return false;
        }
        let mut stack: SmallVec<[Entity; 8]> = SmallVec::new();
        stack.push(entity);

        while let Some(cur) = stack.pop() {
            if !self.entities.is_alive(cur) {
                continue; // уже снесён каскадом по другому пути
            }

            // ── Связи, где cur — target ────────────────────────
            if self.target_index.has_target(cur.index) {
                for kind_idx in 0..self.relations.kind_count() as u32 {
                    let Some(subjects) = self.target_index.take_subjects(kind_idx, cur.index)
                    else {
                        continue;
                    };
                    let pair = crate::relations::RelationPair {
                        kind_idx,
                        target: cur,
                    };
                    for &s in &subjects {
                        self.subject_index.remove(s.index, pair);
                    }
                    if self.relations.is_cascade(kind_idx) {
                        stack.extend(subjects);
                    }
                }
            }

            // ── Связи, где cur — subject ───────────────────────
            for pair in self.subject_index.take_all(cur.index) {
                self.target_index
                    .remove(pair.kind_idx, pair.target.index, cur);
            }

            // ── Строка хранилища ───────────────────────────────
            let location = match self.entities.get_location(cur) {
                Some(loc) => loc,
                None => {
                    self.entities.free(cur);
                    continue;
                }
            };
            let arch_idx = location.archetype_id.0 as usize;
            unsafe {
                if let Some(displaced) =
                    self.archetypes[arch_idx].remove_row(location.row as usize)
                {
                    self.entities.set_location(
                        displaced,
                        EntityLocation {
                            archetype_id: location.archetype_id,
                            row: location.row,
                        },
                    );
                }
            }
            self.entities.free(cur);
        }
        true
    }

    // ── Read / Write ───────────────────────────────────────────

    #[inline]
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let component_id = self.registry.get_id::<T>()?;
        let location = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component::<T>(location.row as usize, component_id)
        }
    }

    /// Мутабельный доступ с обновлением change-tick строки (change detection).
    ///
    /// Стампит текущий тик мира → `Changed<T>` срабатывает (как и при мутации
    /// через `Query<&mut T>`/`Write<T>`, C1).
    #[inline]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let component_id = self.registry.get_id::<T>()?;
        self.get_mut_by_id(entity, component_id)
    }

    // ── Random-access fast path (CR-M3) ────────────────────────
    //
    // Горячие циклы (анимация: ~22k get_mut/кадр) берут ComponentId ОДИН раз
    // на проход через `component_id::<T>()` и дальше ходят `get_by_id`/
    // `get_mut_by_id` — без TypeId-hash на каждый вызов.

    /// ComponentId типа `T`, если тот зарегистрирован.
    #[inline]
    pub fn component_id<T: Component>(&self) -> Option<ComponentId> {
        self.registry.get_id::<T>()
    }

    /// `get` по заранее взятому ComponentId (см. [`component_id`](Self::component_id)).
    ///
    /// `component_id` обязан соответствовать `T` (debug_assert).
    #[inline]
    pub fn get_by_id<T: Component>(&self, entity: Entity, component_id: ComponentId) -> Option<&T> {
        debug_assert_eq!(
            self.registry.get_id::<T>(),
            Some(component_id),
            "get_by_id: ComponentId не соответствует T"
        );
        let location = self.entities.get_location(entity)?;
        unsafe {
            self.archetypes[location.archetype_id.0 as usize]
                .get_component::<T>(location.row as usize, component_id)
        }
    }

    /// `get_mut` по заранее взятому ComponentId — со стампом change-tick,
    /// как и [`get_mut`](Self::get_mut).
    #[inline]
    pub fn get_mut_by_id<T: Component>(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Option<&mut T> {
        debug_assert_eq!(
            self.registry.get_id::<T>(),
            Some(component_id),
            "get_mut_by_id: ComponentId не соответствует T"
        );
        let location = self.entities.get_location(entity)?;
        let tick = self.current_tick;
        let row = location.row as usize;

        let arch = &mut self.archetypes[location.archetype_id.0 as usize];
        let col_idx = arch.column_index(component_id)?;
        let col = &mut arch.columns[col_idx];
        if row < col.change_ticks.len() {
            col.change_ticks[row] = tick;
        }
        unsafe { Some(col.get_mut::<T>(row)) }
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.entities.is_alive(entity)
    }

    /// Проверить, есть ли у сущности компонент `T`.
    ///
    /// O(1) после первого вызова для данного archetype (column_index кешируется).
    #[inline]
    pub fn has_component<T: Component>(&self, entity: Entity) -> bool {
        let Some(cid) = self.registry.get_id::<T>() else {
            return false;
        };
        let Some(loc) = self.entities.get_location(entity) else {
            return false;
        };
        self.archetypes[loc.archetype_id.0 as usize].has_component(cid)
    }

    // ── Query API ──────────────────────────────────────────────

    pub fn query_typed<Q: WorldQuery>(&self) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, Tick::ZERO)
    }

    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, last_run)
    }

    pub fn query(&self) -> QueryBuilder<'_> {
        QueryBuilder::new(self)
    }

    // ── Внутренние методы ──────────────────────────────────────

    pub(crate) fn find_or_create_archetype_with(
        &mut self,
        current: ArchetypeId,
        add: ComponentId,
    ) -> ArchetypeId {
        if let Some(&id) = self.archetypes[current.0 as usize].add_edges.get(&add) {
            return id;
        }
        let mut new_components: Vec<ComponentId> = self.archetypes[current.0 as usize]
            .component_ids
            .iter()
            .copied()
            .collect();
        new_components.push(add);
        new_components.sort_unstable();
        let new_id = self.get_or_create_archetype(&new_components);
        self.archetypes[current.0 as usize]
            .add_edges
            .insert(add, new_id);
        self.archetypes[new_id.0 as usize]
            .remove_edges
            .insert(add, current);
        new_id
    }

    pub(crate) fn find_or_create_archetype_without(
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
        let new_id = self.get_or_create_archetype(&new_components);
        self.archetypes[current.0 as usize]
            .remove_edges
            .insert(remove, new_id);
        self.archetypes[new_id.0 as usize]
            .add_edges
            .insert(remove, current);
        new_id
    }

    #[inline(never)]
    pub(crate) fn get_or_create_archetype(&mut self, components: &[ComponentId]) -> ArchetypeId {
        // Borrow<[ComponentId]> — zero-copy lookup без создания ArchetypeKey
        if let Some(&id) = self.archetype_index.get(components) {
            return id;
        }
        let id = ArchetypeId(self.archetypes.len() as u32);
        let infos: Vec<&ComponentInfo> = components
            .iter()
            .filter_map(|&cid| self.registry.get_info(cid))
            .collect();
        let arch = Archetype::new(id, components.iter().copied().collect(), &infos);
        for &cid in &arch.component_ids {
            self.component_arch_index.entry(cid).or_default().push(id);
        }
        self.archetypes.push(arch);
        self.archetype_index
            .insert(ArchetypeKey::from(components), id);
        // QueryCache не инвалидируем: новые архетипы записи кэша подхватывают
        // инкрементально (seen_arch_count), перемещения entity список не меняют.
        id
    }

    pub(crate) fn move_entity(
        &mut self,
        entity: Entity,
        from_location: EntityLocation,
        to_archetype_id: ArchetypeId,
    ) -> u32 {
        let from_idx = from_location.archetype_id.0 as usize;
        let to_idx = to_archetype_id.0 as usize;
        let from_row = from_location.row as usize;

        let to_row = self.archetypes[to_idx].entities.len();
        self.archetypes[to_idx].entities.push(entity);

        // Единственный проход: для каждой колонки из исходного архетипа
        // определяем наличие в целевом и сразу копируем или дропаем.
        let from_len = self.archetypes[from_idx].columns.len();

        for i in 0..from_len {
            let cid = self.archetypes[from_idx].columns[i].component_id;
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
                    self.archetypes[to_idx].columns[to_col_idx]
                        .change_ticks
                        .push(tick);
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
        let from_last = self.archetypes[from_idx].entities.len() - 1;
        if from_row != from_last {
            let displaced = self.archetypes[from_idx].entities[from_last];
            self.archetypes[from_idx].entities.swap(from_row, from_last);
            self.archetypes[from_idx].entities.pop();
            self.entities.set_location(
                displaced,
                EntityLocation {
                    archetype_id: from_location.archetype_id,
                    row: from_row as u32,
                },
            );
        } else {
            self.archetypes[from_idx].entities.pop();
        }

        to_row as u32
    }

    // ── Оптимизация 4.1: add_relation_batch ───────────────────

    /// Batch-добавление одинаковой relation от множества субъектов к одному target.
    ///
    /// После CR-M1 relations не входят в идентичность архетипа, поэтому это
    /// просто bulk-вставка в индексы — O(S), без структурных изменений мира.
    pub fn add_relation_batch<R: crate::relations::RelationKind>(
        &mut self,
        subjects: &[Entity],
        _kind: R,
        target: Entity,
    ) {
        if subjects.is_empty() {
            return;
        }
        let kind_idx = self.relations.get_or_register::<R>();
        for &subject in subjects {
            self.add_relation_by_kind_idx(subject, kind_idx, target);
        }
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

// ── MainWorld ──────────────────────────────────────────────────

/// Wraps a [`World`] for temporary insertion as a resource.
///
/// Used by extract systems to read the main world while running on the
/// render world's scheduler. Bevy-compatible pattern.
///
/// # Safety
/// `Send + Sync` are safe because World is only accessed through
/// the resource system with proper scheduler synchronization.
pub struct MainWorld(pub World);

impl MainWorld {
    pub fn world(&self) -> &World {
        &self.0
    }
}

// SAFETY: MainWorld access is guarded by scheduler's sequential extract stage.
unsafe impl Send for MainWorld {}
unsafe impl Sync for MainWorld {}

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

pub const DEFAULT_MAX_CHUNK_SIZE: usize = 65536;

/// Конфигурация стратегии параллельного чанкования.
///
/// Определяет, как [`adaptive_chunk_size`] разбивает entity на чанки
/// для параллельной итерации (`par_for_each`).
///
/// Передаётся через `World::set_chunk_config()`. Если не задана явно,
/// используется [`ChunkConfig::default()`].
///
/// # Пример
///
/// ```ignore
/// let config = ChunkConfig {
///     min_entities_per_thread: 32,
///     max_chunk_size: 8192,
///     auto_serial_fallback: true,
/// };
/// world.set_chunk_config(config);
/// ```
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Минимальное число entity на поток, ниже которого параллелизм не выгоден.
    /// Для 8 потоков с `min = 16` миры до 128 entity идут в один чанк (serial).
    ///
    /// Default: 16.
    pub min_entities_per_thread: usize,

    /// Динамический минимум размера чанка — защита от микро-задач rayon.
    /// Если вычисленный размер чанка меньше этого значения — поднимается до него.
    ///
    /// Default: 128/32/64 (зависит от размера мира, как до рефакторинга).
    pub dynamic_min_chunk: usize,

    /// Максимальный размер чанка (ограничитель роста при огромных мирах).
    ///
    /// Default: 65536 (или из `PAR_CHUNK_SIZE`, если задан).
    pub max_chunk_size: usize,

    /// Если `true` — всегда использовать один чанк для `N < min_entities_per_thread * threads`.
    /// Если `false` — всегда разбивать на `threads` чанков (даже мелких).
    ///
    /// Default: `true`.
    pub auto_serial_fallback: bool,

    /// Во сколько раз больше задач создавать чем потоков Rayon.
    /// 1.0 = ровно num_threads задач, 2.0 = вдвое больше.
    /// Больше задач → лучше work-stealing, но больше per-task overhead.
    ///
    /// Default: `2.0`.
    pub task_multiplier: f32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        let max_from_env = {
            let user = PAR_CHUNK_SIZE.load(std::sync::atomic::Ordering::Relaxed);
            if user > 0 {
                user
            } else {
                DEFAULT_MAX_CHUNK_SIZE
            }
        };
        Self {
            min_entities_per_thread: 16,
            dynamic_min_chunk: 64,
            max_chunk_size: max_from_env,
            auto_serial_fallback: true,
            task_multiplier: 2.0,
        }
    }
}

/// Вычислить адаптивный размер чанка на основе количества entity и конфигурации.
///
/// Логика (с учётом `task_multiplier` для work-stealing Rayon):
/// 1. Если `auto_serial_fallback` и `entity_count < min_entities_per_thread * thread_count` — один чанк (serial).
/// 2. Иначе — `ceil(entity_count / thread_count / task_multiplier)`, зажато в `[dynamic_min_chunk, max_chunk_size]`.
pub fn adaptive_chunk_size(entity_count: usize, num_threads: usize, config: &ChunkConfig) -> usize {
    if entity_count == 0 {
        return 1;
    }
    let n = num_threads.max(1);
    let serial_threshold = config.min_entities_per_thread.saturating_mul(n);
    if config.auto_serial_fallback && entity_count < serial_threshold {
        return entity_count;
    }
    let targets = if n > 1 {
        (n as f32 * config.task_multiplier).ceil() as usize
    } else {
        1
    };
    let raw = entity_count.div_ceil(targets);
    raw.clamp(config.dynamic_min_chunk, config.max_chunk_size)
        .min(entity_count)
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

pub struct SystemContext<'w> {
    /// SubWorld'ы, которые видит эта система.
    /// Обычно один SubWorld, но может быть несколько если система
    /// работает с несколькими группами архетипов.
    pub(crate) sub_worlds: &'w [crate::sub_world::SubWorld<'w>],
    /// Thread-local команды. Указатель на `Vec<Commands>` из Scheduler.
    /// Каждый поток rayon имеет свой `Commands` по индексу `current_thread_index()`.
    /// Если не предоставлен — используется `inline_cmds`.
    pub(crate) deferred_cmds: Option<*mut Vec<Commands>>,
    /// Локальный Commands для sequential систем или когда deferred_cmds не задан.
    /// Используется вместо глобального статического `DUMMY_COMMANDS`.
    pub(crate) inline_cmds: UnsafeCell<Commands>,
}

unsafe impl Send for SystemContext<'_> {}
unsafe impl Sync for SystemContext<'_> {}

impl<'w> SystemContext<'w> {
    pub fn new(sub_worlds: &'w [crate::sub_world::SubWorld<'w>]) -> Self {
        Self {
            sub_worlds,
            deferred_cmds: None,
            inline_cmds: UnsafeCell::new(Commands::new()),
        }
    }

    /// Создаёт SystemContext из одного SubWorld (наиболее частый случай).
    pub fn from_sub_world(sub_world: &'w crate::sub_world::SubWorld<'w>) -> Self {
        Self {
            sub_worlds: std::slice::from_ref(sub_world),
            deferred_cmds: None,
            inline_cmds: UnsafeCell::new(Commands::new()),
        }
    }

    /// Создать контекст с thread-local командами.
    pub fn with_commands(
        sub_worlds: &'w [crate::sub_world::SubWorld<'w>],
        deferred_cmds: *mut Vec<Commands>,
    ) -> Self {
        Self {
            sub_worlds,
            deferred_cmds: Some(deferred_cmds),
            inline_cmds: UnsafeCell::new(Commands::new()),
        }
    }

    /// Получить `Commands` для текущего потока.
    /// Команды применяются планировщиком после завершения Stage.
    ///
    /// `&self → &mut Commands` намеренно: буфер команд — thread-local (по индексу
    /// rayon-потока) либо `UnsafeCell` в sequential-режиме, поэтому уникальность
    /// `&mut` гарантирована без `&mut self`. Это позволяет нескольким системам
    /// в одном батче складывать команды параллельно через общий `&SystemContext`.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn commands(&self) -> &mut Commands {
        if let Some(deferred_cmds) = self.deferred_cmds {
            unsafe {
                let thread_idx = rayon::current_thread_index().unwrap_or(0);
                let vec = &mut *deferred_cmds;
                return &mut vec[thread_idx];
            }
        }
        // SAFETY: inline_cmds используется только когда deferred_cmds не задан
        // (sequential режим). В этом случае доступ exclusive — один поток.
        unsafe { &mut *self.inline_cmds.get() }
    }

    /// Получить World (для обратной совместимости).
    /// Используется для query, resource, event доступа.
    fn world(&self) -> &'w World {
        self.sub_worlds[0].world()
    }

    #[inline]
    pub fn query<Q: WorldQuery>(&self) -> CachedQuery<'_, Q> {
        // База change-detection — `last_run_tick` мира (граница прошлого кадра),
        // так `Changed<T>` внутри системы достоверен (TD-9), а не «всё подряд».
        let last_run = self.sub_worlds[0].world().last_run_tick();
        CachedQuery::from_sub_world(&self.sub_worlds[0], last_run)
    }

    #[inline]
    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> CachedQuery<'_, Q> {
        CachedQuery::from_sub_world(&self.sub_worlds[0], last_run)
    }

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(self.world().resource::<T>())
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = self
                .world()
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
            let ptr = self
                .world()
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self
                .world()
                .event_queue_ptr::<T>()
                .expect("event_writer: event type not registered");
            EventWriter::from_ptr(ptr)
        }
    }

    #[inline]
    pub fn entity_count(&self) -> usize {
        self.world().entity_count()
    }

    /// Извлечь параметры через трейт [`SystemParam`](crate::system_param::SystemParam).
    ///
    /// ```ignore
    /// type Params = (ResRead<DeltaTime>, QueryParam<(Read<Vel>, Write<Pos>)>);
    /// let (dt, q) = ctx.fetch::<Params>();
    /// ```
    #[inline]
    pub fn fetch<P: crate::system_param::SystemParam>(&self) -> P::Item<'_> {
        P::fetch(self)
    }

    // Итерация только через ctx.query::<Q>().for_each(...)
    // или ctx.query::<Q>().par_for_each(...)
}

// ── Relations API on SystemContext ─────────────────────────────────

impl<'w> SystemContext<'w> {
    /// Запрос по relation: найти все entity с relation `R` к `target`,
    /// у которых также есть компоненты `Q`.
    #[inline]
    pub fn query_relation<R: crate::relations::RelationKind, Q: WorldQuery>(
        &self,
        _kind: R,
        target: Entity,
    ) -> crate::relations::RelationIter<'_, Q> {
        self.world().query_relation::<R, Q>(_kind, target)
    }

    /// Wildcard-запрос: найти все entity с любым relation вида `R`,
    /// у которых также есть компоненты `Q`.
    #[inline]
    pub fn query_wildcard<R: crate::relations::RelationKind, Q: WorldQuery>(
        &self,
        _kind: R,
    ) -> crate::relations::RelationIter<'_, Q> {
        self.world().query_wildcard::<R, Q>(_kind)
    }

    /// Все entity, связанные relation `R` с `parent`.
    #[inline]
    pub fn children_of<R: crate::relations::RelationKind>(
        &self,
        _kind: R,
        parent: Entity,
    ) -> impl Iterator<Item = Entity> + '_ {
        self.world().children_of(_kind, parent)
    }

    /// Проверить наличие relation `R` между `subject` и `target`.
    #[inline]
    pub fn has_relation<R: crate::relations::RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
        target: Entity,
    ) -> bool {
        self.world().has_relation(subject, _kind, target)
    }

    /// Найти target entity, с которым `subject` связан relation `R`.
    #[inline]
    pub fn get_relation_target<R: crate::relations::RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
    ) -> Option<Entity> {
        self.world().get_relation_target(subject, _kind)
    }
}

// ── ParallelWorld ──────────────────────────────────────────────

pub struct ParallelWorld<'w> {
    pub(crate) world: *const World,
    pub(crate) _marker: std::marker::PhantomData<&'w World>,
}

unsafe impl Send for ParallelWorld<'_> {}
unsafe impl Sync for ParallelWorld<'_> {}

impl<'w> ParallelWorld<'w> {
    #[inline]
    pub unsafe fn get(&self) -> &'w World {
        &*self.world
    }
}

// ── CachedQuery ────────────────────────────────────────────────

pub struct CachedQuery<'w, Q: WorldQuery> {
    world: &'w World,
    arch_indices: Arc<[usize]>,
    last_run: Tick,
    cached_ids: Vec<ComponentId>,
    row_ranges: &'w [(usize, usize, usize)],
    _phantom: std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> CachedQuery<'w, Q> {
    pub fn new(world: &'w World, last_run: Tick) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        let arch_indices = if ids.len() == Q::component_count() {
            let key = QueryCacheKey::for_query::<Q>(world, &ids);
            world
                .query_cache
                .get_or_compute(key, &world.archetypes, |arch| {
                    Q::matches_archetype(arch, &ids)
                })
        } else {
            Arc::new([])
        };

        Self {
            world,
            arch_indices,
            last_run,
            cached_ids: ids,
            row_ranges: &[],
            _phantom: std::marker::PhantomData,
        }
    }

    /// Создать CachedQuery с ограничением на архетипы и строки из SubWorld.
    ///
    /// Не вызывает `get_or_compute` (thread-safe для параллельных систем).
    /// Фильтрация по `Q::matches_archetype` происходит в `for_each`/`par_for_each`
    /// — там `fetch_state` вызывается только для совпадающих архетипов.
    pub fn from_sub_world(sub: &'w SubWorld<'w>, last_run: Tick) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(sub.world(), &mut ids);

        let arch_indices: Arc<[usize]> = sub.archetype_indices().into();
        let row_ranges: &'w [(usize, usize, usize)] = sub.row_ranges();

        Self {
            world: sub.world(),
            arch_indices,
            last_run,
            cached_ids: ids,
            row_ranges,
            _phantom: std::marker::PhantomData,
        }
    }

    fn row_range(&self, arch_idx: usize) -> (usize, usize) {
        self.row_ranges
            .iter()
            .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
            .unwrap_or((0, usize::MAX))
    }

    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        let ids = &self.cached_ids;
        if ids.len() != Q::component_count() {
            return;
        }
        for &arch_idx in self.arch_indices.as_ref() {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() {
                continue;
            }
            if !Q::matches_archetype(arch, ids) {
                continue;
            }
            let state = unsafe { Q::fetch_state(arch, ids, self.last_run, self.world.current_tick()) };
            let (row_start, row_end) = self.row_range(arch_idx);
            let end = row_end.min(arch.len());
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let entities = &arch.entities[row_start..end];
            for (offset, &entity) in entities.iter().enumerate() {
                let row = row_start + offset;
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entity, item);
                }
            }
        }
    }

    /// Параллельная итерация.
    pub fn par_for_each<F>(&self, f: F)
    where
        Q: Send,
        F: Fn(Entity, Q::Item<'_>) + Send + Sync,
    {
        use crate::par_utils::compute_par_chunks;
        use rayon::prelude::*;
        let num_threads = rayon::current_num_threads();

        // ids — owned clone, как в Query::par_for_each (не ссылка на &self)
        let ids = self.cached_ids.clone();
        if ids.len() != Q::component_count() {
            return;
        }

        let world = self.world;
        let last_run = self.last_run;
        let row_ranges = self.row_ranges;
        let rr = |arch_idx: usize| -> (usize, usize) {
            row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX))
        };
        let chunks = compute_par_chunks(
            self.arch_indices
                .iter()
                .copied()
                .filter(|&arch_idx| !world.archetypes[arch_idx].is_empty())
                .filter(|&arch_idx| Q::matches_archetype(&world.archetypes[arch_idx], &ids))
                .map(|arch_idx| {
                    let s = rr(arch_idx);
                    let effective_len =
                        s.1.min(world.archetypes[arch_idx].len())
                            .saturating_sub(s.0);
                    (arch_idx, effective_len)
                }),
            num_threads,
            world.chunk_config(),
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let (r_start, r_end) = rr(arch_idx);
            let clamped_start = r_start + start;
            let clamped_end = (r_start + end).min(r_end);
            if clamped_start >= clamped_end {
                return;
            }
            let arch = unsafe { &*world.archetypes.as_ptr().add(arch_idx) };
            let state = unsafe { Q::fetch_state(arch, &ids, last_run, world.current_tick()) };
            let entities = &arch.entities[clamped_start..clamped_end];
            for (offset, &entity) in entities.iter().enumerate() {
                let row = clamped_start + offset;
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entity, item);
                }
            }
        });
    }

    pub fn len(&self) -> usize {
        self.arch_indices
            .iter()
            .map(|&i| self.world.archetypes[i].len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.arch_indices
            .iter()
            .all(|&i| self.world.archetypes[i].is_empty())
    }

    /// Создать `Iterator` по (Entity, компонентам).
    ///
    /// В отличие от `for_each`, возвращает стандартный Rust-итератор.
    /// `fetch_state` вызывается лениво — только при переходе на новый архетип.
    #[inline]
    pub fn iter(&self) -> CachedQueryIter<'w, Q> {
        CachedQueryIter {
            world: self.world,
            arch_indices: self.arch_indices.clone(),
            cached_ids: self.cached_ids.clone(),
            last_run: self.last_run,
            row_ranges: self.row_ranges,
            arch_pos: 0,
            row: 0,
            row_end: 0,
            entities: std::ptr::null(),
            state: None,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Lazy-итератор для `CachedQuery`.
///
/// Вызывает `fetch_state` только при переходе на новый архетип,
/// а не при создании — в отличие от `QueryIter`.
pub struct CachedQueryIter<'w, Q: WorldQuery> {
    world: &'w World,
    arch_indices: Arc<[usize]>,
    cached_ids: Vec<ComponentId>,
    last_run: Tick,
    row_ranges: &'w [(usize, usize, usize)],

    arch_pos: usize,
    row: usize,
    row_end: usize,
    entities: *const Entity,
    state: Option<Q::State>,
    _phantom: std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> Iterator for CachedQueryIter<'w, Q> {
    type Item = (Entity, Q::Item<'w>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Текущий архетип исчерпан — настраиваем следующий совпадающий.
            while self.row >= self.row_end {
                if !self.advance_archetype() {
                    return None;
                }
            }

            let row = self.row;
            self.row += 1;

            let entity = unsafe { *self.entities.add(row) };
            let item = unsafe { Q::fetch_item(*self.state.as_ref().unwrap(), row) };

            if let Some(item) = item {
                return Some((entity, item));
            }
        }
    }
}

impl<'w, Q: WorldQuery> CachedQueryIter<'w, Q> {
    /// Настроить следующий непустой совпадающий архетип (начиная с `arch_pos`).
    /// Возвращает `false`, когда архетипы кончились. Пропускает пустые/не-matching
    /// в цикле, поэтому первый архетип (индекс 0) НЕ теряется (см. TD-1).
    fn advance_archetype(&mut self) -> bool {
        while self.arch_pos < self.arch_indices.len() {
            let arch_idx = self.arch_indices[self.arch_pos];
            self.arch_pos += 1; // потребляем текущий индекс

            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() || !Q::matches_archetype(arch, &self.cached_ids) {
                continue;
            }

            let (r_start, r_end) = self
                .row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX));
            let end = r_end.min(arch.len());
            if end <= r_start {
                continue;
            }

            self.state = Some(unsafe {
                Q::fetch_state(arch, &self.cached_ids, self.last_run, self.world.current_tick())
            });
            self.row = r_start;
            self.row_end = end;
            self.entities = arch.entities.as_ptr();
            return true;
        }
        false
    }
}

// ── Bundle ─────────────────────────────────────────────────────

pub trait Bundle: Sized {
    fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]>;

    /// Записать ComponentId'ы напрямую в `out` — без создания промежуточных SmallVec.
    fn push_component_ids(
        &self,
        registry: &mut ComponentRegistry,
        out: &mut SmallVec<[ComponentId; 8]>,
    ) {
        out.extend(self.component_ids(registry));
    }

    fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick);

    /// Количество компонентов в этом Bundle (статически, для разбивки col_indices).
    fn component_count() -> usize;

    /// Пакетная запись компонентов с предвычисленными column indices.
    ///
    /// По умолчанию вызывает `write_into`. Переопределяется для оптимизации:
    /// использует переданные `col_indices` вместо повторного
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

    /// Возвращает true, если хотя бы один компонент Bundle имеет Drop (нужно для spawn_many).
    ///
    /// Для типов с Drop bulk-copy через `copy_nonoverlapping` небезопасен,
    /// используется per-entity цикл.
    fn needs_drop() -> bool {
        false
    }
}

// ── Blanket impl: любой Component является Bundle (из одного компонента) ──

impl<T: Component> Bundle for T {
    #[inline(always)]
    fn component_count() -> usize {
        1
    }

    #[inline(always)]
    fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
        smallvec::smallvec![registry.get_or_register::<T>()]
    }

    #[inline(always)]
    fn push_component_ids(
        &self,
        registry: &mut ComponentRegistry,
        out: &mut SmallVec<[ComponentId; 8]>,
    ) {
        out.push(registry.get_or_register::<T>());
    }

    #[inline(always)]
    fn write_into(self, world: &mut World, archetype_id: ArchetypeId, row: usize, tick: Tick) {
        let cid = world.registry.get_or_register::<T>();
        if let Some(ci) = world.archetypes[archetype_id.0 as usize].column_index(cid) {
            unsafe {
                let col = &mut world.archetypes[archetype_id.0 as usize].columns[ci];
                if col.item_size > 0 {
                    if col.len >= col.capacity {
                        col.grow();
                    }
                    let dst = col.get_ptr(row);
                    std::ptr::copy_nonoverlapping(
                        &self as *const T as *const u8,
                        dst,
                        col.item_size,
                    );
                }
                col.change_ticks.push(tick);
                col.len += 1;
            }
        }
        std::mem::forget(self);
    }

    #[inline(always)]
    fn write_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        tick: Tick,
        col_indices: &[usize],
    ) {
        let col_idx = col_indices[0];
        unsafe {
            let col = &mut world.archetypes[archetype_id.0 as usize].columns[col_idx];
            if col.item_size > 0 {
                if col.len >= col.capacity {
                    col.grow();
                }
                let dst = col.get_ptr(row);
                std::ptr::copy_nonoverlapping(&self as *const T as *const u8, dst, col.item_size);
            }
            col.change_ticks.push(tick);
            col.len += 1;
        }
        std::mem::forget(self);
    }

    #[inline(always)]
    fn needs_drop() -> bool {
        std::mem::needs_drop::<T>()
    }
}

// ── Рекурсивный impl_bundle! для кортежей Bundle ──
//
// Элементы кортежа — любые Bundle (компоненты, другие Bundle-структуры, кортежи).
// Число аритетей — 12 (как Bevy).

macro_rules! impl_bundle {
    ($($T:ident),+) => {
        #[allow(non_snake_case)]
        impl<$($T: Bundle),+> Bundle for ($($T,)+) {
            #[inline]
            fn component_count() -> usize {
                0usize $( + $T::component_count() )+
            }

            #[inline]
            fn component_ids(&self, registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
                let mut ids = smallvec::SmallVec::<[ComponentId; 8]>::new();
                #[allow(non_snake_case)]
                let ($($T,)+) = self;
                $( $T.push_component_ids(registry, &mut ids); )+
                ids.sort_unstable();
                ids
            }

            #[inline]
            fn write_into(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
            ) {
                #[allow(non_snake_case)]
                let ($($T,)+) = self;
                $( $T.write_into(world, archetype_id, row, tick); )+
            }

            #[inline]
            fn write_into_batch(
                self,
                world:        &mut World,
                archetype_id: ArchetypeId,
                row:          usize,
                tick:         Tick,
                col_indices:  &[usize],
            ) {
                #[allow(non_snake_case)]
                let ($($T,)+) = self;
                let mut _offset = 0usize;
                $(
                    let _cnt = $T::component_count();
                    $T.write_into_batch(world, archetype_id, row, tick, &col_indices[_offset.._offset + _cnt]);
                    _offset += _cnt;
                )+
            }

            #[inline]
            fn needs_drop() -> bool {
                false $( || $T::needs_drop() )+
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
impl_bundle!(A, B, C, D, E, F, G, H, I);
impl_bundle!(A, B, C, D, E, F, G, H, I, J);
impl_bundle!(A, B, C, D, E, F, G, H, I, J, K);
impl_bundle!(A, B, C, D, E, F, G, H, I, J, K, L);
// ── impl Bundle for () ────────────────────────────────────────

impl Bundle for () {
    fn component_count() -> usize {
        0
    }

    fn component_ids(&self, _registry: &mut ComponentRegistry) -> SmallVec<[ComponentId; 8]> {
        SmallVec::new()
    }

    fn write_into(self, _world: &mut World, _archetype_id: ArchetypeId, _row: usize, _tick: Tick) {
        // ()
    }

    fn needs_drop() -> bool {
        false
    }
}

// ── EntityRef ──────────────────────────────────────────────────

/// Facade для операций над одной entity: вставка, удаление, деспавн, чтение.
///
/// Создаётся через [`World::entity`].
pub struct EntityRef<'w> {
    world: &'w mut World,
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
    pub fn add_relation<R: crate::relations::RelationKind>(
        &mut self,
        kind: R,
        target: Entity,
    ) -> &mut Self {
        self.world.add_relation(self.entity, kind, target);
        self
    }

    /// Удалить relation.
    pub fn remove_relation<R: crate::relations::RelationKind>(
        &mut self,
        kind: R,
        target: Entity,
    ) -> &mut Self {
        self.world.remove_relation(self.entity, kind, target);
        self
    }

    /// Проверить наличие relation.
    pub fn has_relation<R: crate::relations::RelationKind>(&self, kind: R, target: Entity) -> bool {
        self.world.has_relation(self.entity, kind, target)
    }
}

impl World {
    /// Получить [`EntityRef`] для entity.
    pub fn entity(&mut self, entity: Entity) -> EntityRef<'_> {
        EntityRef {
            world: self,
            entity,
        }
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
        self.registry
            .iter()
            .find(|info| info.name == name)
            .map(|i| i.id)
    }

    // ── EntityTemplate API ────────────────────────────────────────

    /// Зарегистрировать именованный шаблон сущности.
    pub fn register_template(
        &mut self,
        name: &str,
        template: impl crate::template::EntityTemplate + 'static,
    ) {
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
        let cfg = ChunkConfig::default();
        // entity_count < min_per_thread * threads → serial fallback (one chunk = entity_count)
        assert_eq!(adaptive_chunk_size(50, 8, &cfg), 50); // 50 < 16*8=128 → serial
        assert_eq!(adaptive_chunk_size(50, 4, &cfg), 50); // 50 < 16*4=64 → serial
        assert_eq!(adaptive_chunk_size(1, 8, &cfg), 1); // 1 < 128 → serial
        assert_eq!(adaptive_chunk_size(99, 8, &cfg), 99); // 99 < 128 → serial
    }

    #[test]
    fn adaptive_chunk_size_medium_world() {
        let cfg = ChunkConfig::default();
        // entity_count >= threshold → ceil(ec / threads), clamped to [dynamic_min_chunk=64, max]
        assert_eq!(adaptive_chunk_size(200, 8, &cfg), 64); // ceil(200/8)=25 < 64 → 64
        assert_eq!(adaptive_chunk_size(500, 8, &cfg), 64); // ceil(500/8)=63 < 64 → 64
        assert_eq!(adaptive_chunk_size(100, 8, &cfg), 100); // 100 < 128 → serial
    }

    #[test]
    fn adaptive_chunk_size_large_world() {
        let cfg = ChunkConfig::default();
        // task_multiplier=2.0 → targets=ceil(8*2)=16 → ceil(1000/16)=63 → clamp(63,64,65536)=64
        assert_eq!(adaptive_chunk_size(1000, 8, &cfg), 64);
        // targets=16 → ceil(10000/16)=625
        assert_eq!(adaptive_chunk_size(10000, 8, &cfg), 625);
    }

    #[test]
    fn adaptive_chunk_size_single_thread() {
        let cfg = ChunkConfig::default();
        // single thread → targets=1 (multiplier only for n>1)
        assert_eq!(adaptive_chunk_size(50, 1, &cfg), 50);
        assert_eq!(adaptive_chunk_size(200, 1, &cfg), 200);
        assert_eq!(adaptive_chunk_size(1000, 1, &cfg), 1000);
    }

    #[test]
    fn adaptive_chunk_size_max_cap() {
        let cfg = ChunkConfig::default();
        // single thread → targets=1 → ceil(131072/1)=131072 → cap 65536
        assert_eq!(
            adaptive_chunk_size(DEFAULT_MAX_CHUNK_SIZE * 2, 1, &cfg),
            DEFAULT_MAX_CHUNK_SIZE
        );
        // 8 threads → targets=16 → ceil(131072/16)=8192, within bounds
        assert_eq!(
            adaptive_chunk_size(DEFAULT_MAX_CHUNK_SIZE * 2, 8, &cfg),
            8192
        );
    }

    #[test]
    fn adaptive_chunk_size_transition_points() {
        let cfg = ChunkConfig::default();
        // 99 < 128 (16*8) → serial
        assert_eq!(adaptive_chunk_size(99, 8, &cfg), 99);
        // 100 < 128 → serial
        assert_eq!(adaptive_chunk_size(100, 8, &cfg), 100);
        // 999 >= 128 → targets=ceil(8*2.0)=16 → ceil(999/16)=63 → clamp(63,64,65536)=64
        assert_eq!(adaptive_chunk_size(999, 8, &cfg), 64);
        // 1000 >= 128 → targets=16 → ceil(1000/16)=63 → clamp=64
        assert_eq!(adaptive_chunk_size(1000, 8, &cfg), 64);
    }

    #[test]
    fn chunk_config_no_serial_fallback() {
        let cfg = ChunkConfig {
            min_entities_per_thread: 16,
            dynamic_min_chunk: 1,
            max_chunk_size: 4096,
            auto_serial_fallback: false,
            task_multiplier: 1.0,
        };
        // auto_serial_fallback = false → always split into threads chunks
        // multiplier=1.0 → targets=8 → ceil(50/8)=7
        assert_eq!(adaptive_chunk_size(50, 8, &cfg), 7);
        // ceil(1/8) = 1
        assert_eq!(adaptive_chunk_size(1, 8, &cfg), 1);
    }

    #[test]
    fn chunk_config_custom_thresholds() {
        let cfg = ChunkConfig {
            min_entities_per_thread: 8,
            dynamic_min_chunk: 1,
            max_chunk_size: 8192,
            auto_serial_fallback: true,
            task_multiplier: 1.0,
        };
        // 8 * 8 = 64 threshold
        assert_eq!(adaptive_chunk_size(50, 8, &cfg), 50); // 50 < 64 → serial
        assert_eq!(adaptive_chunk_size(100, 8, &cfg), 13); // 100 >= 64 → ceil(100/8)=13
    }

    // ── Bundle composition tests ─────────────────────────────────

    #[derive(Debug, PartialEq)]
    struct Pos {
        x: f32,
        y: f32,
    }
    impl crate::component::Component for Pos {}

    #[derive(Debug, PartialEq)]
    struct Vel {
        x: f32,
        y: f32,
    }
    impl crate::component::Component for Vel {}

    #[derive(Debug, PartialEq)]
    struct Hp(f32);
    impl crate::component::Component for Hp {}

    #[derive(Debug, PartialEq)]
    struct Armor(f32);
    impl crate::component::Component for Armor {}

    #[derive(Debug, PartialEq)]
    struct Team(u8);
    impl crate::component::Component for Team {}

    /// TD-1: `CachedQuery::iter()` НЕ должен пропускать первый архетип.
    #[test]
    fn cached_query_iter_does_not_skip_first_archetype() {
        use crate::query::Read;

        // Один архетип (только Pos).
        let mut world = World::new();
        let a = world.spawn((Pos { x: 1.0, y: 0.0 },));
        let b = world.spawn((Pos { x: 2.0, y: 0.0 },));
        let got: Vec<_> = world
            .query_changed::<Read<Pos>>(Tick::ZERO)
            .iter()
            .map(|(e, _)| e)
            .collect();
        assert_eq!(got.len(), 2, "iter() должен вернуть ОБА entity (не пропустить первый)");
        assert!(got.contains(&a) && got.contains(&b));

        // Несколько архетипов: (Pos) и (Pos, Vel).
        let c = world.spawn((Pos { x: 3.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        let got2: Vec<_> = world
            .query_changed::<Read<Pos>>(Tick::ZERO)
            .iter()
            .map(|(e, _)| e)
            .collect();
        assert_eq!(got2.len(), 3, "iter() должен охватить все архетипы, включая первый");
        assert!(got2.contains(&a) && got2.contains(&b) && got2.contains(&c));

        // for_each и iter дают одинаковый набор.
        let mut fe = 0usize;
        world.query_changed::<Read<Pos>>(Tick::ZERO).for_each(|_, _| fe += 1);
        assert_eq!(fe, got2.len(), "for_each и iter должны быть согласованы");
    }

    /// CR-M2: `(Read<A>, Read<B>)` и `(Read<A>, Without<B>)` имеют одинаковый
    /// fill_ids — записи кэша НЕ должны отравлять друг друга.
    #[test]
    fn cached_query_without_does_not_share_entry_with_read() {
        use crate::query::{Read, Without};

        let mut world = World::new();
        let _both = world.spawn((Pos { x: 1.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        let only_pos = world.spawn((Pos { x: 2.0, y: 0.0 },));

        // Сначала прогреваем кэш формой (Read, Read)…
        let with_vel = world.query_typed::<(Read<Pos>, Read<Vel>)>().len();
        assert_eq!(with_vel, 1);

        // …затем (Read, Without) обязан увидеть СВОЙ список архетипов.
        let mut seen = Vec::new();
        world
            .query_typed::<(Read<Pos>, Without<Vel>)>()
            .for_each(|e, _| seen.push(e));
        assert_eq!(seen, vec![only_pos], "Without-форма не должна делить запись кэша с Read-формой");
    }

    /// CR-M2: запись кэша инкрементально дополняется архетипами, созданными
    /// ПОСЛЕ первого построения списка.
    #[test]
    fn cached_query_picks_up_new_archetypes() {
        use crate::query::Read;

        let mut world = World::new();
        world.spawn((Pos { x: 1.0, y: 0.0 },));
        assert_eq!(world.query_typed::<Read<Pos>>().len(), 1);

        // Новый архетип (Pos, Vel) после прогрева кэша.
        world.spawn((Pos { x: 2.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        assert_eq!(world.query_typed::<Read<Pos>>().len(), 2);
    }

    /// CR-M2: entity, въехавшая в ОПУСТЕВШИЙ архетип, не теряется кэшем
    /// (пустые архетипы остаются в списках; insert/remove кэш не сбрасывают).
    #[test]
    fn cached_query_sees_entity_in_repopulated_archetype() {
        use crate::query::Read;

        let mut world = World::new();
        let e = world.spawn((Pos { x: 1.0, y: 0.0 }, Vel { x: 3.0, y: 0.0 }));
        assert_eq!(world.query_typed::<(Read<Pos>, Read<Vel>)>().len(), 1);

        // Архетип (Pos, Vel) пустеет…
        world.remove::<Vel>(e);
        assert_eq!(world.query_typed::<(Read<Pos>, Read<Vel>)>().len(), 0);

        // …и снова наполняется — кэшированный список обязан его видеть.
        world.insert(e, Vel { x: 4.0, y: 0.0 });
        let mut seen = Vec::new();
        world
            .query_typed::<(Read<Pos>, Read<Vel>)>()
            .for_each(|ent, _| seen.push(ent));
        assert_eq!(seen, vec![e]);
    }

    /// CR-M2 (C-4): на мире >128 архетипов Query::new берёт кандидатов из
    /// component_arch_index по САМОМУ РЕДКОМУ обязательному компоненту.
    #[test]
    fn query_new_candidates_from_rarest_component_on_large_world() {
        use crate::query::{Query, Read, With};

        struct F0;
        struct F1;
        struct F2;
        struct F3;
        struct F4;
        struct F5;
        struct F6;
        struct F7;
        struct Rare(u32);
        impl crate::component::Component for F0 {}
        impl crate::component::Component for F1 {}
        impl crate::component::Component for F2 {}
        impl crate::component::Component for F3 {}
        impl crate::component::Component for F4 {}
        impl crate::component::Component for F5 {}
        impl crate::component::Component for F6 {}
        impl crate::component::Component for F7 {}
        impl crate::component::Component for Rare {}

        let mut world = World::new();
        let mut rare_holder = None;
        // 200 уникальных составов → >128 архетипов (кандидат-путь).
        for i in 0..200u32 {
            let e = world.spawn((Pos { x: i as f32, y: 0.0 },));
            if i & 1 != 0 { world.insert(e, F0); }
            if i & 2 != 0 { world.insert(e, F1); }
            if i & 4 != 0 { world.insert(e, F2); }
            if i & 8 != 0 { world.insert(e, F3); }
            if i & 16 != 0 { world.insert(e, F4); }
            if i & 32 != 0 { world.insert(e, F5); }
            if i & 64 != 0 { world.insert(e, F6); }
            if i & 128 != 0 { world.insert(e, F7); }
            if i == 137 {
                world.insert(e, Rare(7));
                rare_holder = Some(e);
            }
        }
        assert!(world.archetype_count() > 128, "тесту нужен кандидат-путь");

        // Редкий компонент: кандидаты = 1 архетип, результат корректен.
        let got: Vec<_> = Query::<(Read<Pos>, Read<Rare>)>::new(&world)
            .iter()
            .map(|(e, _)| e)
            .collect();
        assert_eq!(got, vec![rare_holder.unwrap()]);

        // With-форма тем же путём.
        let cnt = Query::<(Read<Pos>, With<Rare>)>::new(&world).iter().count();
        assert_eq!(cnt, 1);

        // Широкий запрос на том же мире — все 200 строк на месте.
        let all = Query::<Read<Pos>>::new(&world).iter().count();
        assert_eq!(all, 200);
    }

    // Вложенные Bundle — ручная реализация (proc-макросы не работают внутри apex-core)
    struct PlayerBase {
        pos: Pos,
        hp: Hp,
    }

    impl crate::Bundle for PlayerBase {
        fn component_count() -> usize {
            2
        }

        fn component_ids(
            &self,
            registry: &mut crate::ComponentRegistry,
        ) -> SmallVec<[crate::ComponentId; 8]> {
            let mut ids = SmallVec::new();
            crate::Bundle::push_component_ids(&self.pos, registry, &mut ids);
            crate::Bundle::push_component_ids(&self.hp, registry, &mut ids);
            ids.sort_unstable();
            ids
        }

        fn write_into(
            self,
            world: &mut crate::World,
            archetype_id: crate::ArchetypeId,
            row: usize,
            tick: crate::Tick,
        ) {
            crate::Bundle::write_into(self.pos, world, archetype_id, row, tick);
            crate::Bundle::write_into(self.hp, world, archetype_id, row, tick);
        }

        fn needs_drop() -> bool {
            false || <Pos as crate::Bundle>::needs_drop() || <Hp as crate::Bundle>::needs_drop()
        }
    }

    struct ArmedPlayer {
        base: PlayerBase,
        weapon: Vel,
        armor: Armor,
    }

    impl crate::Bundle for ArmedPlayer {
        fn component_count() -> usize {
            4
        }

        fn component_ids(
            &self,
            registry: &mut crate::ComponentRegistry,
        ) -> SmallVec<[crate::ComponentId; 8]> {
            let mut ids = SmallVec::new();
            crate::Bundle::push_component_ids(&self.base, registry, &mut ids);
            crate::Bundle::push_component_ids(&self.weapon, registry, &mut ids);
            crate::Bundle::push_component_ids(&self.armor, registry, &mut ids);
            ids.sort_unstable();
            ids
        }

        fn write_into(
            self,
            world: &mut crate::World,
            archetype_id: crate::ArchetypeId,
            row: usize,
            tick: crate::Tick,
        ) {
            crate::Bundle::write_into(self.base, world, archetype_id, row, tick);
            crate::Bundle::write_into(self.weapon, world, archetype_id, row, tick);
            crate::Bundle::write_into(self.armor, world, archetype_id, row, tick);
        }

        fn needs_drop() -> bool {
            false
                || <PlayerBase as crate::Bundle>::needs_drop()
                || <Vel as crate::Bundle>::needs_drop()
                || <Armor as crate::Bundle>::needs_drop()
        }
    }

    #[test]
    fn bundle_nested_struct_spawn() {
        let mut world = World::new();
        let e = world.spawn(ArmedPlayer {
            base: PlayerBase {
                pos: Pos { x: 10.0, y: 20.0 },
                hp: Hp(100.0),
            },
            weapon: Vel { x: 1.0, y: 0.5 },
            armor: Armor(50.0),
        });

        // Все компоненты на месте
        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 10.0, y: 20.0 }));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(100.0)));
        assert_eq!(world.get::<Vel>(e), Some(&Vel { x: 1.0, y: 0.5 }));
        assert_eq!(world.get::<Armor>(e), Some(&Armor(50.0)));
        assert!(world.get::<Team>(e).is_none());
    }

    #[test]
    fn bundle_tuple_of_bundles_spawn() {
        let mut world = World::new();
        let e = world.spawn((
            PlayerBase {
                pos: Pos { x: 1.0, y: 2.0 },
                hp: Hp(75.0),
            },
            Vel { x: 3.0, y: 4.0 },
            Team(1),
        ));

        // Кортеж из Bundle-структуры + компонентов работает
        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 1.0, y: 2.0 }));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(75.0)));
        assert_eq!(world.get::<Vel>(e), Some(&Vel { x: 3.0, y: 4.0 }));
        assert_eq!(world.get::<Team>(e), Some(&Team(1)));
        assert!(world.get::<Armor>(e).is_none());
    }

    #[test]
    fn bundle_single_component_direct_spawn() {
        let mut world = World::new();
        // Компонент напрямую в spawn (blanket impl<T: Component> Bundle for T)
        let e = world.spawn(Pos { x: 5.0, y: 6.0 });
        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 5.0, y: 6.0 }));
    }

    #[test]
    fn bundle_mixed_tuple_of_components_and_bundles() {
        let mut world = World::new();
        // Смесь: одиночные компоненты + Bundle-структура + ещё компонент
        let e = world.spawn((
            Hp(200.0),
            PlayerBase {
                pos: Pos { x: 7.0, y: 8.0 },
                hp: Hp(80.0),
            },
            Armor(30.0),
            Team(2),
        ));

        assert_eq!(world.get::<Pos>(e), Some(&Pos { x: 7.0, y: 8.0 }));
        // У Hp двусмысленность: один в кортеже отдельно, другой внутри PlayerBase
        // Колонка одна — побеждает последний записанный (PlayerBase.hp = 80).
        // Проверяем что все компоненты присутствуют
        assert!(world.get::<Hp>(e).is_some());
        assert_eq!(world.get::<Armor>(e), Some(&Armor(30.0)));
        assert_eq!(world.get::<Team>(e), Some(&Team(2)));
    }

    #[test]
    fn bundle_spawn_many_with_bundle_struct() {
        let mut world = World::new();
        // spawn_many работает с вложенными Bundle (bulk-copy при needs_drop() == false)
        let entities = world.spawn_many(10, |_| ArmedPlayer {
            base: PlayerBase {
                pos: Pos { x: 50.0, y: 50.0 },
                hp: Hp(100.0),
            },
            weapon: Vel { x: 0.1, y: 0.0 },
            armor: Armor(10.0),
        });

        assert_eq!(entities.len(), 10);
        // Проверяем через прямой get, не через query
        for &e in &entities {
            assert!(world.get::<Pos>(e).is_some(), "Entity {:?} missing Pos", e);
            assert!(world.get::<Hp>(e).is_some(), "Entity {:?} missing Hp", e);
            assert!(world.get::<Vel>(e).is_some(), "Entity {:?} missing Vel", e);
            assert!(
                world.get::<Armor>(e).is_some(),
                "Entity {:?} missing Armor",
                e
            );
        }
    }

    #[test]
    fn bundle_spawn_batch_heterogeneous_bundles() {
        let mut world = World::new();
        // Разные способы spawn в одном тесте
        let boss = world.spawn(ArmedPlayer {
            base: PlayerBase {
                pos: Pos { x: 1.0, y: 1.0 },
                hp: Hp(50.0),
            },
            weapon: Vel { x: 0.0, y: 0.0 },
            armor: Armor(10.0),
        });
        let minion = world.spawn((Pos { x: 2.0, y: 2.0 }, Hp(25.0), Team(3)));
        let empty = world.spawn(());

        assert!(world.has_component::<Pos>(boss));
        assert_eq!(world.get::<Armor>(boss), Some(&Armor(10.0)));
        assert!(world.has_component::<Pos>(minion));
        assert_eq!(world.get::<Team>(minion), Some(&Team(3)));
        assert!(!world.has_component::<Pos>(empty));
    }
}
