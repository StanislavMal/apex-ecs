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

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;

/// Аренда свободных слотов для резервации: снимок `free_list` (индекс + поколение), переезжающий из
/// `free_list` на время между flush'ами, плюс убывающий курсор. **Слоты аренды дизъюнктны с текущим
/// `free_list`** (аренда их оттуда забрала) — поэтому `reserve` (через аренду, `&self`) и прямой
/// `allocate` (через `free_list`, `&mut`) никогда не выдадут один индекс. `cursor` убывает:
/// `n = fetch_sub(1)`; `n>0` → переиспользуем `free[n-1]`; `n≤0` → свежий индекс из `high_water`.
struct ReserveLease {
    cursor: AtomicI64,
    free: Box<[(u32, u32)]>,
}

/// Выдать один `Entity` из аренды (переиспользование) или свежий из high-water. Общий для
/// [`EntityReserver::reserve`] и фоллбэка [`EntityAllocator::allocate`].
#[inline]
fn reserve_from(lease: &ReserveLease, high_water: &AtomicU32) -> Entity {
    let n = lease.cursor.fetch_sub(1, Ordering::Relaxed);
    if n > 0 {
        let (index, generation) = lease.free[(n - 1) as usize];
        Entity { index, generation }
    } else {
        let index = high_water.fetch_add(1, Ordering::Relaxed);
        Entity { index, generation: 0 }
    }
}

/// Lock-free резерватор entity-индексов — отделён от [`EntityAllocator`], чтобы
/// [`Commands`](crate::commands::Commands) могли получить настоящий `Entity` через `&self` (без
/// `&mut World`), пока система ещё бежит (1:1 Bevy `Entities::reserve_entity`).
///
/// **Переиспользует свободные слоты** (TD-39): держит общий high-water (`Arc<AtomicU32>`) И текущую
/// аренду свободных слотов ([`ReserveLease`], `Arc`, переарендуется на каждом flush). `reserve()`
/// сперва переиспользует слоты аренды (поколение из снимка валидно — слот в аренде не трогается до
/// потребления), и лишь при исчерпании аренды двигает high-water. Так `records` не растёт на каждый
/// `cmd.spawn` (раньше резервация была монотонной ⇒ неограниченная утечка под командным churn'ом).
/// До flush entity «зарезервирована, но не жива» (`is_alive`=false) — как `commands.spawn().id()` в
/// Bevy до sync-точки.
#[derive(Clone)]
pub struct EntityReserver {
    high_water: Arc<AtomicU32>,
    lease: Arc<ReserveLease>,
}

impl EntityReserver {
    /// Зарезервировать `Entity` (переиспользует свободный слот или свежий). Lock-free, безопасно из
    /// параллельных систем (атомарный курсор аренды + high-water).
    #[inline]
    pub fn reserve(&self) -> Entity {
        reserve_from(&self.lease, &self.high_water)
    }

    /// Зарезервировать `n` `Entity` минимумом атомарных операций (один `fetch_sub` аренды + один
    /// `fetch_add` high-water на пачку) — для массового спавна
    /// ([`Commands::spawn_batch`](crate::commands::Commands::spawn_batch)).
    pub fn reserve_n(&self, n: usize) -> Vec<Entity> {
        if n == 0 {
            return Vec::new();
        }
        let old = self.lease.cursor.fetch_sub(n as i64, Ordering::Relaxed);
        let reuse = old.max(0).min(n as i64) as usize; // сколько взято из аренды
        let fresh = n - reuse;
        let mut out = Vec::with_capacity(n);
        for k in 0..reuse {
            let (index, generation) = self.lease.free[old as usize - 1 - k];
            out.push(Entity { index, generation });
        }
        if fresh > 0 {
            let start = self.high_water.fetch_add(fresh as u32, Ordering::Relaxed);
            for j in 0..fresh as u32 {
                out.push(Entity { index: start + j, generation: 0 });
            }
        }
        out
    }
}

