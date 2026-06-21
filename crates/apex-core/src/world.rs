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
/// Ключ кэша запросов (CR-M2b): по одному `u64` на компонент — ComponentId в
/// нижних 32 битах, роль (required/without/optional) в верхних
/// (`WorldQuery::fill_cache_key`). Однозначно кодирует матч-семантику формы:
/// `(Read<A>, Read<B>)`, `(Read<A>, Without<B>)` и `(Read<A>, Maybe<B>)` —
/// РАЗНЫЕ записи (раньше делили одну — отравление кэша).
///
/// Hot-path без аллокаций: ключ строится одним проходом в inline-SmallVec,
/// lookup — zero-copy по `&[u64]` (Borrow), владение — только при вставке.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct QueryCacheKey(SmallVec<[u64; 8]>);

impl std::borrow::Borrow<[u64]> for QueryCacheKey {
    fn borrow(&self) -> &[u64] {
        &self.0
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
        key: &SmallVec<[u64; 8]>,
        archetypes: &[Archetype],
        matches: impl Fn(&Archetype) -> bool,
    ) -> Arc<[usize]> {
        let total = archetypes.len();

        // Hit path: запись актуальна, если видела все текущие архетипы.
        // Lookup по &[u64] — без построения владеющего ключа.
        {
            let map = self.entries.read().unwrap();
            if let Some(entry) = map.get(key.as_slice()) {
                if entry.seen_arch_count == total {
                    return entry.arch_indices.clone();
                }
            }
        }

        let mut map = self.entries.write().unwrap();
        // Двойная проверка: другой поток мог дополнить между read и write lock.
        let (mut indices, start) = match map.get(key.as_slice()) {
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
            QueryCacheKey(key.clone()),
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
/// суммарные живые строки, максимум строк в одном архетипе и память (W3-5).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchetypeStats {
    pub archetypes: usize,
    pub empty_archetypes: usize,
    pub total_rows: usize,
    pub max_rows_in_archetype: usize,
    /// Аллоцировано под данные компонентов (Σ capacity × item_size).
    pub component_bytes: usize,
    /// Аллоцировано под change/added-тики (Σ capacity × 4 × 2).
    pub tick_bytes: usize,
    /// Аллоцировано под списки entity архетипов (Σ capacity × 8).
    pub entity_bytes: usize,
}

impl ArchetypeStats {
    /// Суммарная память хранилища (компоненты + тики + entity-списки).
    #[inline]
    pub fn total_bytes(&self) -> usize {
        self.component_bytes + self.tick_bytes + self.entity_bytes
    }
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

/// Генератор уникальных id миров (для привязки [`QueryState`] к миру).
/// Начинается с 1: id 0 зарезервирован как «ничей» (свежий QueryState).
static WORLD_ID_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct World {
    /// Уникален в пределах процесса; `QueryState` сверяет его, чтобы не
    /// применить стейт одного мира к другому (main vs render vs isolated).
    pub(crate) world_id: u64,
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
    /// Очередь хуков состава (W3-1): структурные операции СНАЧАЛА завершаются,
    /// потом диспетчер вызывает хуки на консистентном мире. Вложенные
    /// структурные операции из хуков дописывают в ту же очередь — обрабатывает
    /// тот же (внешний) диспетчер, без рекурсии.
    pub(crate) hook_queue: Vec<HookEvent>,
    /// Диспетчер хуков уже работает выше по стеку (re-entrancy guard).
    pub(crate) hook_dispatch_active: bool,
}

/// Отложенное событие состава для диспетчера хуков (W3-1).
#[derive(Clone, Copy)]
pub(crate) enum HookEvent {
    Added(Entity, ComponentId),
    Removed(Entity, ComponentId),
    RelationAdded {
        kind_idx: u32,
        subject: Entity,
        target: Entity,
    },
    RelationRemoved {
        kind_idx: u32,
        subject: Entity,
        target: Entity,
    },
}

impl World {
    pub fn new() -> Self {
        let mut registry = ComponentRegistry::new();
        registry.register_all_auto();
        let mut world = Self {
            world_id: WORLD_ID_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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
            hook_queue: Vec::new(),
            hook_dispatch_active: false,
        };
        world
            .archetypes
            .push(Archetype::new(ArchetypeId::EMPTY, SmallVec::new(), &[]));
        world
            .archetype_index
            .insert(ArchetypeKey(SmallVec::new()), ArchetypeId::EMPTY);
        world
    }

    /// Уникальный id мира в пределах процесса (привязка [`QueryState`]).
    #[inline]
    pub fn id(&self) -> u64 {
        self.world_id
    }

    /// Интервал автозапуска [`check_change_ticks`](Self::check_change_ticks):
    /// каждые 2²⁶ тиков (~3 дня @250Hz). Должен быть много меньше
    /// `2³¹ − Tick::MAX_CHANGE_AGE`, чтобы тик не успел «перевернуться»
    /// между проходами клампа.
    const TICK_CHECK_INTERVAL: u32 = 1 << 26;

    /// Продвигает глобальный tick. **Не делает flush событий** — это ответственность Scheduler.
    /// Для использования без Scheduler вызывайте [`flush_all_events()`](Self::flush_all_events) вручную.
    pub fn tick(&mut self) {
        self.current_tick.0 = self.current_tick.0.wrapping_add(1);
        if self.current_tick.0.is_multiple_of(Self::TICK_CHECK_INTERVAL) {
            self.check_change_ticks();
        }
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
        if self.current_tick.0.is_multiple_of(Self::TICK_CHECK_INTERVAL) {
            self.check_change_ticks();
        }
    }

    /// Кламп старых change-тиков к окну [`Tick::MAX_CHANGE_AGE`] (W2-3,
    /// аналог Bevy `check_change_ticks`).
    ///
    /// `Changed<T>` использует wrapping-сравнение, корректное при разнице
    /// < 2³¹: строка, не менявшаяся дольше, стала бы ложно-Changed (~99 дней
    /// аптайма @250Hz). Кламп подтягивает такие тики к границе окна, сохраняя
    /// «давно не менялась» навсегда. Запускается автоматически из
    /// [`tick`](Self::tick)/[`advance_change_tick`](Self::advance_change_tick)
    /// раз в `TICK_CHECK_INTERVAL`; публичен для прод-серверов/редактора с
    /// собственным циклом.
    pub fn check_change_ticks(&mut self) {
        let current = self.current_tick;
        for arch in &mut self.archetypes {
            for col in &mut arch.columns {
                col.check_change_ticks(current);
            }
        }
        self.last_run_tick.check_against(current);
    }

    /// База change-detection для систем (тик предыдущей границы кадра).
    #[inline]
    pub fn last_run_tick(&self) -> Tick {
        self.last_run_tick
    }

    /// Выставить базу change-detection (`Changed<T>`/`Added<T>` сравнивают change-tick строки с ней).
    /// **Внутренний API планировщика:** он ставит её перед каждой СТАДИЕЙ равной тику, на котором эта
    /// стадия выполнялась в прошлый раз. Вместе с продвижением `current_tick` между стадиями ([`tick`])
    /// это даёт **cross-stage change detection**: запись в поздней стадии кадра N видна более ранней
    /// стадии кадра N+1 (закрывает слепую зону per-frame-тика, TD-52). Прямое использование вне
    /// планировщика обычно не нужно.
    #[inline]
    pub fn set_last_run_tick(&mut self, tick: Tick) {
        self.last_run_tick = tick;
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

    /// Зарегистрировать компонент с **контекст-зависимыми** serde-функциями (TD-44): компонент с внешней
    /// ссылкой (Handle ассета, Entity-референс) (де)сериализуется через [`SerdeContext`](crate::SerdeContext),
    /// который передаётся в `WorldSerializer::snapshot_with`/`restore_with`. Резолвер живёт в движке/
    /// редакторе ⇒ apex-ecs остаётся ассет-агностичным. См. [`ComponentRegistry::register_serde_with`].
    pub fn register_component_serde_with<T: Component>(
        &mut self,
        fns: crate::component::ComponentSerdeFns,
    ) -> ComponentId {
        self.registry.register_serde_with::<T>(fns)
    }

    // ── Хуки состава (W3-1) ────────────────────────────────────

    /// Зарегистрировать `on_add`-хук компонента `T`: вызывается после того,
    /// как `T` ПОЯВИЛСЯ у entity (spawn / insert нового; замена значения
    /// существующего компонента хук НЕ дёргает — это `Changed`, не `Added`).
    ///
    /// Хук вызывается на консистентном мире (после завершения структурной
    /// операции) и может делать любые операции, включая структурные.
    /// Один хук на компонент; повторная регистрация — panic (для нескольких
    /// подписчиков используйте события).
    pub fn on_add<T: Component>(&mut self, hook: crate::component::ComponentHookFn) {
        let cid = self.registry.get_or_register::<T>();
        let hooks = self.registry.hooks_mut(cid);
        assert!(
            hooks.on_add.is_none(),
            "on_add-хук для `{}` уже зарегистрирован (один хук на компонент; \
             для нескольких подписчиков используйте события)",
            std::any::type_name::<T>()
        );
        hooks.on_add = Some(hook);
        self.registry.set_flag(cid, crate::component::FLAG_ON_ADD);
    }

    /// Зарегистрировать `on_remove`-хук компонента `T`: вызывается после того,
    /// как entity ПОТЕРЯЛА `T` (`remove` или `despawn` — в последнем случае
    /// entity уже мертва, `is_alive == false`). Значение компонента к моменту
    /// вызова уже уничтожено — хук получает только entity.
    ///
    /// Один хук на компонент; повторная регистрация — panic.
    pub fn on_remove<T: Component>(&mut self, hook: crate::component::ComponentHookFn) {
        let cid = self.registry.get_or_register::<T>();
        let hooks = self.registry.hooks_mut(cid);
        assert!(
            hooks.on_remove.is_none(),
            "on_remove-хук для `{}` уже зарегистрирован (один хук на компонент; \
             для нескольких подписчиков используйте события)",
            std::any::type_name::<T>()
        );
        hooks.on_remove = Some(hook);
        self.registry.set_flag(cid, crate::component::FLAG_ON_REMOVE);
    }

    /// Объявить: компонент `C` требует `R` (D2-4, аналог Bevy `#[require]`).
    ///
    /// При появлении `C` у entity (spawn / insert) недостающий `R`
    /// дотягивается `R::default()` — явно заданное значение всегда выигрывает.
    /// Требования транзитивны (если `R` сам что-то требует). Для derive-типов
    /// удобнее атрибут: `#[derive(Component)] #[require(LocalTransform)]`.
    ///
    /// ```ignore
    /// world.require_component::<MeshRenderer, LocalTransform>();
    /// world.require_component::<MeshRenderer, GlobalTransform>();
    /// let e = world.spawn((MeshRenderer::new(mesh, mat),)); // трансформы дотянутся
    /// ```
    pub fn require_component<C: Component, R: Component + Default>(&mut self) {
        self.registry.register_required::<C, R>();
    }

    /// Включить эмиссию событий [`Removed<T>`](crate::events::Removed) при
    /// потере компонента `T` (remove/despawn) — аналог Bevy
    /// `RemovedComponents`. Чтение — обычными путями событий (`&[Removed<T>]`
    /// в `system!`, `event_reader`), per-reader курсоры исключают дубли.
    ///
    /// Идемпотентна. Для невключённых типов удаления не записываются
    /// (нулевая стоимость).
    pub fn track_removals<T: Component>(&mut self) {
        let cid = self.registry.get_or_register::<T>();
        self.events.register::<crate::events::Removed<T>>();
        self.registry.hooks_mut(cid).emit_removed = Some(|events, entity| {
            events
                .get_or_register_mut::<crate::events::Removed<T>>()
                .send(crate::events::Removed::new(entity));
        });
        self.registry
            .set_flag(cid, crate::component::FLAG_TRACK_REMOVED);
    }

    /// Зарегистрировать `on_add`-хук связи вида `R`: вызывается после
    /// успешного `add_relation` с `(subject, target)`.
    /// Один хук на вид; повторная регистрация — panic.
    pub fn on_relation_add<R: crate::relations::RelationKind>(
        &mut self,
        hook: crate::relations::RelationHookFn,
    ) {
        let kind_idx = self.relations.get_or_register::<R>();
        self.relations.set_on_add(kind_idx, hook);
    }

    /// Зарегистрировать `on_remove`-хук связи вида `R`: вызывается после
    /// исчезновения пары — явный `remove_relation` ИЛИ вычистка при despawn
    /// subject'а/target'а (включая каскад; entity к этому моменту могут быть
    /// мертвы). Один хук на вид; повторная регистрация — panic.
    pub fn on_relation_remove<R: crate::relations::RelationKind>(
        &mut self,
        hook: crate::relations::RelationHookFn,
    ) {
        let kind_idx = self.relations.get_or_register::<R>();
        self.relations.set_on_remove(kind_idx, hook);
    }

    /// Диспетчер хуков: вызывается в КОНЦЕ публичных структурных операций.
    /// Быстрый путь (нет подписчиков/очередь пуста) — одна проверка.
    #[inline]
    pub(crate) fn flush_hooks(&mut self) {
        if self.hook_queue.is_empty() || self.hook_dispatch_active {
            return;
        }
        self.flush_hooks_slow();
    }

    #[cold]
    fn flush_hooks_slow(&mut self) {
        self.hook_dispatch_active = true;
        let mut i = 0;
        // Хуки могут дописывать события в хвост очереди (вложенные структурные
        // операции) — обычный while по растущему Vec, без рекурсии.
        while i < self.hook_queue.len() {
            let ev = self.hook_queue[i];
            i += 1;
            match ev {
                HookEvent::Added(entity, cid) => {
                    // Required-компоненты (D2-4) — ДО пользовательского
                    // on_add: хук видит entity уже с полным составом.
                    // Транзитивные requires идут через эту же очередь
                    // (вставка R ставит своё Added-событие).
                    if self.registry.flags(cid) & crate::component::FLAG_REQUIRES != 0 {
                        let fns: SmallVec<[crate::component::RequiredInsertFn; 4]> = self
                            .registry
                            .requires(cid)
                            .map(|s| s.iter().copied().collect())
                            .unwrap_or_default();
                        for f in fns {
                            f(self, entity);
                        }
                    }
                    let hook = self.registry.hooks(cid).and_then(|h| h.on_add);
                    if let Some(f) = hook {
                        f(self, entity);
                    }
                }
                HookEvent::Removed(entity, cid) => {
                    let hook = self.registry.hooks(cid).and_then(|h| h.on_remove);
                    if let Some(f) = hook {
                        f(self, entity);
                    }
                }
                HookEvent::RelationAdded {
                    kind_idx,
                    subject,
                    target,
                } => {
                    if let Some(f) = self.relations.on_add_hook(kind_idx) {
                        f(self, subject, target);
                    }
                }
                HookEvent::RelationRemoved {
                    kind_idx,
                    subject,
                    target,
                } => {
                    if let Some(f) = self.relations.on_remove_hook(kind_idx) {
                        f(self, subject, target);
                    }
                }
            }
        }
        self.hook_queue.clear();
        self.hook_dispatch_active = false;
    }

    /// Поставить `Added`-хуки для свежесозданной entity по списку её
    /// компонентов (вызывающий уже проверил `registry.any_flags()`).
    fn queue_added_hooks(&mut self, entity: Entity, ids: &[ComponentId]) {
        for &cid in ids {
            if self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0 {
                self.hook_queue.push(HookEvent::Added(entity, cid));
            }
        }
    }

    /// Уведомления о ПОТЕРЕ компонента: `on_remove`-хук в очередь +
    /// немедленная эмиссия `Removed<T>`-события (вызывающий уже проверил
    /// `registry.any_flags()`).
    fn notify_removed(&mut self, entity: Entity, cid: ComponentId) {
        let flags = self.registry.flags(cid);
        if flags & crate::component::FLAG_ON_REMOVE != 0 {
            self.hook_queue.push(HookEvent::Removed(entity, cid));
        }
        if flags & crate::component::FLAG_TRACK_REMOVED != 0 {
            let emit = self.registry.hooks(cid).and_then(|h| h.emit_removed);
            if let Some(f) = emit {
                f(&mut self.events, entity);
            }
        }
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
            stats.entity_bytes += arch.entities.capacity() * std::mem::size_of::<Entity>();
            for col in &arch.columns {
                let (data, ticks) = col.allocated_bytes();
                stats.component_bytes += data;
                stats.tick_bytes += ticks;
            }
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
        let entity = self.entities.allocate();
        self.spawn_at(entity, bundle);
        entity
    }

    /// Дескриптор-резерватор entity (делит атомарный high-water с аллокатором). Клонируется в
    /// [`Commands`](crate::commands::Commands), чтобы `commands.spawn().id()` отдавал настоящий
    /// `Entity` из параллельной системы (1:1 Bevy `Entities::reserve_entity`).
    #[inline]
    pub fn entity_reserver(&self) -> crate::entity::EntityReserver {
        self.entities.reserver()
    }

    /// Материализовать записи под все зарезервированные через [`World::entity_reserver`] индексы.
    /// Вызывается [`Commands::apply`](crate::commands::Commands::apply) перед обработкой очереди, до
    /// того как spawn-команды проставят компоненты зарезервированным entity. Идемпотентно/дёшево.
    #[inline]
    pub fn flush_reserved(&mut self) {
        self.entities.flush();
    }

    /// Заспавнить компоненты на УЖЕ зарезервированную (через [`World::entity_reserver`]) entity —
    /// путь `commands.spawn().id()`. Семантически идентичен [`World::spawn`], но не аллоцирует новый
    /// id, а наполняет переданный (его записи гарантирует [`World::flush_reserved`] на границе apply).
    #[inline]
    pub fn spawn_reserved<B: Bundle>(&mut self, entity: Entity, bundle: B) {
        self.spawn_at(entity, bundle);
    }

    /// Общее тело спавна: наполнить КОНКРЕТНУЮ entity компонентами `bundle`. Запись entity при
    /// необходимости создаётся (`ensure_record`) — нужно для зарезервированных id, опередивших flush;
    /// для прямого `spawn` (аллокатор уже создал запись) это no-op.
    fn spawn_at<B: Bundle>(&mut self, entity: Entity, bundle: B) {
        self.entities.ensure_record(entity.index());
        let ids = bundle.component_ids(&mut self.registry);
        if ids.is_empty() {
            // Быстрый путь для пустой entity (spawn(()))
            let row = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
            self.entities.set_location(
                entity,
                EntityLocation {
                    archetype_id: ArchetypeId::EMPTY,
                    row,
                },
            );
            return;
        }
        // Обычный путь
        let archetype_id = self.get_or_create_archetype(&ids);
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
        if self.registry.any_flags() {
            self.queue_added_hooks(entity, &ids);
            self.flush_hooks();
        }
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
        // `decl_ids` — порядок ОБЪЯВЛЕНИЯ бандла (= порядок обхода `write_into_batch`); `ids` —
        // ОТСОРТИРОВАННЫЙ (для архетипа). Их РАЗДЕЛЕНИЕ критично: col_indices ОБЯЗАН быть в порядке
        // обхода, иначе компонент пишется в чужую колонку (UB).
        let mut decl_ids: SmallVec<[ComponentId; 8]> = SmallVec::new();
        probe.push_component_ids(&mut self.registry, &mut decl_ids);
        let mut ids = decl_ids.clone();
        ids.sort_unstable();
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

        // Предвычисляем column indices в порядке ОБЪЯВЛЕНИЯ (`decl_ids`) — РОВНО как их потребляет
        // `write_into_batch`. Избегает повторных get_or_register/column_index в write_into для
        // каждой entity (~40k HashMap lookup'ов при 10k). КРИТИЧНО из `decl_ids`, НЕ из
        // отсортированных `ids` (иначе при «порядок объявления ≠ порядок id» — запись в чужую колонку).
        let col_indices: SmallVec<[usize; 8]> = decl_ids
            .iter()
            .filter_map(|&id| self.archetypes[arch_idx].column_index(id))
            .collect();

        // ВСЕГДА per-entity: `make_bundle(i)` вызывается для КАЖДОЙ сущности (контракт замыкания —
        // данные per-index). Прежний bulk-copy «копировать строку 0 во все» БЫЛ НЕКОРРЕКТЕН: звал
        // `make_bundle` лишь для строки 0 ⇒ `spawn_many(n, |i| A(i))` молча давал ВСЕМ A(0) (потеря
        // per-entity данных). `col_indices` (порядок ОБХОДА) делает запись по колонкам правильной.
        // Перф: пишем ДАННЫЕ per-entity (`write_data_into_batch`, без тиков/len), а тики/`len`
        // проставляем ПОКОЛОНОЧНО один раз на пачку (resize вместо count×ncols push'ей).
        {
            for (i, &entity) in entities.iter().enumerate() {
                let row = start_row + i;
                let bundle = make_bundle(i);
                self.archetypes[arch_idx].entities.push(entity);
                bundle.write_data_into_batch(self, archetype_id, row, tick, &col_indices);
            }
            // Тики + len — ПОКОЛОНОЧНО, к АБСОЛЮТНОМУ target (start_row+count). Это устойчиво к ОБОИМ
            // путям записи: для data-only override'ов (leaf/tuple/derive) — заполняет count новых
            // слотов; для дефолта (ручной impl → write_into_batch уже выставил тики/len) — no-op.
            let target_len = start_row + count;
            let arch = &mut self.archetypes[arch_idx];
            for &col_idx in &col_indices {
                let col = &mut arch.columns[col_idx];
                col.change_ticks.resize(target_len, tick);
                col.added_ticks.resize(target_len, tick);
                col.len = target_len;
            }
        }

        self.entities
            .set_locations_batch(&entities, archetype_id, start_row as u32);

        if self.registry.any_flags() {
            let flagged: SmallVec<[ComponentId; 8]> = ids
                .iter()
                .copied()
                .filter(|&cid| self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0)
                .collect();
            if !flagged.is_empty() {
                for &entity in &entities {
                    for &cid in &flagged {
                        self.hook_queue.push(HookEvent::Added(entity, cid));
                    }
                }
                self.flush_hooks();
            }
        }
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

    /// Bulk-спавн пачки сущностей с ПО-ЭЛЕМЕНТНЫМИ бандлами одного типа `B` в ОДИН архетип одним
    /// резолвом (`component_ids`/архетип/колонки — раз на пачку, НЕ на каждый спавн). Путь
    /// `Commands::apply` для подряд идущих одно-типных spawn-команд (см. `spawn_apply_batch`):
    /// снимает per-spawn `spawn_at`-налог (10k архетип-поисков на 10k спавнов). `entities` — либо
    /// все зарезервированы (системный путь; их записи материализовал предшествующий
    /// `flush_reserved`), либо все `PLACEHOLDER` (standalone-`Commands` — тогда id аллоцируются здесь
    /// одним `allocate_batch`).
    pub(crate) fn spawn_bundles_bulk<B: Bundle>(&mut self, entities: Vec<Entity>, bundles: Vec<B>) {
        let count = bundles.len();
        if count == 0 {
            return;
        }
        debug_assert_eq!(entities.len(), count);

        // PLACEHOLDER (standalone) ⇒ аллоцируем свежие id; иначе берём зарезервированные.
        let placeholder = entities[0] == Entity::PLACEHOLDER;
        debug_assert!(
            entities
                .iter()
                .all(|&e| (e == Entity::PLACEHOLDER) == placeholder),
            "пачка spawn-команд одного Commands однородна по наличию резерватора"
        );
        let entities: Vec<Entity> = if placeholder {
            self.entities.allocate_batch(count)
        } else {
            for &e in &entities {
                self.entities.ensure_record(e.index());
            }
            entities
        };

        // Резолв архетипа/ids/колонок — ОДИН раз на пачку (вместо per-spawn в spawn_at).
        // `decl_ids` (порядок объявления = обхода write_into_batch) ОТДЕЛЬНО от `ids` (сорт. для
        // архетипа) — col_indices строится из decl_ids, иначе компонент пишется в чужую колонку (UB).
        let mut decl_ids: SmallVec<[ComponentId; 8]> = SmallVec::new();
        bundles[0].push_component_ids(&mut self.registry, &mut decl_ids);
        let mut ids = decl_ids.clone();
        ids.sort_unstable();
        if ids.is_empty() {
            // Пустой бандл (`spawn(())`) — в EMPTY-архетип.
            for (i, _bundle) in bundles.into_iter().enumerate() {
                let entity = entities[i];
                let row = unsafe { self.archetypes[0].allocate_row(entity) } as u32;
                self.entities.set_location(
                    entity,
                    EntityLocation {
                        archetype_id: ArchetypeId::EMPTY,
                        row,
                    },
                );
            }
            return;
        }
        let archetype_id = self.get_or_create_archetype(&ids);
        let arch_idx = archetype_id.0 as usize;
        let start_row = self.archetypes[arch_idx].entities.len();
        let tick = self.current_tick;
        self.archetypes[arch_idx].entities.reserve(count);
        for col in &mut self.archetypes[arch_idx].columns {
            col.reserve(count);
        }
        let col_indices: SmallVec<[usize; 8]> = decl_ids
            .iter()
            .filter_map(|&id| self.archetypes[arch_idx].column_index(id))
            .collect();

        // Бандлы РАЗНЫЕ per-item ⇒ пишем ДАННЫЕ каждого через write_data_into_batch (с предвычисленными
        // col_indices в порядке обхода — без повторного get_or_register/архетип-поиска); тики/len —
        // поколоночно к абсолютному target (устойчиво к data-only override и дефолту).
        for (i, bundle) in bundles.into_iter().enumerate() {
            let entity = entities[i];
            let row = start_row + i;
            self.archetypes[arch_idx].entities.push(entity);
            bundle.write_data_into_batch(self, archetype_id, row, tick, &col_indices);
        }
        let target_len = start_row + count;
        for &col_idx in &col_indices {
            let col = &mut self.archetypes[arch_idx].columns[col_idx];
            col.change_ticks.resize(target_len, tick);
            col.added_ticks.resize(target_len, tick);
            col.len = target_len;
        }
        self.entities
            .set_locations_batch(&entities, archetype_id, start_row as u32);

        if self.registry.any_flags() {
            let flagged: SmallVec<[ComponentId; 8]> = ids
                .iter()
                .copied()
                .filter(|&cid| self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0)
                .collect();
            if !flagged.is_empty() {
                for &entity in &entities {
                    for &cid in &flagged {
                        self.hook_queue.push(HookEvent::Added(entity, cid));
                    }
                }
                self.flush_hooks();
            }
        }
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
                    // replace_at дропает СТАРОЕ значение (W2-1: write_at молча
                    // терял его — утечка для Drop-типов: String, Vec, Arc…).
                    col.replace_at(
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
        if self.registry.any_flags()
            && self.registry.flags(component_id) & crate::component::ADDED_NOTIFY_MASK != 0
        {
            self.hook_queue.push(HookEvent::Added(entity, component_id));
            self.flush_hooks();
        }
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
                        // replace_at: дроп старого значения (см. W2-1).
                        col.replace_at(location.row as usize, data.as_ptr(), tick);
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
        if self.registry.any_flags()
            && self.registry.flags(component_id) & crate::component::ADDED_NOTIFY_MASK != 0
        {
            self.hook_queue.push(HookEvent::Added(entity, component_id));
            self.flush_hooks();
        }
    }

    /// Групповая вставка компонентов одной entity (W2-1): ОДИН archetype move
    /// на всю пачку вместо move-на-компонент. Используется `Commands::apply`
    /// для бёрстов `insert` на одну entity.
    ///
    /// `parts` — (ComponentId, указатель на значение, tick). Значения
    /// ПЕРЕДАЮТСЯ ВО ВЛАДЕНИЕ (байтовая копия в колонку; вызывающий обязан
    /// `forget`-нуть источник / не дропать байты). Уже существующие компоненты
    /// перезаписываются с дропом старого значения; дубликаты в пачке
    /// применяются по порядку (выживает последний, промежуточные дропаются).
    ///
    /// Возвращает `false` (ничего не записано), если entity мертва —
    /// вызывающий обязан сам освободить payload'ы.
    pub(crate) fn insert_parts(
        &mut self,
        entity: Entity,
        parts: &[(ComponentId, *const u8, Tick)],
    ) -> bool {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return false,
        };

        // Финальный архетип — цепочкой add_edges, БЕЗ перемещения данных.
        // Попутно собираем СВЕЖЕдобавленные компоненты с on_add-подпиской
        // (дубликат в пачке второй раз сюда не попадает — компонент уже в
        // target-составе).
        let any_flags = self.registry.any_flags();
        let mut added_hooked: SmallVec<[ComponentId; 8]> = SmallVec::new();
        let mut target = location.archetype_id;
        for &(cid, _, _) in parts {
            if !self.archetypes[target.0 as usize].has_component(cid) {
                target = self.find_or_create_archetype_with(target, cid);
                if any_flags && self.registry.flags(cid) & crate::component::ADDED_NOTIFY_MASK != 0 {
                    added_hooked.push(cid);
                }
            }
        }

        let row = if target != location.archetype_id {
            let new_row = self.move_entity(entity, location, target);
            self.entities.set_location(
                entity,
                EntityLocation {
                    archetype_id: target,
                    row: new_row,
                },
            );
            new_row as usize
        } else {
            location.row as usize
        };

        let arch = &mut self.archetypes[target.0 as usize];
        for &(cid, ptr, tick) in parts {
            // Новая колонка (len == row) — push; существующая — replace с
            // дропом старого значения.
            unsafe { arch.write_or_replace_component(row, cid, ptr, tick) };
        }
        for &cid in &added_hooked {
            self.hook_queue.push(HookEvent::Added(entity, cid));
        }
        self.flush_hooks();
        true
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
        if self.registry.any_flags() {
            self.notify_removed(entity, component_id);
            self.flush_hooks();
        }
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
        if self.registry.any_flags() {
            self.notify_removed(entity, component_id);
            self.flush_hooks();
        }
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
                    if self.relations.has_remove_hook(kind_idx) {
                        for &s in &subjects {
                            self.hook_queue.push(HookEvent::RelationRemoved {
                                kind_idx,
                                subject: s,
                                target: cur,
                            });
                        }
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
                if self.relations.has_remove_hook(pair.kind_idx) {
                    self.hook_queue.push(HookEvent::RelationRemoved {
                        kind_idx: pair.kind_idx,
                        subject: cur,
                        target: pair.target,
                    });
                }
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

            // Уведомления о потере ВСЕХ компонентов entity (on_remove /
            // Removed<T>); хуки увидят entity уже мёртвой — после despawn.
            if self.registry.any_flags() {
                let ids: SmallVec<[ComponentId; 8]> = self.archetypes[arch_idx]
                    .component_ids
                    .iter()
                    .copied()
                    .filter(|&cid| self.registry.flags(cid) != 0)
                    .collect();
                for cid in ids {
                    self.notify_removed(cur, cid);
                }
            }

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
        self.flush_hooks();
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

    /// Кешированный типизированный запрос (как Bevy `world.query::<Q>()`;
    /// зеркало `ctx.query` в системах). Список архетипов берётся из
    /// инкрементального глобального кэша.
    pub fn query<Q: WorldQuery>(&self) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, Tick::ZERO)
    }

    /// То же с явной базой change-detection (`Changed<T>`/`Added<T>` в Q).
    pub fn query_changed<Q: WorldQuery>(&self, last_run: Tick) -> CachedQuery<'_, Q> {
        CachedQuery::new(self, last_run)
    }

    /// Динамический запрос по runtime-`ComponentId` (редкий случай: типы не
    /// известны статически — скриптинг/инспектор). Для обычного кода —
    /// типизированный [`query`](Self::query).
    pub fn query_builder(&self) -> QueryBuilder<'_> {
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
                    // Перенос строки сохраняет ОБА тика: archetype move не
                    // «обновляет» ни Changed<T>, ни Added<T> (W3-1).
                    let changed = self.archetypes[from_idx].columns[i].get_tick(from_row);
                    let added = self.archetypes[from_idx].columns[i].get_added_tick(from_row);
                    self.archetypes[to_idx].columns[to_col_idx].push_moved_ticks(changed, added);

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
        let cmds = if let Some(deferred_cmds) = self.deferred_cmds {
            unsafe {
                let thread_idx = rayon::current_thread_index().unwrap_or(0);
                let vec = &mut *deferred_cmds;
                &mut vec[thread_idx]
            }
        } else {
            // SAFETY: inline_cmds используется только когда deferred_cmds не задан
            // (sequential режим). В этом случае доступ exclusive — один поток.
            unsafe { &mut *self.inline_cmds.get() }
        };
        // Единая точка внедрения резерватора: любой `cmd.spawn().id()` из системы получает
        // настоящий cross-frame `Entity` (резерватор делит атомарный high-water с аллокатором того
        // же мира, к которому команды и применятся). Идемпотентно — Arc-clone раз на жизнь Commands.
        if !cmds.has_reserver() {
            cmds.set_reserver(self.world().entity_reserver());
        }
        cmds
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

/// Список архетипов запроса: shared из глобального `QueryCache` ЛИБО
/// заимствованный (per-system индексы SubWorld / [`QueryState`]).
///
/// Заимствование — ключ к нулевой стоимости `ctx.query` (W2-0): раньше каждый
/// вызов копировал весь список индексов в новый `Arc<[usize]>` (heap-аллокация,
/// растущая с числом архетипов системы).
#[derive(Clone)]
enum ArchIndices<'w> {
    Shared(Arc<[usize]>),
    Borrowed(&'w [usize]),
}

impl<'w> ArchIndices<'w> {
    /// Слайс с лайфтаймом 'w (для Shared — через &self, безопасно: Arc жив,
    /// пока жив владелец; итератор держит собственный clone).
    #[inline]
    fn as_slice(&self) -> &[usize] {
        match self {
            ArchIndices::Shared(arc) => arc,
            ArchIndices::Borrowed(s) => s,
        }
    }
}

pub struct CachedQuery<'w, Q: WorldQuery> {
    world: &'w World,
    arch_indices: ArchIndices<'w>,
    last_run: Tick,
    cached_ids: SmallVec<[ComponentId; 8]>,
    row_ranges: &'w [(usize, usize, usize)],
    /// `true`, если каждый индекс в `arch_indices` УЖЕ прошёл `matches_archetype`
    /// (конструкторы `new`/`from_state_parts`: список — точный матч). Тогда
    /// итерация пропускает per-archetype re-check (набор компонентов архетипа
    /// неизменен ⇒ матч не меняется). `from_sub_world` ставит `false`: его индексы —
    /// суперсет планировщика, фильтрация обязана происходить при обходе.
    match_verified: bool,
    _phantom: std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> CachedQuery<'w, Q> {
    pub fn new(world: &'w World, last_run: Tick) -> Self {
        // Один проход реестра: ключ кэша несёт и ids (нижние 32 бита каждой
        // НЕ-маркерной записи, в порядке fill_ids — инвариант fill_cache_key),
        // и роли формы. Структурные маркеры Or<> (KEY_MARKER_BIT) пропускаются.
        let mut key: SmallVec<[u64; 8]> = SmallVec::new();
        Q::fill_cache_key(world, &mut key);
        let ids: SmallVec<[ComponentId; 8]> = key
            .iter()
            .filter(|&&e| e & crate::query::KEY_MARKER_BIT == 0)
            .map(|&e| ComponentId(e as u32))
            .collect();
        debug_assert_eq!(ids.len(), Q::component_count(), "инвариант fill_cache_key нарушен");

        let arch_indices = world
            .query_cache
            .get_or_compute(&key, &world.archetypes, |arch| {
                Q::matches_archetype(arch, &ids)
            });

        Self {
            world,
            arch_indices: ArchIndices::Shared(arch_indices),
            last_run,
            cached_ids: ids,
            row_ranges: &[],
            match_verified: true, // get_or_compute отфильтровал по matches_archetype
            _phantom: std::marker::PhantomData,
        }
    }

    /// Создать CachedQuery с ограничением на архетипы и строки из SubWorld.
    ///
    /// Zero-alloc (W2-0): индексы архетипов ЗАИМСТВУЮТСЯ у SubWorld (они
    /// предвычислены планировщиком per-system и дополняются инкрементально),
    /// ids — inline-SmallVec. Не вызывает `get_or_compute` (thread-safe для
    /// параллельных систем). Фильтрация по `Q::matches_archetype` происходит
    /// в `for_each`/`par_for_each` — `fetch_state` только для совпадающих.
    pub fn from_sub_world(sub: &'w SubWorld<'w>, last_run: Tick) -> Self {
        let mut ids = crate::query::IdBuf::new();
        Q::fill_ids(sub.world(), &mut ids);

        Self {
            world: sub.world(),
            arch_indices: ArchIndices::Borrowed(sub.archetype_indices()),
            last_run,
            cached_ids: ids,
            row_ranges: sub.row_ranges(),
            match_verified: false, // индексы планировщика — суперсет, фильтруем при обходе
            _phantom: std::marker::PhantomData,
        }
    }

    /// Внутренний конструктор для [`QueryState`]: заимствует готовые индексы
    /// и ids стейта — ноль аллокаций, ноль локов.
    pub(crate) fn from_state_parts(
        world: &'w World,
        arch_indices: &'w [usize],
        ids: &[ComponentId],
        last_run: Tick,
    ) -> Self {
        Self {
            world,
            arch_indices: ArchIndices::Borrowed(arch_indices),
            last_run,
            cached_ids: ids.iter().copied().collect(),
            row_ranges: &[],
            match_verified: true, // QueryState.update отфильтровал по matches_archetype
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
        debug_assert_eq!(ids.len(), Q::component_count(), "инвариант fill_ids нарушен");
        let this_run = self.world.current_tick();
        for &arch_idx in self.arch_indices.as_slice() {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() {
                continue;
            }
            if !self.match_verified && !Q::matches_archetype(arch, ids) {
                continue;
            }
            let state = unsafe { Q::fetch_state(arch, ids, self.last_run, this_run) };
            let (row_start, row_end) = self.row_range(arch_idx);
            let end = row_end.min(arch.len());
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let entities = &arch.entities[row_start..end];
            if Q::has_row_filter() {
                // Построчный фильтр (`Changed`/`Added`/`Or` с ними): entity грузим ЛЕНИВО —
                // только для прошедших строк (`fetch_item` вернул `Some`). Убирает второй
                // поток памяти на непрошедших строках (~1.5× на разрежённых изменениях).
                for offset in 0..len {
                    let row = row_start + offset;
                    if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                        f(entities[offset], item);
                    }
                }
            } else {
                // Архетип-уровневая форма: `fetch_item` инфаллибелен для совпавшего
                // архетипа ⇒ плотный цикл без per-row Option-ветки (перф-кампания §3.1A,
                // Bevy «archetype-level filter» fast-path). Семантика не меняется
                // (`Mut<T>` по-прежнему стампит change-tick на `DerefMut`).
                for offset in 0..len {
                    let item = unsafe { Q::fetch_item_unchecked(state, row_start + offset) };
                    f(entities[offset], item);
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

        // ids — owned clone (inline-копия SmallVec), как в Query::par_for_each
        let ids = self.cached_ids.clone();

        let world = self.world;
        let last_run = self.last_run;
        let row_ranges = self.row_ranges;
        let match_verified = self.match_verified;
        let rr = |arch_idx: usize| -> (usize, usize) {
            row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX))
        };
        let chunks = compute_par_chunks(
            self.arch_indices
                .as_slice()
                .iter()
                .copied()
                .filter(|&arch_idx| !world.archetypes[arch_idx].is_empty())
                .filter(|&arch_idx| match_verified || Q::matches_archetype(&world.archetypes[arch_idx], &ids))
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
            if Q::has_row_filter() {
                for (offset, &entity) in entities.iter().enumerate() {
                    let row = clamped_start + offset;
                    if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                        f(entity, item);
                    }
                }
            } else {
                // Архетип-уровневая форма: плотный цикл без Option-ветки (§3.1A).
                for (offset, &entity) in entities.iter().enumerate() {
                    let item = unsafe { Q::fetch_item_unchecked(state, clamped_start + offset) };
                    f(entity, item);
                }
            }
        });
    }

    pub fn len(&self) -> usize {
        self.arch_indices
            .as_slice()
            .iter()
            .map(|&i| self.world.archetypes[i].len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.arch_indices
            .as_slice()
            .iter()
            .all(|&i| self.world.archetypes[i].is_empty())
    }

    /// Плотная (chunk) итерация (W2-0.5) — см. [`Query::for_each_chunk`]
    /// (crate::query::Query::for_each_chunk): слайсы колонок вместо per-row
    /// `fetch_item`; write-слайсы стампятся диапазоном. `Changed<T>` не
    /// компилируется.
    pub fn for_each_chunk<F>(&self, mut f: F)
    where
        Q: crate::dense::DenseQuery,
        F: FnMut(&[Entity], <Q as crate::dense::DenseQuery>::Slices<'_>),
    {
        let ids = &self.cached_ids;
        let this_run = self.world.current_tick();
        for &arch_idx in self.arch_indices.as_slice() {
            let arch = &self.world.archetypes[arch_idx];
            if arch.is_empty() || (!self.match_verified && !Q::matches_archetype(arch, ids)) {
                continue;
            }
            let (row_start, row_end) = self.row_range(arch_idx);
            let end = row_end.min(arch.len());
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let slices = unsafe { Q::fetch_slices(arch, ids, row_start, len, this_run) };
            f(&arch.entities[row_start..end], slices);
        }
    }

    /// Параллельная плотная итерация — те же chunk-диапазоны, что у
    /// [`par_for_each`](Self::par_for_each), но колбэк получает слайсы.
    pub fn par_for_each_chunk<F>(&self, f: F)
    where
        Q: crate::dense::DenseQuery + Send,
        F: Fn(&[Entity], <Q as crate::dense::DenseQuery>::Slices<'_>) + Send + Sync,
    {
        use crate::par_utils::compute_par_chunks;
        use rayon::prelude::*;
        let num_threads = rayon::current_num_threads();

        let ids = self.cached_ids.clone();
        let world = self.world;
        let row_ranges = self.row_ranges;
        let rr = |arch_idx: usize| -> (usize, usize) {
            row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX))
        };
        let chunks = compute_par_chunks(
            self.arch_indices
                .as_slice()
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

        let this_run = world.current_tick();
        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let (r_start, r_end) = rr(arch_idx);
            let clamped_start = r_start + start;
            let clamped_end = (r_start + end).min(r_end);
            if clamped_start >= clamped_end {
                return;
            }
            let arch = unsafe { &*world.archetypes.as_ptr().add(arch_idx) };
            let len = clamped_end - clamped_start;
            let slices = unsafe { Q::fetch_slices(arch, &ids, clamped_start, len, this_run) };
            f(&arch.entities[clamped_start..clamped_end], slices);
        });
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
    arch_indices: ArchIndices<'w>,
    cached_ids: SmallVec<[ComponentId; 8]>,
    last_run: Tick,
    row_ranges: &'w [(usize, usize, usize)],

    arch_pos: usize,
    row: usize,
    row_end: usize,
    state: Option<Q::State>,
    _phantom: std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> Iterator for CachedQueryIter<'w, Q> {
    /// П1 (TD-8): итерация выдаёт ТОЛЬКО `Q::Item` (как `Query::iter`);
    /// entity — через форму запроса (`ctx.query::<(Entity, Read<A>)>()`).
    type Item = Q::Item<'w>;

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

            let state = *self.state.as_ref().unwrap();
            if Q::has_row_filter() {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    return Some(item);
                }
            } else {
                // Инфаллибельная форма: строка всегда выдаётся (§3.1A).
                return Some(unsafe { Q::fetch_item_unchecked(state, row) });
            }
        }
    }
}

impl<'w, Q: WorldQuery> CachedQueryIter<'w, Q> {
    /// Настроить следующий непустой совпадающий архетип (начиная с `arch_pos`).
    /// Возвращает `false`, когда архетипы кончились. Пропускает пустые/не-matching
    /// в цикле, поэтому первый архетип (индекс 0) НЕ теряется (см. TD-1).
    fn advance_archetype(&mut self) -> bool {
        while self.arch_pos < self.arch_indices.as_slice().len() {
            let arch_idx = self.arch_indices.as_slice()[self.arch_pos];
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
            return true;
        }
        false
    }
}

// ── QueryState — per-system стейт запроса (W2-0, модель Bevy) ──

/// Владелец долгоживущего стейта запроса: список матчащих архетипов +
/// разрешённые ComponentId. Дополняется ИНКРЕМЕНТАЛЬНО (архетипы append-only),
/// в устоявшемся состоянии `query()` — это одна проверка счётчика: ни локов,
/// ни hash-lookup'ов, ни аллокаций (в отличие от глобального `QueryCache`,
/// который платит ключ+hash+RwLock+Arc-clone на каждый вызов).
///
/// Привязан к конкретному миру по [`World::id`]: применение к другому миру
/// (main vs render vs isolated) прозрачно перестраивает стейт — ComponentId
/// одного мира не валидны в другом.
///
/// ```ignore
/// struct ExtractMeshes {
///     q: QueryState<(Read<Mesh>, Read<GlobalTransform>)>,
/// }
/// // в горячем цикле:
/// self.q.query(&world).for_each(|e, (mesh, gt)| { ... });
/// ```
pub struct QueryState<Q: WorldQuery> {
    world_id: u64,
    ids: crate::query::IdBuf,
    /// Все ли компоненты формы были зарегистрированы на момент апдейта.
    /// Пока нет — ids перепроверяются каждый вызов (регистрация ленивая),
    /// а скан архетипов не начинается.
    ids_resolved: bool,
    arch_indices: Vec<usize>,
    seen_arch_count: usize,
    _phantom: std::marker::PhantomData<fn() -> Q>,
}

impl<Q: WorldQuery> Default for QueryState<Q> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Q: WorldQuery> QueryState<Q> {
    /// Пустой стейт; к миру привяжется при первом [`query`](Self::query).
    pub fn new() -> Self {
        Self {
            world_id: 0,
            ids: crate::query::IdBuf::new(),
            ids_resolved: false,
            arch_indices: Vec::new(),
            seen_arch_count: 0,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Запрос с базой change-detection `world.last_run_tick()` (как
    /// `ctx.query` в системах).
    #[inline]
    pub fn query<'a>(&'a mut self, world: &'a World) -> CachedQuery<'a, Q> {
        self.query_with_tick(world, world.last_run_tick())
    }

    /// Запрос с явной базой `last_run` (для `Changed<T>`-форм с собственным
    /// отсчётом, например extract-систем).
    pub fn query_with_tick<'a>(&'a mut self, world: &'a World, last_run: Tick) -> CachedQuery<'a, Q> {
        self.update(world);
        CachedQuery::from_state_parts(world, &self.arch_indices, &self.ids, last_run)
    }

    /// Инкрементальный апдейт под мир: пересборка при смене мира, доразрешение
    /// ids (ленивая регистрация компонентов), доскан ТОЛЬКО новых архетипов.
    fn update(&mut self, world: &World) {
        if self.world_id != world.world_id {
            self.world_id = world.world_id;
            self.ids = crate::query::IdBuf::new();
            self.ids_resolved = false;
            self.arch_indices.clear();
            self.seen_arch_count = 0;
        }

        if !self.ids_resolved {
            // Сентинел INVALID = ещё не зарегистрированный компонент формы.
            // Он может быть ЛЕГИТИМЕН навсегда (Maybe/Without/мёртвая ветка
            // Or) — поэтому не блокируем скан, а перечитываем ids каждый
            // вызов; если разрешение изменилось (компонент зарегистрировали
            // позже) — пересканируем архетипы с нуля: матчи могли поменяться.
            let mut fresh = crate::query::IdBuf::new();
            Q::fill_ids(world, &mut fresh);
            if fresh != self.ids {
                self.ids = fresh;
                self.arch_indices.clear();
                self.seen_arch_count = 0;
            }
            self.ids_resolved = !self.ids.contains(&ComponentId::INVALID);
        }

        let total = world.archetypes.len();
        if self.seen_arch_count < total {
            for (i, arch) in world.archetypes[self.seen_arch_count..].iter().enumerate() {
                if Q::matches_archetype(arch, &self.ids) {
                    self.arch_indices.push(self.seen_arch_count + i);
                }
            }
            self.seen_arch_count = total;
        }
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

    /// Записать данные компонентов для batch-спавна ([`spawn_many`]). ПЕРЕОПРЕДЕЛЯЕТСЯ (leaf/tuple/
    /// derive) на data-only: пишет ТОЛЬКО данные (без change/added-тиков и без `col.len`), а тики/`len`
    /// вызывающий проставляет ПОКОЛОНОЧНО один раз на пачку (резко дешевле `count×ncols` push'ей).
    /// **Дефолт** (для ручных `impl Bundle`) — полный `write_into_batch` (данные+тики+len). Вызывающий
    /// устойчив к ОБОИМ: использует АБСОЛЮТНЫЙ target (`start_row+count`) при `resize`/`len`, поэтому
    /// уже-выставленные дефолтом тики/len — no-op, а для data-only override — заполняются. `tick`
    /// дефолтом используется, override'ами игнорируется.
    fn write_data_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        tick: Tick,
        col_indices: &[usize],
    ) {
        self.write_into_batch(world, archetype_id, row, tick, col_indices);
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
                col.added_ticks.push(tick);
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
            col.added_ticks.push(tick);
            col.len += 1;
        }
        std::mem::forget(self);
    }

    #[inline(always)]
    fn write_data_into_batch(
        self,
        world: &mut World,
        archetype_id: ArchetypeId,
        row: usize,
        _tick: Tick,
        col_indices: &[usize],
    ) {
        let col_idx = col_indices[0];
        // SAFETY: ёмкость зарезервирована вызывающим (`reserve(count)`), `row` в пределах; тики/`len`
        // вызывающий проставит поколоночно ПОСЛЕ записи данных всех строк (data-only).
        unsafe {
            let col = &mut world.archetypes[archetype_id.0 as usize].columns[col_idx];
            if col.item_size > 0 {
                let dst = col.get_ptr(row);
                std::ptr::copy_nonoverlapping(&self as *const T as *const u8, dst, col.item_size);
            }
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

            /// ВАЖНО (корректность batch-спавна): пушит id в порядке ОБЪЯВЛЕНИЯ кортежа — том же,
            /// в котором `write_into_batch` обходит компоненты. `component_ids` СОРТИРУЕТ (для
            /// архетипа), а `col_indices` для `write_into_batch` ОБЯЗАН быть в порядке обхода, иначе
            /// компонент пишется в чужую колонку (UB: запись 64B Matrix4 в 12B колонку). См.
            /// `spawn_many_inner`/`spawn_bundles_bulk` — они строят `col_indices` ИМЕННО отсюда.
            #[inline]
            fn push_component_ids(
                &self,
                registry: &mut ComponentRegistry,
                out: &mut SmallVec<[ComponentId; 8]>,
            ) {
                #[allow(non_snake_case)]
                let ($($T,)+) = self;
                $( $T.push_component_ids(registry, out); )+
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
            fn write_data_into_batch(
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
                    $T.write_data_into_batch(world, archetype_id, row, tick, &col_indices[_offset.._offset + _cnt]);
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

    // ── W2-3: tick-wrap кламп ──────────────────────────────────

    #[test]
    fn check_change_ticks_clamps_stale_rows() {
        use crate::query::{Changed, Read};

        struct P(#[allow(dead_code)] f32);
        impl crate::component::Component for P {}

        let mut world = World::new();
        let _e = world.spawn((P(0.0),)); // change-tick строки = текущий тик

        // Симулируем долгий аптайм: прыжки < 2³¹ с клампом между ними —
        // суммарно далеко за период переполнения.
        for _ in 0..4 {
            world.current_tick.0 = world.current_tick.0.wrapping_add(1 << 30);
            world.check_change_ticks();
        }

        // last_run «вчера»: строка очень старая и НЕ должна быть Changed.
        let last_run = Tick(world.current_tick.0.wrapping_sub(2));
        let changed = crate::query::Query::<(Changed<P>, Read<P>)>::new_with_tick(&world, last_run)
            .iter()
            .count();
        assert_eq!(changed, 0, "кламп удержал строку в «давно не менялась»");

        // Контроль: БЕЗ клампа тот же сценарий даёт ложно-Changed.
        let mut world2 = World::new();
        let _e2 = world2.spawn((P(0.0),));
        world2.current_tick.0 = world2.current_tick.0.wrapping_add(1 << 31).wrapping_add(8);
        let last_run2 = Tick(world2.current_tick.0.wrapping_sub(2));
        let false_changed =
            crate::query::Query::<(Changed<P>, Read<P>)>::new_with_tick(&world2, last_run2)
                .iter()
                .count();
        assert_eq!(false_changed, 1, "санити: без клампа wrap даёт ложный Changed");
    }

    // ── W2-0: QueryState ───────────────────────────────────────

    #[test]
    fn query_state_incremental_and_reusable() {
        use crate::query::Read;

        struct P(f32);
        impl crate::component::Component for P {}
        struct Tag;
        impl crate::component::Component for Tag {}

        let mut world = World::new();
        world.spawn((P(1.0),));

        let mut state = QueryState::<Read<P>>::new();
        assert_eq!(state.query(&world).iter().count(), 1);

        // Новый архетип ПОСЛЕ первого запроса — стейт дополняется хвостом.
        world.spawn((P(2.0), Tag));
        assert_eq!(state.query(&world).iter().count(), 2);

        // Повторный вызов без изменений — чистый hit (ничего не пересканируется).
        assert_eq!(state.query(&world).iter().count(), 2);
        let mut sum = 0.0;
        state.query(&world).for_each(|_, p| sum += p.0);
        assert_eq!(sum, 3.0);
    }

    #[test]
    fn query_state_rebinds_to_other_world() {
        use crate::query::Read;

        struct P(#[allow(dead_code)] f32);
        impl crate::component::Component for P {}

        let mut a = World::new();
        let mut b = World::new();
        a.spawn((P(0.0),));
        b.spawn((P(0.0),));
        b.spawn((P(0.0),));

        let mut state = QueryState::<Read<P>>::new();
        assert_eq!(state.query(&a).iter().count(), 1);
        assert_eq!(state.query(&b).iter().count(), 2, "стейт перепривязался к миру B");
        assert_eq!(state.query(&a).iter().count(), 1, "и обратно к A");
    }

    #[test]
    fn query_state_resolves_late_registered_component() {
        use crate::query::Read;

        struct Late(#[allow(dead_code)] u32);
        impl crate::component::Component for Late {}

        let mut world = World::new();
        let mut state = QueryState::<Read<Late>>::new();
        // Компонент ещё не зарегистрирован — запрос пуст, но не падает.
        assert_eq!(state.query(&world).iter().count(), 0);

        world.spawn((Late(7),));
        assert_eq!(state.query(&world).iter().count(), 1, "ids доразрешились после регистрации");
    }

    #[test]
    fn query_state_changed_with_explicit_tick() {
        use crate::query::{Changed, Read};

        struct P(f32);
        impl crate::component::Component for P {}

        let mut world = World::new();
        let target = world.spawn((P(0.0),));
        let _other = world.spawn((P(0.0),));

        world.tick();
        let last_run = world.current_tick();
        world.tick();

        if let Some(p) = world.get_mut::<P>(target) {
            p.0 = 1.0;
        }

        let mut state = QueryState::<(Entity, Changed<P>, Read<P>)>::new();
        let hits: Vec<_> = state
            .query_with_tick(&world, last_run)
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert_eq!(hits, vec![target]);
    }

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
            .query_changed::<(Entity, Read<Pos>)>(Tick::ZERO)
            .iter()
            .map(|(e, _)| e)
            .collect();
        assert_eq!(got.len(), 2, "iter() должен вернуть ОБА entity (не пропустить первый)");
        assert!(got.contains(&a) && got.contains(&b));

        // Несколько архетипов: (Pos) и (Pos, Vel).
        let c = world.spawn((Pos { x: 3.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        let got2: Vec<_> = world
            .query_changed::<(Entity, Read<Pos>)>(Tick::ZERO)
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
        let with_vel = world.query::<(Read<Pos>, Read<Vel>)>().len();
        assert_eq!(with_vel, 1);

        // …затем (Read, Without) обязан увидеть СВОЙ список архетипов.
        let mut seen = Vec::new();
        world
            .query::<(Read<Pos>, Without<Vel>)>()
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
        assert_eq!(world.query::<Read<Pos>>().len(), 1);

        // Новый архетип (Pos, Vel) после прогрева кэша.
        world.spawn((Pos { x: 2.0, y: 0.0 }, Vel { x: 0.0, y: 0.0 }));
        assert_eq!(world.query::<Read<Pos>>().len(), 2);
    }

    /// CR-M2: entity, въехавшая в ОПУСТЕВШИЙ архетип, не теряется кэшем
    /// (пустые архетипы остаются в списках; insert/remove кэш не сбрасывают).
    #[test]
    fn cached_query_sees_entity_in_repopulated_archetype() {
        use crate::query::Read;

        let mut world = World::new();
        let e = world.spawn((Pos { x: 1.0, y: 0.0 }, Vel { x: 3.0, y: 0.0 }));
        assert_eq!(world.query::<(Read<Pos>, Read<Vel>)>().len(), 1);

        // Архетип (Pos, Vel) пустеет…
        world.remove::<Vel>(e);
        assert_eq!(world.query::<(Read<Pos>, Read<Vel>)>().len(), 0);

        // …и снова наполняется — кэшированный список обязан его видеть.
        world.insert(e, Vel { x: 4.0, y: 0.0 });
        let mut seen = Vec::new();
        world
            .query::<(Read<Pos>, Read<Vel>)>()
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
        struct Rare(#[allow(dead_code)] u32);
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
        let got: Vec<_> = Query::<(Entity, Read<Pos>, Read<Rare>)>::new(&world)
            .iter()
            .map(|(e, _, _)| e)
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

    /// Регресс: `spawn_many`/bulk-path обязан писать компоненты в КОЛОНКУ ПО ИХ ID, а не позиционно
    /// в порядке объявления. Баг (до фикса col_indices): `col_indices` строился из ОТСОРТИРОВАННЫХ
    /// id, а `write_into_batch` потреблял их в порядке ОБЪЯВЛЕНИЯ ⇒ при «порядок объявления ≠ порядок
    /// id» компонент писался в чужую колонку (UB: 64B в 1B-колонку, повреждение данных — проявлялось
    /// как heavy_compute-регресс). Здесь порядок объявления (Big, Small) ОБРАТЕН порядку id
    /// (Small зарегистрирован первым ⇒ меньший id).
    #[test]
    fn spawn_many_writes_components_by_id_not_declaration_position() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct BigComp([u64; 8]); // 64 байта
        impl Component for BigComp {}
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct SmallComp(u8); // 1 байт
        impl Component for SmallComp {}

        let mut world = World::new();
        // Small РАНЬШЕ Big ⇒ id(Small) < id(Big). Порядок объявления бандла — ОБРАТНЫЙ.
        world.register_component::<SmallComp>();
        world.register_component::<BigComp>();

        let entities = world.spawn_many(256, |i| (BigComp([i as u64; 8]), SmallComp(0xAB)));
        assert_eq!(entities.len(), 256);

        for (i, &e) in entities.iter().enumerate() {
            let big = world.get::<BigComp>(e).expect("BigComp присутствует");
            let small = world.get::<SmallComp>(e).expect("SmallComp присутствует");
            assert_eq!(
                big.0, [i as u64; 8],
                "BigComp entity[{i}] повреждён — компонент записан в чужую колонку (col_indices order)"
            );
            assert_eq!(small.0, 0xAB, "SmallComp entity[{i}] повреждён");
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

// ── W3-1: hooks/observers + Added/Removed ──────────────────────

#[cfg(test)]
mod hooks_and_added_tests {
    use super::*;
    use crate::query::{Added, Changed, Query, Read};

    #[derive(Debug, PartialEq)]
    struct Hp(u32);
    impl Component for Hp {}

    #[derive(Debug, PartialEq)]
    struct Armor(u32);
    impl Component for Armor {}

    /// Лог вызовов хуков (ресурс — состояние подписчика живёт в ресурсах).
    #[derive(Default)]
    struct HookLog {
        added: Vec<Entity>,
        removed: Vec<Entity>,
        removed_alive: Vec<bool>,
        rel_added: Vec<(Entity, Entity)>,
        rel_removed: Vec<(Entity, Entity)>,
    }

    fn log_world() -> World {
        let mut w = World::new();
        w.insert_resource(HookLog::default());
        w
    }

    // ── Added<T> ───────────────────────────────────────────────

    #[test]
    fn added_detects_fresh_spawn_and_expires_next_frame() {
        let mut world = World::new();
        world.spawn((Hp(1),));

        let lr = world.last_run_tick();
        let n = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 1, "свежий spawn виден Added<T>");

        world.advance_change_tick();
        let lr = world.last_run_tick();
        let n = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 0, "на следующем кадре Added<T> истекает");
    }

    #[test]
    fn added_survives_archetype_move_and_not_retriggered() {
        let mut world = World::new();
        let e = world.spawn((Hp(1),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        // insert Armor двигает entity в новый архетип: Added<Armor> — да,
        // Added<Hp> — НЕТ (added-тик пережил перенос).
        world.insert(e, Armor(5));
        let added_armor = Query::<(Added<Armor>, Read<Armor>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        let added_hp = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(added_armor, 1);
        assert_eq!(added_hp, 0, "archetype move не «обновляет» Added");
    }

    #[test]
    fn reinsert_existing_is_changed_but_not_added() {
        let mut world = World::new();
        let e = world.spawn((Hp(1),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        world.insert(e, Hp(2)); // replace существующего
        let added = Query::<(Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        let changed = Query::<(Changed<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(added, 0, "replace не перезапускает Added (как Bevy)");
        assert_eq!(changed, 1, "replace помечает Changed");
        assert_eq!(world.get::<Hp>(e), Some(&Hp(2)));
    }

    #[test]
    fn added_alignment_survives_swap_remove() {
        let mut world = World::new();
        let e0 = world.spawn((Hp(0),));
        let _e1 = world.spawn((Hp(1),));
        let _e2 = world.spawn((Hp(2),));
        world.advance_change_tick();
        let lr = world.last_run_tick();

        let e3 = world.spawn((Hp(3),)); // единственный «свежий»
        world.despawn(e0); // swap_remove: e3 переедет на строку 0

        let fresh: Vec<Entity> = Query::<(Entity, Added<Hp>, Read<Hp>)>::new_with_tick(&world, lr)
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert_eq!(
            fresh,
            vec![e3],
            "swap_remove сохраняет выравнивание added-тиков"
        );
    }

    // ── on_add / on_remove ─────────────────────────────────────

    #[test]
    fn on_add_fires_for_spawn_insert_and_commands_burst() {
        let mut world = log_world();
        world.on_add::<Hp>(|w, e| w.resource_mut::<HookLog>().added.push(e));

        let a = world.spawn((Hp(1),)); // spawn
        let b = world.spawn((Armor(0),));
        world.insert(b, Hp(2)); // insert (archetype move)

        // Commands-бёрст → insert_parts (групповой путь W2-1).
        let c = world.spawn((Armor(0),));
        let mut cmds = Commands::new();
        cmds.insert(c, Hp(3));
        cmds.insert(c, Armor(1)); // replace — НЕ on_add
        cmds.apply(&mut world);

        assert_eq!(world.resource::<HookLog>().added, vec![a, b, c]);
    }

    #[test]
    fn on_add_fires_for_spawn_many() {
        let mut world = log_world();
        world.on_add::<Hp>(|w, e| w.resource_mut::<HookLog>().added.push(e));
        let spawned = world.spawn_many(3, |i| (Hp(i as u32),));
        assert_eq!(world.resource::<HookLog>().added, spawned);
    }

    #[test]
    fn on_add_hook_can_do_structural_changes() {
        // Хук дотягивает Armor каждому, кто получил Hp (прообраз required
        // components D2-4). Вложенный insert идёт через ту же очередь.
        let mut world = World::new();
        world.on_add::<Hp>(|w, e| {
            if !w.has_component::<Armor>(e) {
                w.insert(e, Armor(100));
            }
        });
        let e = world.spawn((Hp(1),));
        assert_eq!(world.get::<Armor>(e), Some(&Armor(100)));
        assert_eq!(world.get::<Hp>(e), Some(&Hp(1)));
    }

    #[test]
    fn on_remove_fires_for_remove_and_despawn_with_dead_entity() {
        let mut world = log_world();
        world.on_remove::<Hp>(|w, e| {
            let alive = w.is_alive(e);
            let log = w.resource_mut::<HookLog>();
            log.removed.push(e);
            log.removed_alive.push(alive);
        });

        let a = world.spawn((Hp(1),));
        world.remove::<Hp>(a); // remove: entity жива
        let b = world.spawn((Hp(2),));
        world.despawn(b); // despawn: entity мертва

        let log = world.resource::<HookLog>();
        assert_eq!(log.removed, vec![a, b]);
        assert_eq!(log.removed_alive, vec![true, false]);
    }

    #[test]
    fn on_remove_fires_for_cascade_despawn() {
        let mut world = log_world();
        world.on_remove::<Hp>(|w, e| w.resource_mut::<HookLog>().removed.push(e));

        let parent = world.spawn((Armor(0),));
        let child = world.spawn((Hp(1),));
        world.add_relation(child, crate::relations::ChildOf, parent);

        world.despawn(parent); // каскад сносит child
        assert!(!world.is_alive(child));
        assert_eq!(world.resource::<HookLog>().removed, vec![child]);
    }

    #[test]
    #[should_panic(expected = "уже зарегистрирован")]
    fn double_on_add_registration_panics() {
        let mut world = World::new();
        world.on_add::<Hp>(|_, _| {});
        world.on_add::<Hp>(|_, _| {});
    }

    // ── Relation hooks ─────────────────────────────────────────

    #[test]
    fn relation_hooks_fire_on_add_remove_and_despawn_cleanup() {
        let mut world = log_world();
        world.on_relation_add::<crate::relations::Owns>(|w, s, t| {
            w.resource_mut::<HookLog>().rel_added.push((s, t))
        });
        world.on_relation_remove::<crate::relations::Owns>(|w, s, t| {
            w.resource_mut::<HookLog>().rel_removed.push((s, t))
        });

        let owner = world.spawn((Hp(1),));
        let item = world.spawn((Armor(0),));
        world.add_relation(owner, crate::relations::Owns, item);
        world.remove_relation(owner, crate::relations::Owns, item);

        // Повторная связь — вычистка через despawn target'а.
        world.add_relation(owner, crate::relations::Owns, item);
        world.despawn(item);

        let log = world.resource::<HookLog>();
        assert_eq!(log.rel_added, vec![(owner, item), (owner, item)]);
        assert_eq!(
            log.rel_removed,
            vec![(owner, item), (owner, item)],
            "explicit remove + despawn-вычистка"
        );
    }

    // ── track_removals / Removed<T> ────────────────────────────

    #[test]
    fn removed_events_emitted_for_remove_and_despawn() {
        let mut world = World::new();
        world.track_removals::<Hp>();

        let a = world.spawn((Hp(1),));
        let b = world.spawn((Hp(2),));
        world.remove::<Hp>(a);
        world.despawn(b);

        world.flush_all_events();
        let mut reader = world.event_reader::<crate::events::Removed<Hp>>();
        let got: Vec<Entity> = reader.read().iter().map(|r| r.entity).collect();
        assert_eq!(got, vec![a, b]);
    }

    // ── D2-4: required components ──────────────────────────────

    #[test]
    fn required_components_via_derive_attr() {
        #[derive(apex_macros::Component, Default, Debug, PartialEq)]
        struct LocalTf(u32);
        #[derive(apex_macros::Component, Default, Debug, PartialEq)]
        struct GlobalTf(u32);

        #[derive(apex_macros::Component)]
        #[require(LocalTf, GlobalTf)]
        struct Renderer;

        let mut world = World::new(); // derive-регистраторы через linkme
        let e = world.spawn((Renderer,));
        assert_eq!(
            world.get::<LocalTf>(e),
            Some(&LocalTf(0)),
            "недостающий required дотянут дефолтом"
        );
        assert_eq!(world.get::<GlobalTf>(e), Some(&GlobalTf(0)));

        // Явно заданное значение выигрывает у дефолта.
        let e2 = world.spawn((Renderer, LocalTf(7)));
        assert_eq!(world.get::<LocalTf>(e2), Some(&LocalTf(7)));
        assert_eq!(world.get::<GlobalTf>(e2), Some(&GlobalTf(0)));

        // insert-путь тоже дотягивает.
        let e3 = world.spawn((Hp(1),));
        world.insert(e3, Renderer);
        assert_eq!(world.get::<GlobalTf>(e3), Some(&GlobalTf(0)));
    }

    #[test]
    fn required_components_transitive_and_manual_api() {
        // C требует B, B требует A — ручной API (для типов с ручным
        // impl Component, как в движке).
        #[derive(Default, Debug, PartialEq)]
        struct A(u8);
        impl Component for A {}
        #[derive(Default, Debug, PartialEq)]
        struct B(u8);
        impl Component for B {}
        struct C;
        impl Component for C {}

        let mut world = World::new();
        world.require_component::<C, B>();
        world.require_component::<B, A>();

        let e = world.spawn((C,));
        assert_eq!(world.get::<B>(e), Some(&B(0)), "прямое требование");
        assert_eq!(world.get::<A>(e), Some(&A(0)), "транзитивное через очередь");
    }

    #[test]
    fn required_components_user_on_add_sees_full_entity() {
        struct C;
        impl Component for C {}
        #[derive(Default)]
        struct R(#[allow(dead_code)] u8);
        impl Component for R {}

        let mut world = log_world();
        world.require_component::<C, R>();
        // on_add владельца вызывается ПОСЛЕ дотяжки requires.
        world.on_add::<C>(|w, e| {
            assert!(
                w.has_component::<R>(e),
                "требуемый компонент уже на месте при вызове on_add"
            );
            w.resource_mut::<HookLog>().added.push(e);
        });
        let e = world.spawn((C,));
        assert_eq!(world.resource::<HookLog>().added, vec![e]);
    }

    // ── W3-5: память в archetype_stats ─────────────────────────

    #[test]
    fn archetype_stats_reports_memory() {
        let mut world = World::new();
        world.spawn_many(100, |i| (Hp(i as u32),));
        let s = world.archetype_stats();
        assert!(s.component_bytes >= 100 * std::mem::size_of::<Hp>());
        assert!(s.tick_bytes >= 100 * 2 * std::mem::size_of::<Tick>()); // change + added
        assert!(s.entity_bytes >= 100 * std::mem::size_of::<Entity>());
        assert_eq!(
            s.total_bytes(),
            s.component_bytes + s.tick_bytes + s.entity_bytes
        );
    }

    #[test]
    fn untracked_component_emits_nothing() {
        let mut world = World::new();
        world.track_removals::<Hp>();
        let e = world.spawn((Armor(1),));
        world.despawn(e); // Armor не трекается

        world.flush_all_events();
        let mut reader = world.event_reader::<crate::events::Removed<Hp>>();
        assert_eq!(reader.read().iter().count(), 0);
    }
}
