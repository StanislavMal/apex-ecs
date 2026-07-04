/// Entity — generational index.
///
/// `Serialize`/`Deserialize` so components holding `Entity` refs can be
/// snapshotted; on restore those refs are remapped via [`MapEntities`](crate::MapEntities) (E6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
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
use std::sync::{Arc, RwLock};

/// Аренда свободных слотов для резервации: снимок `free_list` (индекс + поколение), переезжающий из
/// `free_list` на время между flush'ами, плюс убывающий курсор. **Слоты аренды дизъюнктны с текущим
/// `free_list`** (аренда их оттуда забрала) — поэтому `reserve` (через аренду, `&self`) и прямой
/// `allocate` (через `free_list`, `&mut`) никогда не выдадут один индекс. `cursor` убывает:
/// `n = fetch_sub(1)`; `n>0` → переиспользуем `free[n-1]`; `n≤0` → свежий индекс из `high_water`.
struct ReserveLease {
    cursor: AtomicI64,
    free: Box<[(u32, u32)]>,
}

/// Разделяемая ячейка текущей аренды. `EntityAllocator` и ВСЕ `EntityReserver`
/// держат клон ОДНОГО `Arc<LeaseCell>`, поэтому `flush`/`refresh_lease`
/// публикуют новую аренду через `write()`, а резерваторы всегда читают ТЕКУЩУЮ
/// через `read()` — устаревший снимок аренды невозможен (B2: раньше резерватор
/// держал снятый `Arc<ReserveLease>`, а flush подменял свой ⇒ старый резерватор
/// выдавал слоты, уже вернувшиеся в free_list и переарендованные = двойная
/// выдача одного индекса). Конкурентные резерваторы берут read-lock (параллельны
/// между собой), flush — write-lock (эксклюзив, на sync-точке apply, где систем
/// не бежит).
type LeaseCell = Arc<RwLock<Arc<ReserveLease>>>;