/// Менеджер entity — generational IDs с batch API + резервацией с переиспользованием (TD-39).
pub struct EntityAllocator {
    /// Общий high-water свежих индексов (делится с резерватором и аллокатором) — двигается только
    /// когда аренда/free_list исчерпаны, поэтому `records.len()` ≈ пик одновременных entity.
    high_water: Arc<AtomicU32>,
    /// Текущая аренда свободных слотов для резервации (переарендуется на flush).
    lease: Arc<ReserveLease>,
    records: Vec<EntityRecord>,
    free_list: Vec<u32>,
}

impl EntityAllocator {
    pub fn new() -> Self {
        Self {
            high_water: Arc::new(AtomicU32::new(0)),
            lease: Arc::new(ReserveLease {
                cursor: AtomicI64::new(0),
                free: Box::new([]),
            }),
            records: Vec::new(),
            free_list: Vec::new(),
        }
    }

    /// Дескриптор-резерватор, делящий high-water И текущую аренду. Клонируется в [`Commands`] (см.
    /// [`World::entity_reserver`](crate::world::World::entity_reserver)).
    #[inline]
    pub fn reserver(&self) -> EntityReserver {
        EntityReserver {
            high_water: Arc::clone(&self.high_water),
            lease: Arc::clone(&self.lease),
        }
    }

    /// Пере-арендовать: текущий `free_list` (новые освобождения + возвращённые) → новая аренда; курсор
    /// = размер аренды. `free_list` опустошается (его слоты теперь в аренде, дизъюнктны с будущими
    /// освобождениями).
    fn refresh_lease(&mut self) {
        let free: Vec<(u32, u32)> = self
            .free_list
            .drain(..)
            .map(|i| (i, self.records[i as usize].generation))
            .collect();
        let cursor = free.len() as i64;
        self.lease = Arc::new(ReserveLease {
            cursor: AtomicI64::new(cursor),
            free: free.into_boxed_slice(),
        });
    }

    /// Сверка на границе apply (`&mut World`): (1) вернуть НЕ потреблённые слоты аренды в free_list;
    /// (2) растить `records` до high-water (свежие слоты, поколение 0); (3) пере-арендовать. После
    /// этого `spawn_reserved` проставляет компоненты/локацию зарезервированным entity.
    pub fn flush(&mut self) {
        // (2) Свежие индексы (reserve/allocate двигали high-water) — материализовать записи.
        let hw = self.high_water.load(Ordering::Relaxed) as usize;
        if self.records.len() < hw {
            self.records.resize_with(hw, || EntityRecord {
                generation: 0,
                encoded_location: NO_LOCATION,
            });
        }
        // Быстрый путь (типично: только спавны, без despawn'ов): нечего возвращать/переарендовывать.
        if self.free_list.is_empty() && self.lease.free.is_empty() {
            return;
        }
        // (1) Не потреблённые слоты аренды (free[0..cursor]) ещё свободны — вернуть в free_list.
        let cursor = self.lease.cursor.load(Ordering::Relaxed).max(0) as usize;
        let unconsumed = cursor.min(self.lease.free.len());
        for k in 0..unconsumed {
            self.free_list.push(self.lease.free[k].0);
        }
        // (3) Пере-арендовать из обновлённого free_list (новые освобождения + возвращённые).
        self.refresh_lease();
    }

    /// Убедиться, что `records` покрывает `index` (свежие промежуточные слоты — поколение 0). Нужно,
    /// т.к. резервации двигают high-water, не трогая `records`; прямой `allocate` свежего индекса
    /// может опередить flush.
    #[inline]
    pub(crate) fn ensure_record(&mut self, index: u32) {
        let needed = index as usize + 1;
        if self.records.len() < needed {
            self.records.resize_with(needed, || EntityRecord {
                generation: 0,
                encoded_location: NO_LOCATION,
            });
        }
    }