#[inline]
fn read_lease(cell: &LeaseCell) -> Arc<ReserveLease> {
    // Клон под кратким read-lock; курсорные операции идут по клону (общий
    // атомарный `cursor` — та же аренда, что видят другие резерваторы).
    cell.read().unwrap_or_else(|e| e.into_inner()).clone()
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
/// D8b: private deterministic id-block cursor. A per-system reserver hands out
/// `base, base+1, …` from `next` until `end`, with NO cross-system contention and a
/// deterministic order (the block base is assigned by the scheduler in system-rank
/// order). `next` is atomic only so the reserver stays `Send + Sync`; spawning
/// systems are never row-split (`Commands` ⇒ `non_query_side_effects` ⇒ single ASD
/// task), so in practice one thread drives one block.
struct BlockCursor {
    next: AtomicU32,
    end: u32,
}

#[derive(Clone)]
pub struct EntityReserver {
    high_water: Arc<AtomicU32>,
    lease: LeaseCell,
    /// D8b: optional deterministic block. When present, `reserve()` draws from the
    /// private block (deterministic, contention-free) until exhausted, then falls
    /// back to the shared high-water/lease.
    block: Option<Arc<BlockCursor>>,
}

impl EntityReserver {
    /// Зарезервировать `Entity` (переиспользует свободный слот или свежий). Безопасно из
    /// параллельных систем: read-lock ячейки аренды (параллелен между резерваторами) → атомарный
    /// курсор текущей аренды + high-water. Всегда видит АКТУАЛЬНУЮ аренду (B2).
    ///
    /// D8b: если резерватор засеян блоком (`with_block`), сперва выдаёт id из приватного
    /// блока (детерминированно, без contention); при исчерпании блока — общий путь.
    #[inline]
    pub fn reserve(&self) -> Entity {
        if let Some(block) = &self.block {
            let id = block.next.fetch_add(1, Ordering::Relaxed);
            if id < block.end {
                return Entity { index: id, generation: 0 };
            }
            // Block exhausted — fall through to the shared reserver. The rank-ordered
            // overflow claim (fully deterministic overflow) is a follow-up (§4.1).
        }
        let lease = read_lease(&self.lease);
        reserve_from(&lease, &self.high_water)
    }

    /// D8b: клон резерватора, привязанный к детерминированному блоку `[base, base+size)`.
    /// `reserve()` выдаёт `base, base+1, …` приватным счётчиком (без contention,
    /// детерминированно), после исчерпания — общий путь. Вызывающий (планировщик)
    /// гарантирует, что `[base, base+size)` уже зарезервирован из high-water
    /// ([`EntityAllocator::reserve_block`]).
    pub fn with_block(&self, base: u32, size: u32) -> EntityReserver {
        EntityReserver {
            high_water: Arc::clone(&self.high_water),
            lease: Arc::clone(&self.lease),
            block: Some(Arc::new(BlockCursor {
                next: AtomicU32::new(base),
                end: base.saturating_add(size),
            })),
        }
    }

    /// D8b: сколько id ещё осталось в блоке (для адаптивного размера/переклейма).
    /// `None` — резерватор без блока.
    pub fn block_remaining(&self) -> Option<u32> {
        self.block
            .as_ref()
            .map(|b| b.end.saturating_sub(b.next.load(Ordering::Relaxed)))
    }

    /// Зарезервировать `n` `Entity` минимумом атомарных операций (один `fetch_sub` аренды + один
    /// `fetch_add` high-water на пачку) — для массового спавна
    /// ([`Commands::spawn_batch`](crate::commands::Commands::spawn_batch)).
    pub fn reserve_n(&self, n: usize) -> Vec<Entity> {
        if n == 0 {
            return Vec::new();
        }
        // D8b: block mode — draw `n` contiguously from the private block (deterministic,
        // no contention). Single-task per spawning system, so plain load+store is sound.
        // On block overflow, fall through to the shared path (rare; rank-ordered overflow
        // determinism is a follow-up, §4.1).
        if let Some(block) = &self.block {
            let cur = block.next.load(Ordering::Relaxed);
            if cur.saturating_add(n as u32) <= block.end {
                block.next.store(cur + n as u32, Ordering::Relaxed);
                return (0..n as u32)
                    .map(|j| Entity { index: cur + j, generation: 0 })
                    .collect();
            }
        }
        let lease = read_lease(&self.lease);
        let old = lease.cursor.fetch_sub(n as i64, Ordering::Relaxed);
        let reuse = old.max(0).min(n as i64) as usize; // сколько взято из аренды
        let fresh = n - reuse;
        let mut out = Vec::with_capacity(n);
        for k in 0..reuse {
            let (index, generation) = lease.free[old as usize - 1 - k];
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
    /// Разделяемая ячейка текущей аренды (переарендуется на flush; см. [`LeaseCell`]).
    /// Общая с ВСЕМИ [`EntityReserver`] — публикация новой аренды через неё
    /// исключает устаревший снимок (B2).
    lease: LeaseCell,
    records: Vec<EntityRecord>,
    free_list: Vec<u32>,
    /// Число **живых** (located) записей — поддерживается O(1) на переходах локации (как `Entities::len`
    /// в Bevy). [`len`](Self::len) возвращает его напрямую: это и дешевле формулы `records − free − lease`,
    /// и КОНСИСТЕНТНО с [`is_alive`](Self::is_alive) — резервации, материализованные `flush`'ем как
    /// location-less записи (например осиротевшие при дропе `Commands` без `apply`), сюда НЕ попадают
    /// (раньше они завышали счёт — TD-40).
    live: u32,
}

impl EntityAllocator {
    pub fn new() -> Self {
        Self {
            high_water: Arc::new(AtomicU32::new(0)),
            lease: Arc::new(RwLock::new(Arc::new(ReserveLease {
                cursor: AtomicI64::new(0),
                free: Box::new([]),
            }))),
            records: Vec::new(),
            free_list: Vec::new(),
            live: 0,
        }
    }

    /// Дескриптор-резерватор, делящий high-water И текущую аренду. Клонируется в [`Commands`] (см.
    /// [`World::entity_reserver`](crate::world::World::entity_reserver)).
    #[inline]
    pub fn reserver(&self) -> EntityReserver {
        EntityReserver {
            high_water: Arc::clone(&self.high_water),
            // Клон Arc на ТУ ЖЕ ячейку — резерватор видит все будущие переаренды.
            lease: Arc::clone(&self.lease),
            block: None,
        }
    }

    /// D8b: зарезервировать непрерывный блок `[base, base+size)` из high-water и вернуть
    /// резерватор, привязанный к нему. `base` детерминирован (текущий high-water,
    /// главный поток), поэтому раздача блоков системам стадии в порядке ранга даёт
    /// детерминированные id. Неиспользованный хвост блока материализуется `flush`'ем
    /// как location-less записи (как обычная зарезервированная-но-не-живая) —
    /// возвращать их в free-list в порядке ранга — задача планировщика (§4.1).
    pub fn reserve_block(&self, size: u32) -> EntityReserver {
        let base = self.high_water.fetch_add(size, Ordering::Relaxed);
        self.reserver().with_block(base, size)
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
        // Публикуем новую аренду в РАЗДЕЛЯЕМУЮ ячейку — все резерваторы увидят её
        // при следующем `reserve` (B2: старый снимок больше не выдаётся).
        *self.lease.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(ReserveLease {
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
        // Читаем текущую аренду из ячейки (клон — read-lock отпускается сразу, до
        // write-lock в refresh_lease, иначе self-deadlock).
        let lease = read_lease(&self.lease);
        // Быстрый путь (типично: только спавны, без despawn'ов): нечего возвращать/переарендовывать.
        if self.free_list.is_empty() && lease.free.is_empty() {
            return;
        }
        // (1) Не потреблённые слоты аренды (free[0..cursor]) ещё свободны — вернуть в free_list.
        let cursor = lease.cursor.load(Ordering::Relaxed).max(0) as usize;
        let unconsumed = cursor.min(lease.free.len());
        for k in 0..unconsumed {
            self.free_list.push(lease.free[k].0);
        }
        drop(lease);
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
            let lease = read_lease(&self.lease);
            let e = reserve_from(&lease, &self.high_water);
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
        let lease = read_lease(&self.lease);
        let old = lease.cursor.fetch_sub(remaining as i64, Ordering::Relaxed);
        let reuse = old.max(0).min(remaining as i64) as usize;
        for k in 0..reuse {
            let (index, generation) = lease.free[old as usize - 1 - k];
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
            let became_live = !record.has_location(); // location-less → located ⇒ +1 живой
            record.set_location(EntityLocation {
                archetype_id,
                row: start_row + i as u32,
            });
            if became_live {
                self.live += 1;
            }
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
        let was_live = record.has_location(); // located → location-less ⇒ −1 живой
        record.generation = record.generation.wrapping_add(1);
        record.clear_location();
        if was_live {
            self.live -= 1;
        }
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
        let mut became_live = false;
        if let Some(record) = self.records.get_mut(entity.index as usize) {
            if record.generation == entity.generation {
                became_live = !record.has_location(); // location-less → located ⇒ +1 живой
                record.set_location(location);
            }
        }
        if became_live {
            self.live += 1;
        }
    }

    /// Число **живых** (located) entity — консистентно с [`is_alive`](Self::is_alive). O(1): возвращает
    /// поддерживаемый счётчик `live` (не считает зарезервированные-но-не-материализованные записи,
    /// которые `flush` создаёт location-less — TD-40). Дешевле прежней формулы `records − free − lease`.
    pub fn len(&self) -> usize {
        self.live as usize
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

    /// TD-40 регресс: `len()` (= `entity_count`) считает только ЖИВЫЕ (located) entity и КОНСИСТЕНТЕН с
    /// `is_alive` — зарезервированные-но-не-материализованные записи (осиротевшие, как при дропе
    /// `Commands` без `apply`) НЕ завышают счёт, даже после того как `flush` создаст их как location-less.
    #[test]
    fn len_counts_only_located_ignoring_orphaned_reservations() {
        let mut alloc = EntityAllocator::new();
        // Две настоящие entity.
        let a = alloc.allocate();
        alloc.set_location(a, make_loc());
        let b = alloc.allocate();
        alloc.set_location(b, make_loc());
        assert_eq!(alloc.len(), 2);

        // Три резервации, которые НИКОГДА не материализуются (как дропнутый Commands-буфер): двигают
        // общий high-water, но локацию им никто не проставит.
        let r = alloc.reserver();
        let orphans = [r.reserve(), r.reserve(), r.reserve()];
        // flush растит `records` до high-water → осиротевшие становятся location-less записями.
        alloc.flush();

        assert_eq!(alloc.len(), 2, "осиротевшие резервации не должны завышать entity_count (TD-40)");
        for o in orphans {
            assert!(!alloc.is_alive(o), "осиротевшая резервация не жива");
        }
        // Despawn реальной entity → счёт уменьшается; свободный слот тоже не считается.
        assert!(alloc.free(a));
        assert_eq!(alloc.len(), 1, "len консистентен с is_alive после despawn");
        assert!(!alloc.is_alive(a) && alloc.is_alive(b));
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

    /// B2: резерватор, ПЕРЕЖИВШИЙ flush, обязан выдавать из ТЕКУЩЕЙ аренды, а не
    /// из устаревшего снимка. Раньше он держал снятый `Arc<ReserveLease>`; после
    /// flush его непотреблённые слоты возвращались в free_list и переарендовывались,
    /// а старый резерватор всё ещё выдавал их — те же индексы уходили и через
    /// прямой `allocate` (двойная выдача одного слота).
    #[test]
    fn reserver_surviving_flush_never_double_issues() {
        let mut alloc = EntityAllocator::new();
        // Наполняем free_list шестью слотами (allocate → материализация → free).
        let first: Vec<Entity> = (0..6).map(|_| alloc.allocate()).collect();
        alloc.flush();
        for &e in &first {
            alloc.set_location(e, make_loc());
        }
        for &e in &first {
            assert!(alloc.free(e));
        }
        alloc.flush(); // аренда L1 получила 6 слотов, free_list пуст

        // Резерватор захвачен ОДИН раз и переживает следующий flush.
        let r = alloc.reserver();
        let x0 = r.reserve(); // частично потребляем L1
        let x1 = r.reserve();
        alloc.flush(); // L1's непотреблённые слоты → free_list → новая аренда L2

        // Все выданные индексы обязаны быть уникальны: старый резерватор `r`
        // (через ту же ячейку теперь видит L2) и прямой allocate делят один курсор.
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(x0.index));
        assert!(seen.insert(x1.index));
        for _ in 0..4 {
            let a = r.reserve();
            assert!(seen.insert(a.index), "резерватор повторно выдал индекс {}", a.index);
            let b = alloc.allocate();
            alloc.set_location(b, make_loc());
            assert!(seen.insert(b.index), "allocate столкнулся с резерватором на {}", b.index);
        }
        alloc.flush();
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

    // ── D8b: deterministic id-block reserver ────────────────────────────────
    #[test]
    fn block_reserver_hands_out_contiguous_deterministic_ids() {
        let alloc = EntityAllocator::new();
        let r = alloc.reserve_block(5);
        let ids: Vec<u32> = (0..5).map(|_| r.reserve().index).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4], "block yields base+0..size in order");
        assert_eq!(r.block_remaining(), Some(0));
    }

    #[test]
    fn block_reserver_overflow_falls_back_to_shared() {
        let alloc = EntityAllocator::new();
        let r = alloc.reserve_block(2); // reserves [0,2) from high_water; hw now 2
        assert_eq!(r.reserve().index, 0);
        assert_eq!(r.reserve().index, 1);
        // block exhausted → shared path draws the next fresh index (2).
        assert_eq!(r.reserve().index, 2);
    }

    #[test]
    fn block_reserver_n_is_contiguous() {
        let alloc = EntityAllocator::new();
        let r = alloc.reserve_block(10);
        let batch = r.reserve_n(4);
        assert_eq!(
            batch.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(r.reserve().index, 4);
    }

    #[test]
    fn per_system_blocks_are_deterministic_across_runs() {
        // Model a stage: systems in rank order get blocks; each spawns some ids.
        // Two independent runs must produce IDENTICAL id assignment (D8b capstone
        // shape at the reserver level — the scheduler seeds blocks in rank order).
        fn run(sizes: &[u32], spawns: &[u32]) -> Vec<Vec<u32>> {
            let alloc = EntityAllocator::new();
            let reservers: Vec<EntityReserver> =
                sizes.iter().map(|&s| alloc.reserve_block(s)).collect();
            // Spawns happen "in parallel" but each system drives its own block, so
            // the id set per system is a pure function of (rank, spawn index).
            reservers
                .iter()
                .zip(spawns)
                .map(|(r, &n)| (0..n).map(|_| r.reserve().index).collect())
                .collect()
        }
        let sizes = [4u32, 4, 4];
        let spawns = [3u32, 2, 4];
        let a = run(&sizes, &spawns);
        let b = run(&sizes, &spawns);
        assert_eq!(a, b, "per-system block id assignment is deterministic");
        // System 0 (rank 0, block base 0) → 0,1,2; system 1 (base 4) → 4,5;
        // system 2 (base 8) → 8,9,10,11.
        assert_eq!(a, vec![vec![0, 1, 2], vec![4, 5], vec![8, 9, 10, 11]]);
    }
}