    /// Выделить одну entity. Сперва переиспользует НОВОЕ освобождение (`free_list`, немедленно), иначе
    /// слот аренды или свежий (через общий курсор/high-water — координирован с резерватором; слоты
    /// аренды и free_list дизъюнктны ⇒ коллизии нет).
    pub fn allocate(&mut self) -> Entity {
        if let Some(index) = self.free_list.pop() {
            let generation = self.records[index as usize].generation;
            Entity { index, generation }
        } else {
            let e = reserve_from(&self.lease, &self.high_water);
            self.ensure_record(e.index);
            e
        }
    }

    /// Выделить N entity за один проход — batch API.
    ///
    /// **Перф (TD-39 регресс-фикс):** атомарные операции и рост `records` БАТЧАТСЯ на всю пачку
    /// (один `cursor.fetch_sub(N)` аренды + один `high_water.fetch_add(N)` + один `resize_with`),
    /// а не per-entity `reserve_from`/`ensure_record`. Семантика идентична (общий курсор/high-water
    /// делятся с резерватором; порядок потребления аренды — тот же descending, что у `reserve_n`),
    /// но 2 атомика + 1 resize на пачку вместо 2×N атомиков + N resize'ов. На `simple_insert`
    /// (10k) это снимало ~150µs «налога» от 20 000 lock-инструкций.
    pub fn allocate_batch(&mut self, count: usize) -> Vec<Entity> {
        let mut entities = Vec::with_capacity(count);
        // 1. Дренируем новые освобождения (free_list) — немедленно доступны.
        let from_free = count.min(self.free_list.len());
        for _ in 0..from_free {
            let index = self.free_list.pop().unwrap();
            let generation = self.records[index as usize].generation;
            entities.push(Entity { index, generation });
        }
        let remaining = count - from_free;
        if remaining == 0 {
            return entities;
        }
        // 2. Остаток из аренды — ОДИН fetch_sub (как reserve_n): consume free[old-1..old-reuse].
        let old = self.lease.cursor.fetch_sub(remaining as i64, Ordering::Relaxed);
        let reuse = old.max(0).min(remaining as i64) as usize;
        for k in 0..reuse {
            let (index, generation) = self.lease.free[old as usize - 1 - k];
            entities.push(Entity { index, generation });
        }
        // 3. Свежие индексы — ОДИН fetch_add high-water + ОДИН resize records.
        let fresh = remaining - reuse;
        if fresh > 0 {
            let start = self.high_water.fetch_add(fresh as u32, Ordering::Relaxed);
            let end = start as usize + fresh;
            if self.records.len() < end {
                self.records.resize_with(end, || EntityRecord {
                    generation: 0,
                    encoded_location: NO_LOCATION,
                });
            }
            for j in 0..fresh as u32 {
                entities.push(Entity {
                    index: start + j,
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
        // Свободны: free_list + НЕ потреблённые слоты аренды (free[0..cursor]).
        let lease_free = (self.lease.cursor.load(Ordering::Relaxed).max(0) as usize)
            .min(self.lease.free.len());
        self.records.len() - self.free_list.len() - lease_free
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

    // ── Атомарная резервация (Commands::spawn().id()) ──────────

    #[test]
    fn reserve_produces_fresh_gen0_ids_then_flush_materializes() {
        let mut alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let a = r.reserve();
        let b = r.reserve();
        assert_ne!(a.index, b.index, "резервации дают разные индексы");
        assert_eq!(a.generation, 0);
        assert_eq!(b.generation, 0);
        // До flush слоты не существуют → entity не жива.
        assert!(!alloc.is_alive(a));

        // flush создаёт записи; spawn_reserved-эквивалент = set_location.
        alloc.flush();
        alloc.set_location(a, make_loc());
        alloc.set_location(b, make_loc());
        assert!(alloc.is_alive(a));
        assert!(alloc.is_alive(b));
    }

    #[test]
    fn reserve_and_direct_allocate_never_collide() {
        let mut alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let mut indices = std::collections::HashSet::new();
        // Перемежаем резервацию и прямой allocate — общий high-water не даёт коллизий индексов.
        for _ in 0..50 {
            assert!(indices.insert(r.reserve().index));
            let e = alloc.allocate();
            alloc.set_location(e, make_loc());
            assert!(indices.insert(e.index), "прямой allocate не пересекает резервации");
        }
        alloc.flush();
    }

    #[test]
    fn reservation_reuses_freed_slots_after_flush() {
        let mut alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let a = r.reserve();
        let b = r.reserve();
        let c = r.reserve();
        alloc.flush();
        for e in [a, b, c] {
            alloc.set_location(e, make_loc());
        }
        let records_after_3 = alloc.records.len();
        assert_eq!(records_after_3, 3);

        // Освобождаем b, переарендуем — слот b попадает в аренду.
        alloc.free(b);
        alloc.flush();

        // Следующая резервация ПЕРЕИСПОЛЬЗУЕТ слот b (а не свежий индекс) ⇒ records НЕ растёт.
        let r2 = alloc.reserver();
        let d = r2.reserve();
        assert_eq!(d.index, b.index, "резервация переиспользовала освобождённый слот");
        assert_ne!(d.generation, b.generation, "новое поколение слота");
        alloc.flush();
        alloc.set_location(d, make_loc());
        assert!(alloc.is_alive(d));
        assert!(!alloc.is_alive(b), "старый хэндл b мёртв (ABA-safe)");
        assert_eq!(
            alloc.records.len(),
            records_after_3,
            "records НЕ вырос — резервация переиспользовала слот, а не аллоцировала свежий"
        );
    }

    /// Главный тест TD-39: устойчивый churn через РЕЗЕРВАЦИЮ (как `cmd.spawn`) НЕ растит `records`
    /// безгранично — слоты переиспользуются. Раньше (монотонная резервация) records = суммарно-всех-
    /// спавнов (неограниченная утечка). 500 «кадров» рефилла-до-пика + despawn половины.
    #[test]
    fn reservation_churn_keeps_records_bounded() {
        let mut alloc = EntityAllocator::new();
        let peak = 200usize;
        let mut live: Vec<Entity> = Vec::new();
        for frame in 0..500 {
            // Рефилл до пика РЕЗЕРВАЦИЕЙ (переиспользует слоты прошлого кадра).
            let r = alloc.reserver();
            while live.len() < peak {
                live.push(r.reserve());
            }
            alloc.flush();
            for &e in &live {
                if alloc.get_location(e).is_none() {
                    alloc.set_location(e, make_loc());
                }
            }
            // Despawn половины (детерминированно), переарендовать.
            let mut kept = Vec::new();
            for (i, &e) in live.iter().enumerate() {
                if i % 2 == frame % 2 {
                    assert!(alloc.free(e), "free живой entity");
                } else {
                    kept.push(e);
                }
            }
            live = kept;
            alloc.flush();
            for &e in &live {
                assert!(alloc.is_alive(e), "выжившая entity осталась живой с верным поколением");
            }
        }
        // records ≈ пик, а НЕ ~кадры×рефилл (≈50k у старой утечки) — доказательство фикса.
        assert!(
            alloc.records.len() <= peak * 2,
            "records ограничен пиком ({peak}) — утечка устранена; got {}",
            alloc.records.len()
        );
    }

    #[test]
    fn parallel_reserve_yields_unique_indices() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let count = AtomicU32::new(0);
        // 8 потоков по 1000 резерваций — все индексы уникальны (lock-free fetch_add).
        let all: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..8 {
                let r = r.clone();
                let all = &all;
                let count = &count;
                s.spawn(move || {
                    let mut local = Vec::with_capacity(1000);
                    for _ in 0..1000 {
                        local.push(r.reserve().index);
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    all.lock().unwrap().extend(local);
                });
            }
        });
        assert_eq!(count.load(Ordering::Relaxed), 8000);
        let mut v = all.into_inner().unwrap();
        v.sort_unstable();
        v.dedup();
        assert_eq!(v.len(), 8000, "ни один индекс не выдан дважды");
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
