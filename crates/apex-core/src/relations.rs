//! Relations — связи между entity.
//!
//! # Модель (CR-M1, 2026-06-11)
//!
//! Пара `(kind, target)` НЕ является компонентом и не входит в идентичность
//! архетипа. Истина о связях живёт в двух индексах мира:
//!
//! - [`SubjectIndex`]: `entity.index` → набор [`RelationPair`] (kind + target
//!   **целиком**, с generation) — отвечает за `has_relation`,
//!   `get_relation_target`, сериализацию;
//! - [`TargetIndex`]: `(kind, target.index)` → subjects — отвечает за
//!   `children_of` (O(детей)), `query_relation`/`query_wildcard` и
//!   **cascade delete при `despawn(target)`**.
//!
//! Следствия:
//! - `add_relation` = две индекс-вставки, БЕЗ структурного изменения
//!   (нет archetype move, нет нового архетипа, нет инвалидации QueryCache);
//! - `despawn(target)` вычищает все связи, где entity — target; для kind'ов с
//!   `cascade_delete_on_target_despawn()` subjects деспавнятся каскадом;
//! - generation-честность: в индексах хранится `Entity` целиком, поэтому
//!   переиспользование `entity.index` новым поколением не возвращает чужие
//!   связи; лимитов кодирования (2^20 на index, 2^11 на kind) больше нет.
//!
//! Историческая модель «пара кодируется в ComponentId и входит в состав
//! архетипа» фрагментировала мир на архетип-на-родителя (many_foxes @1000 —
//! 22k архетипов) и удалена; см. apex-engine/plans/CORE_REFACTORING.md (C-1..C-3).

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::any::TypeId;

use crate::{
    component::Tick,
    entity::Entity,
    query::WorldQuery,
    world::World,
};

// ── RelationPair ───────────────────────────────────────────────

/// Одна связь субъекта: вид + target целиком (index + generation).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RelationPair {
    pub kind_idx: u32,
    pub target: Entity,
}

impl RelationPair {
    #[inline]
    fn sort_key(&self) -> (u32, u32, u32) {
        (self.kind_idx, self.target.index(), self.target.generation())
    }
}

impl PartialOrd for RelationPair {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RelationPair {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

/// Порог переключения от Sparse к Dense хранению отношений.
/// При количестве отношений > DENSE_THRESHOLD на сущность,
/// хранилище переключается на HashSet для O(1) операций.
const DENSE_THRESHOLD: usize = 8;

/// Upper bound on the ancestor walk used for cycle detection in `add_relation`.
/// A well-formed hierarchy is far shallower; exceeding it means a pre-existing
/// cycle (defensive) and the edge is rejected. Bounds the cost against a
/// pathological chain while never limiting any real scene graph.
const MAX_ANCESTOR_WALK: usize = 1 << 20;

/// Хранилище отношений для одной сущности.
///
/// - `Sparse`: отсортированный SmallVec для малого числа отношений (≤8).
///   Использует binary_search — O(log n).
/// - `Dense`: FxHashSet для большого числа отношений (>8).
///   Все операции O(1) amortized.
enum PairStorage {
    Sparse(SmallVec<[RelationPair; 4]>),
    Dense(rustc_hash::FxHashSet<RelationPair>),
}

impl Default for PairStorage {
    fn default() -> Self {
        Self::Sparse(SmallVec::new())
    }
}

impl PairStorage {
    /// true, если пары не было (вставлена впервые).
    #[inline]
    fn insert(&mut self, pair: RelationPair) -> bool {
        match self {
            Self::Sparse(sv) => {
                let pos = match sv.binary_search(&pair) {
                    Ok(_) => return false, // уже существует
                    Err(pos) => pos,
                };
                sv.insert(pos, pair);
                // Auto-upgrade: при превышении порога переключаемся на Dense
                if sv.len() > DENSE_THRESHOLD {
                    let set: rustc_hash::FxHashSet<RelationPair> = sv.drain(..).collect();
                    *self = Self::Dense(set);
                }
                true
            }
            Self::Dense(set) => set.insert(pair),
        }
    }

    #[inline]
    fn remove(&mut self, pair: RelationPair) -> bool {
        match self {
            Self::Sparse(sv) => {
                if let Ok(pos) = sv.binary_search(&pair) {
                    sv.remove(pos);
                    true
                } else {
                    false
                }
            }
            Self::Dense(set) => set.remove(&pair),
        }
    }

    #[inline]
    fn contains(&self, pair: RelationPair) -> bool {
        match self {
            Self::Sparse(sv) => sv.binary_search(&pair).is_ok(),
            Self::Dense(set) => set.contains(&pair),
        }
    }

    #[inline]
    fn contains_kind(&self, kind_idx: u32) -> bool {
        match self {
            Self::Sparse(sv) => sv.iter().any(|p| p.kind_idx == kind_idx),
            Self::Dense(set) => set.iter().any(|p| p.kind_idx == kind_idx),
        }
    }

    /// Первая пара заданного вида (для Sparse — с минимальным target).
    #[inline]
    fn first_with_kind(&self, kind_idx: u32) -> Option<RelationPair> {
        match self {
            Self::Sparse(sv) => sv.iter().find(|p| p.kind_idx == kind_idx).copied(),
            Self::Dense(set) => set.iter().find(|p| p.kind_idx == kind_idx).copied(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Sparse(sv) => sv.is_empty(),
            Self::Dense(set) => set.is_empty(),
        }
    }

    fn iter(&self) -> Box<dyn Iterator<Item = RelationPair> + '_> {
        match self {
            Self::Sparse(sv) => Box::new(sv.iter().copied()),
            Self::Dense(set) => Box::new(set.iter().copied()),
        }
    }

    /// Забрать все пары, оставив хранилище пустым (без аллокаций при Sparse≤4).
    fn take_all(&mut self) -> SmallVec<[RelationPair; 4]> {
        match self {
            Self::Sparse(sv) => std::mem::take(sv),
            Self::Dense(set) => {
                let out: SmallVec<[RelationPair; 4]> = set.iter().copied().collect();
                set.clear();
                out
            }
        }
    }
}

// ── SubjectIndex ───────────────────────────────────────────────

#[derive(Default)]
struct SubjectEntry {
    /// Битовая маска: бит k установлен ↔ есть relation с kind_idx = k.
    kind_mask: u64,
    /// Хранилище пар (Sparse для ≤8, Dense для >8).
    storage: PairStorage,
}

impl SubjectEntry {
    #[inline]
    fn has_kind(&self, kind_idx: u32) -> bool {
        if kind_idx >= 64 {
            return self.storage.contains_kind(kind_idx);
        }
        self.kind_mask & (1u64 << kind_idx) != 0
    }

    #[inline]
    fn insert(&mut self, pair: RelationPair) -> bool {
        let inserted = self.storage.insert(pair);
        if inserted && pair.kind_idx < 64 {
            self.kind_mask |= 1u64 << pair.kind_idx;
        }
        inserted
    }

    #[inline]
    fn remove(&mut self, pair: RelationPair) -> bool {
        let existed = self.storage.remove(pair);
        if existed && pair.kind_idx < 64 {
            // Проверяем, остались ли ещё отношения этого вида
            if !self.storage.contains_kind(pair.kind_idx) {
                self.kind_mask &= !(1u64 << pair.kind_idx);
            }
        }
        existed
    }

    #[inline]
    fn has(&self, pair: RelationPair) -> bool {
        if !self.has_kind(pair.kind_idx) {
            return false;
        }
        self.storage.contains(pair)
    }
}

pub(crate) struct SubjectIndex {
    entries: Vec<SubjectEntry>,
}

impl SubjectIndex {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    #[inline]
    fn ensure(&mut self, entity_index: usize) {
        if entity_index >= self.entries.len() {
            self.entries
                .resize_with(entity_index + 1, SubjectEntry::default);
        }
    }

    /// true, если пары не было (вставлена впервые).
    #[inline]
    pub fn insert(&mut self, entity_index: u32, pair: RelationPair) -> bool {
        let idx = entity_index as usize;
        self.ensure(idx);
        self.entries[idx].insert(pair)
    }

    #[inline]
    pub fn remove(&mut self, entity_index: u32, pair: RelationPair) -> bool {
        let idx = entity_index as usize;
        if idx < self.entries.len() {
            self.entries[idx].remove(pair)
        } else {
            false
        }
    }

    #[inline]
    pub fn has(&self, entity_index: u32, pair: RelationPair) -> bool {
        let idx = entity_index as usize;
        idx < self.entries.len() && self.entries[idx].has(pair)
    }

    #[inline]
    pub fn first_with_kind(&self, entity_index: u32, kind_idx: u32) -> Option<RelationPair> {
        let idx = entity_index as usize;
        let entry = self.entries.get(idx)?;
        if !entry.has_kind(kind_idx) {
            return None;
        }
        entry.storage.first_with_kind(kind_idx)
    }

    /// Забрать все пары entity (для despawn).
    #[inline]
    pub fn take_all(&mut self, entity_index: u32) -> SmallVec<[RelationPair; 4]> {
        let idx = entity_index as usize;
        if idx < self.entries.len() && !self.entries[idx].storage.is_empty() {
            self.entries[idx].kind_mask = 0;
            self.entries[idx].storage.take_all()
        } else {
            SmallVec::new()
        }
    }

    /// Итерация всех (subject_index, pair) — для сериализации.
    pub fn iter_all(&self) -> impl Iterator<Item = (u32, RelationPair)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.storage.is_empty())
            .flat_map(|(i, e)| e.storage.iter().map(move |p| (i as u32, p)))
    }
}

impl Default for SubjectIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ── TargetIndex ────────────────────────────────────────────────

/// Обратный индекс: `(kind, target.index)` → subjects.
///
/// Ключ — голый `target.index`: записи вычищаются при despawn target'а,
/// поэтому живая запись всегда принадлежит ТЕКУЩЕМУ поколению индекса
/// (generation-честность обеспечивается чисткой, subjects хранятся целиком).
pub(crate) struct TargetIndex {
    /// kind_idx → (target.index → subjects в порядке вставки).
    by_kind: Vec<FxHashMap<u32, SmallVec<[Entity; 4]>>>,
    /// Число пар, где `entity.index` — target (fast-skip в despawn).
    target_counts: Vec<u32>,
}

impl TargetIndex {
    pub fn new() -> Self {
        Self {
            by_kind: Vec::new(),
            target_counts: Vec::new(),
        }
    }

    #[inline]
    fn ensure_kind(&mut self, kind_idx: u32) -> &mut FxHashMap<u32, SmallVec<[Entity; 4]>> {
        let k = kind_idx as usize;
        if k >= self.by_kind.len() {
            self.by_kind.resize_with(k + 1, FxHashMap::default);
        }
        &mut self.by_kind[k]
    }

    /// Whether `entity_index` is currently the target of ANY relation (any kind).
    /// Cheap O(1) read used to short-circuit cycle detection: an entity that is not
    /// a target of anything cannot appear in any ancestor chain, so no edge into it
    /// can close a cycle.
    #[inline]
    pub fn is_any_target(&self, entity_index: u32) -> bool {
        self.target_counts
            .get(entity_index as usize)
            .copied()
            .unwrap_or(0)
            > 0
    }

    #[inline]
    pub fn add(&mut self, kind_idx: u32, target: Entity, subject: Entity) {
        self.ensure_kind(kind_idx)
            .entry(target.index())
            .or_default()
            .push(subject);
        let ti = target.index() as usize;
        if ti >= self.target_counts.len() {
            self.target_counts.resize(ti + 1, 0);
        }
        self.target_counts[ti] += 1;
    }

    #[inline]
    pub fn remove(&mut self, kind_idx: u32, target_index: u32, subject: Entity) -> bool {
        let Some(map) = self.by_kind.get_mut(kind_idx as usize) else {
            return false;
        };
        let Some(subjects) = map.get_mut(&target_index) else {
            return false;
        };
        let Some(pos) = subjects.iter().position(|&s| s == subject) else {
            return false;
        };
        subjects.swap_remove(pos);
        if subjects.is_empty() {
            map.remove(&target_index);
        }
        self.target_counts[target_index as usize] -= 1;
        true
    }

    /// Забрать всех subjects записи `(kind, target.index)` (для despawn).
    #[inline]
    pub fn take_subjects(
        &mut self,
        kind_idx: u32,
        target_index: u32,
    ) -> Option<SmallVec<[Entity; 4]>> {
        let map = self.by_kind.get_mut(kind_idx as usize)?;
        let subjects = map.remove(&target_index)?;
        self.target_counts[target_index as usize] -= subjects.len() as u32;
        Some(subjects)
    }

    #[inline]
    pub fn subjects(&self, kind_idx: u32, target_index: u32) -> &[Entity] {
        self.by_kind
            .get(kind_idx as usize)
            .and_then(|m| m.get(&target_index))
            .map(|sv| sv.as_slice())
            .unwrap_or(&[])
    }

    /// Все subjects вида (wildcard) — по одному вхождению на пару.
    pub fn all_subjects(&self, kind_idx: u32) -> impl Iterator<Item = Entity> + '_ {
        self.by_kind
            .get(kind_idx as usize)
            .into_iter()
            .flat_map(|m| m.values().flat_map(|sv| sv.iter().copied()))
    }

    /// Есть ли хоть одна связь, где данный index — target.
    #[inline]
    pub fn has_target(&self, entity_index: u32) -> bool {
        self.target_counts
            .get(entity_index as usize)
            .map(|&c| c > 0)
            .unwrap_or(false)
    }
}

impl Default for TargetIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ── RelationKind ───────────────────────────────────────────────

pub trait RelationKind: Copy + Send + Sync + 'static {
    /// При `despawn(target)` subjects этого вида деспавнятся каскадом.
    /// Для остальных видов связь просто вычищается из индексов
    /// (ни одна связь не переживает свой target).
    fn cascade_delete_on_target_despawn() -> bool {
        false
    }

    /// Exclusive kinds hold at most ONE target per subject: adding a new relation
    /// replaces the previous one (e.g. `ChildOf` — an entity has a single parent).
    /// Non-exclusive kinds accumulate (an entity may `Owns` many things).
    fn exclusive() -> bool {
        false
    }
}

// ── RelationRegistry ───────────────────────────────────────────

/// Хук связи (W3-1): `fn(&mut World, subject, target)` — вызывается после
/// завершения операции на консистентном мире (despawn-вычистка: entity могут
/// быть уже мертвы). Только fn-pointer; один хук на вид на событие.
pub type RelationHookFn = fn(&mut crate::world::World, Entity, Entity);

pub struct RelationRegistry {
    type_to_idx: FxHashMap<TypeId, u32>,
    cascade_flags: Vec<bool>,
    exclusive_flags: Vec<bool>,
    /// kind_idx → type_name строка (для сериализации).
    /// Инвариант: `idx_to_name[kind_idx]` заполняется в `get_or_register`.
    idx_to_name: Vec<String>,
    /// type_name → kind_idx (для десериализации).
    name_to_idx: FxHashMap<String, u32>,
    next_idx: u32,
    /// Хуки per kind_idx (W3-1); `any_hooks` — fast-path гейт горячих путей.
    on_add_hooks: Vec<Option<RelationHookFn>>,
    on_remove_hooks: Vec<Option<RelationHookFn>>,
    any_hooks: bool,
}

impl RelationRegistry {
    pub fn new() -> Self {
        Self {
            type_to_idx: FxHashMap::default(),
            cascade_flags: Vec::new(),
            exclusive_flags: Vec::new(),
            idx_to_name: Vec::new(),
            name_to_idx: FxHashMap::default(),
            next_idx: 0,
            on_add_hooks: Vec::new(),
            on_remove_hooks: Vec::new(),
            any_hooks: false,
        }
    }

    pub fn get_or_register<R: RelationKind>(&mut self) -> u32 {
        let type_id = TypeId::of::<R>();
        if let Some(&idx) = self.type_to_idx.get(&type_id) {
            return idx;
        }
        let idx = self.next_idx;
        self.next_idx += 1;
        self.type_to_idx.insert(type_id, idx);
        self.cascade_flags
            .push(R::cascade_delete_on_target_despawn());
        self.exclusive_flags.push(R::exclusive());
        self.on_add_hooks.push(None);
        self.on_remove_hooks.push(None);

        // Регистрируем name для сериализации
        let name = std::any::type_name::<R>().to_string();
        self.idx_to_name.push(name.clone());
        self.name_to_idx.insert(name, idx);

        idx
    }

    // ── Хуки связей (W3-1) ─────────────────────────────────────

    pub(crate) fn set_on_add(&mut self, kind_idx: u32, hook: RelationHookFn) {
        let slot = &mut self.on_add_hooks[kind_idx as usize];
        assert!(
            slot.is_none(),
            "on_relation_add-хук для вида {} уже зарегистрирован (один хук на вид)",
            self.idx_to_name[kind_idx as usize]
        );
        *slot = Some(hook);
        self.any_hooks = true;
    }

    pub(crate) fn set_on_remove(&mut self, kind_idx: u32, hook: RelationHookFn) {
        let slot = &mut self.on_remove_hooks[kind_idx as usize];
        assert!(
            slot.is_none(),
            "on_relation_remove-хук для вида {} уже зарегистрирован (один хук на вид)",
            self.idx_to_name[kind_idx as usize]
        );
        *slot = Some(hook);
        self.any_hooks = true;
    }

    #[inline]
    pub(crate) fn on_add_hook(&self, kind_idx: u32) -> Option<RelationHookFn> {
        if !self.any_hooks {
            return None;
        }
        self.on_add_hooks.get(kind_idx as usize).copied().flatten()
    }

    #[inline]
    pub(crate) fn on_remove_hook(&self, kind_idx: u32) -> Option<RelationHookFn> {
        if !self.any_hooks {
            return None;
        }
        self.on_remove_hooks
            .get(kind_idx as usize)
            .copied()
            .flatten()
    }

    /// Быстрая проверка «у вида есть on_remove-хук» (despawn-вычистка).
    #[inline]
    pub(crate) fn has_remove_hook(&self, kind_idx: u32) -> bool {
        self.on_remove_hook(kind_idx).is_some()
    }

    pub fn get_idx<R: RelationKind>(&self) -> Option<u32> {
        self.type_to_idx.get(&TypeId::of::<R>()).copied()
    }

    #[inline]
    pub fn is_cascade(&self, kind_idx: u32) -> bool {
        self.cascade_flags
            .get(kind_idx as usize)
            .copied()
            .unwrap_or(false)
    }

    #[inline]
    pub fn is_exclusive(&self, kind_idx: u32) -> bool {
        self.exclusive_flags
            .get(kind_idx as usize)
            .copied()
            .unwrap_or(false)
    }

    // ── Методы для сериализации ────────────────────────────────

    /// Получить type_name строку по kind_idx.
    ///
    /// Используется `WorldSerializer::snapshot` для записи human-readable
    /// имени relation в снэпшот.
    #[inline]
    pub fn get_name(&self, kind_idx: u32) -> Option<&str> {
        self.idx_to_name.get(kind_idx as usize).map(|s| s.as_str())
    }

    /// Получить kind_idx по type_name строке.
    ///
    /// Используется `WorldSerializer::restore` при восстановлении relations.
    /// Возвращает `None` если RelationKind не зарегистрирован в текущем мире
    /// (например, был удалён из кода после сохранения).
    #[inline]
    pub fn get_idx_by_name(&self, name: &str) -> Option<u32> {
        self.name_to_idx.get(name).copied()
    }

    /// Количество зарегистрированных видов relations.
    pub fn kind_count(&self) -> usize {
        self.next_idx as usize
    }
}

impl Default for RelationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── World extension ────────────────────────────────────────────

impl World {
    /// Добавить связь `(kind, target)` субъекту.
    ///
    /// Две индекс-вставки, БЕЗ структурного изменения мира (не создаёт
    /// архетипов, не двигает entity, не инвалидирует кэш запросов).
    ///
    /// Мёртвые subject/target игнорируются (с warn в лог): связь с мёртвым
    /// target никогда не была бы вычищена и при переиспользовании индекса
    /// указала бы на чужую entity.
    ///
    /// # Re-parenting and transforms (C5)
    ///
    /// Changing an entity's `ChildOf` parent does NOT touch its `LocalTransform`,
    /// so `propagate_transforms` (which keys on `Changed<LocalTransform>`) will not
    /// recompute the child's `GlobalTransform` on its own. The caller decides the
    /// intent: to keep the child's WORLD position, recompute and write its
    /// `LocalTransform` relative to the new parent (this also dirties it); to keep
    /// the child's LOCAL transform (it moves with the new parent), also stamp the
    /// `LocalTransform` change tick so propagation picks it up. Nothing here is
    /// done automatically because the two intents are mutually exclusive.
    pub fn add_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = self.relations.get_or_register::<R>();
        self.add_relation_by_kind_idx(subject, kind_idx, target);
    }

    /// Низкоуровневый вариант `add_relation` по уже известному kind_idx
    /// (горячие циклы, restore в apex-serialization).
    pub fn add_relation_by_kind_idx(&mut self, subject: Entity, kind_idx: u32, target: Entity) {
        debug_assert!(
            (kind_idx as usize) < self.relations.kind_count(),
            "add_relation_by_kind_idx: unregistered kind_idx {kind_idx}"
        );
        if !self.entities.is_alive(subject) || !self.entities.is_alive(target) {
            log::warn!(
                "add_relation: subject {subject} or target {target} is not alive — relation not added"
            );
            return;
        }
        // A relation to itself is never meaningful and forms a 1-cycle.
        if subject.index == target.index {
            log::warn!("add_relation: self-relation on {subject} rejected");
            return;
        }

        let exclusive = self.relations.is_exclusive(kind_idx);
        // For hierarchical kinds (exclusive or cascading), reject an edge that
        // would close a cycle: walking parents up from `target` along this kind
        // must not reach `subject`. A cycle would spin transform propagation and
        // cascade-despawn forever (C4). Non-hierarchical kinds may cycle freely.
        //
        // Fast skip: a cycle requires `subject` to already sit in `target`'s ancestor
        // chain, i.e. `subject` must be the target of some existing pair. If `subject`
        // is not a target of ANYTHING, the walk can never reach it — so skip it
        // entirely. This is the common case (attaching a fresh leaf to a parent) and
        // recovers the per-add cost the exclusive/cascade cycle check introduced.
        if (exclusive || self.relations.is_cascade(kind_idx))
            && self.target_index.is_any_target(subject.index)
        {
            let mut cur = target;
            let mut depth = 0usize;
            while let Some(parent) = self.subject_index.first_with_kind(cur.index, kind_idx) {
                if parent.target.index == subject.index {
                    log::warn!(
                        "add_relation: edge {subject} -> {target} (kind {kind_idx}) would create a cycle — rejected"
                    );
                    return;
                }
                cur = parent.target;
                depth += 1;
                if depth > MAX_ANCESTOR_WALK {
                    log::warn!(
                        "add_relation: ancestor chain for kind {kind_idx} exceeds the depth limit — rejected (possible pre-existing cycle)"
                    );
                    return;
                }
            }
        }

        // Exclusive kinds hold at most one target per subject: drop the old pair
        // (and queue its remove hook — this is what makes a re-parent fire
        // remove-then-add on the child).
        if exclusive {
            if let Some(old) = self.subject_index.first_with_kind(subject.index, kind_idx) {
                if old.target == target {
                    return; // already related to this exact target — no-op
                }
                self.subject_index.remove(subject.index, old);
                self.target_index.remove(kind_idx, old.target.index, subject);
                if self.relations.on_remove_hook(kind_idx).is_some() {
                    self.hook_queue
                        .push(crate::world::HookEvent::RelationRemoved {
                            kind_idx,
                            subject,
                            target: old.target,
                        });
                }
            }
        }

        let pair = RelationPair { kind_idx, target };
        if self.subject_index.insert(subject.index, pair) {
            self.target_index.add(kind_idx, target, subject);
            if self.relations.on_add_hook(kind_idx).is_some() {
                self.hook_queue.push(crate::world::HookEvent::RelationAdded {
                    kind_idx,
                    subject,
                    target,
                });
            }
        }
        self.flush_hooks();
    }

    pub fn remove_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return,
        };
        let pair = RelationPair { kind_idx, target };
        if self.subject_index.remove(subject.index, pair) {
            self.target_index.remove(kind_idx, target.index, subject);
            if self.relations.on_remove_hook(kind_idx).is_some() {
                self.hook_queue
                    .push(crate::world::HookEvent::RelationRemoved {
                        kind_idx,
                        subject,
                        target,
                    });
                self.flush_hooks();
            }
        }
    }

    /// O(1) проверка через SubjectIndex: kind_mask check + binary_search.
    #[inline]
    pub fn has_relation<R: RelationKind>(&self, subject: Entity, _kind: R, target: Entity) -> bool {
        if !self.entities.is_alive(subject) {
            return false;
        }
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return false,
        };
        let pair = RelationPair { kind_idx, target };
        self.subject_index.has(subject.index, pair)
    }

    /// Subjects связи `(kind, target)` с данными компонентов `Q`.
    ///
    /// План исполнения: subjects из TargetIndex → группировка по архетипам →
    /// `fetch_state` на архетип + точечные строки. O(детей), не O(архетипов).
    pub fn query_relation<'w, R: RelationKind, Q: WorldQuery>(
        &'w self,
        _kind: R,
        target: Entity,
    ) -> RelationIter<'w, Q> {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return RelationIter::empty(self),
        };
        if !self.entities.is_alive(target) {
            return RelationIter::empty(self);
        }
        let subjects = self.target_index.subjects(kind_idx, target.index);
        self.relation_iter_for_subjects(subjects.iter().copied())
    }

    /// Все subjects, имеющие связь вида `R` (wildcard `(R, *)`).
    /// Subject с несколькими target'ами одного вида выдаётся по разу на пару.
    pub fn query_wildcard<'w, R: RelationKind, Q: WorldQuery>(
        &'w self,
        _kind: R,
    ) -> RelationIter<'w, Q> {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return RelationIter::empty(self),
        };
        let subjects: Vec<Entity> = self.target_index.all_subjects(kind_idx).collect();
        self.relation_iter_for_subjects(subjects.into_iter())
    }

    fn relation_iter_for_subjects<'w, Q: WorldQuery>(
        &'w self,
        subjects: impl Iterator<Item = Entity>,
    ) -> RelationIter<'w, Q> {
        let mut data_ids = crate::query::IdBuf::new();
        Q::fill_ids(self, &mut data_ids);

        // Локации subjects, сгруппированные по архетипу (sort по arch, row).
        let mut locs: Vec<(u32, u32)> = subjects
            .filter_map(|s| {
                self.entities
                    .get_location(s)
                    .map(|l| (l.archetype_id.0, l.row))
            })
            .collect();
        locs.sort_unstable();

        let mut groups: Vec<RelationArchState<Q::State>> = Vec::new();
        let mut rows: Vec<(u32, u32)> = Vec::with_capacity(locs.len());
        let mut cur_arch = u32::MAX;
        let mut cur_group: Option<u32> = None;

        for (arch_id, row) in locs {
            if arch_id != cur_arch {
                cur_arch = arch_id;
                let arch = &self.archetypes[arch_id as usize];
                cur_group = if Q::matches_archetype(arch, &data_ids) {
                    let state =
                        unsafe { Q::fetch_state(arch, &data_ids, Tick::ZERO, self.current_tick()) };
                    groups.push(RelationArchState {
                        arch_idx: arch_id as usize,
                        state,
                    });
                    Some((groups.len() - 1) as u32)
                } else {
                    None
                };
            }
            if let Some(g) = cur_group {
                rows.push((g, row));
            }
        }

        RelationIter {
            world: self,
            groups,
            rows,
            cursor: 0,
        }
    }

    /// Дети (subjects) связи `(kind, parent)` — O(числа детей).
    pub fn children_of<'w, R: RelationKind>(
        &'w self,
        _kind: R,
        parent: Entity,
    ) -> impl Iterator<Item = Entity> + 'w {
        let subjects: &[Entity] = match self.relations.get_idx::<R>() {
            Some(kind_idx) if self.entities.is_alive(parent) => {
                self.target_index.subjects(kind_idx, parent.index)
            }
            _ => &[],
        };
        subjects.iter().copied()
    }

    /// Target первой связи вида `R` у субъекта (с корректным generation).
    pub fn get_relation_target<R: RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
    ) -> Option<Entity> {
        if !self.entities.is_alive(subject) {
            return None;
        }
        let kind_idx = self.relations.get_idx::<R>()?;
        self.subject_index
            .first_with_kind(subject.index, kind_idx)
            .map(|p| p.target)
    }

    /// Рекурсивный despawn поддерева по виду связи.
    ///
    /// Для kind'ов с `cascade_delete_on_target_despawn()` обычный `despawn`
    /// корня делает то же самое автоматически.
    pub fn despawn_recursive<R: RelationKind + Copy>(&mut self, _kind: R, entity: Entity) {
        // Каскадный kind (напр. ChildOf): обычный `despawn` УЖЕ сносит всё поддерево эффективно —
        // его внутренний стек делает `take_subjects` (забирает ВЕСЬ список детей разом, O(поддерева)).
        // Ручная же рекурсия ниже снесла бы детей ПЕРВЫМИ, удаляя каждого из target-списка ещё
        // живого родителя через linear search+remove ⇒ O(n²) (на 1000 детей было 2.4× медленнее Bevy).
        if R::cascade_delete_on_target_despawn() {
            self.despawn(entity);
            return;
        }
        // Не-каскадный kind: связь не сносит subjects автоматически — обходим вручную.
        let children: Vec<Entity> = self.children_of(_kind, entity).collect();
        for child in children {
            self.despawn_recursive(_kind, child);
        }
        self.despawn(entity);
    }

    /// Все связи мира: `(subject.index, kind_idx, target)` — для сериализации.
    pub fn iter_relations(&self) -> impl Iterator<Item = (u32, u32, Entity)> + '_ {
        self.subject_index
            .iter_all()
            .map(|(subject_index, pair)| (subject_index, pair.kind_idx, pair.target))
    }
}

// ── RelationIter ───────────────────────────────────────────────

pub(crate) struct RelationArchState<S> {
    pub arch_idx: usize,
    pub state: S,
}

pub struct RelationIter<'w, Q: WorldQuery> {
    world: &'w World,
    groups: Vec<RelationArchState<Q::State>>,
    /// (индекс группы, row) на каждого subject.
    rows: Vec<(u32, u32)>,
    cursor: usize,
}

impl<'w, Q: WorldQuery> RelationIter<'w, Q> {
    pub(crate) fn empty(world: &'w World) -> Self {
        Self {
            world,
            groups: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
        }
    }
}

impl<'w, Q: WorldQuery> Iterator for RelationIter<'w, Q> {
    type Item = (Entity, Q::Item<'w>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let &(group_idx, row) = self.rows.get(self.cursor)?;
            self.cursor += 1;
            let group = &self.groups[group_idx as usize];
            if let Some(item) = unsafe { Q::fetch_item(group.state, row as usize) } {
                let entity = self.world.archetypes[group.arch_idx].entities[row as usize];
                return Some((entity, item));
            }
        }
    }
}

// ── Встроенные relation kinds ──────────────────────────────────

#[derive(Clone, Copy)]
pub struct ChildOf;
impl RelationKind for ChildOf {
    fn cascade_delete_on_target_despawn() -> bool {
        true
    }
    /// An entity has exactly one parent — a second `set_parent` replaces the first.
    fn exclusive() -> bool {
        true
    }
}

#[derive(Clone, Copy)]
pub struct Owns;
impl RelationKind for Owns {}

#[derive(Clone, Copy)]
pub struct Likes;
impl RelationKind for Likes {}

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::prelude::*;

    use crate::component::Component;

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Position {
        x: f32,
        y: f32,
    }
    impl Component for Position {}

    fn pair(kind_idx: u32, index: u32) -> RelationPair {
        RelationPair {
            kind_idx,
            target: Entity::from_raw_parts(index, 0),
        }
    }

    #[test]
    fn add_has_remove_relation() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));

        world.add_relation(child, ChildOf, parent);
        assert!(world.has_relation(child, ChildOf, parent));
        assert!(!world.has_relation(parent, ChildOf, child));

        world.remove_relation(child, ChildOf, parent);
        assert!(!world.has_relation(child, ChildOf, parent));
    }

    #[test]
    fn add_relation_no_structural_change() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));

        let arch_count = world.archetype_count();
        world.add_relation(child, ChildOf, parent);
        assert_eq!(
            world.archetype_count(),
            arch_count,
            "add_relation не должен создавать архетипов"
        );
    }

    #[test]
    fn subject_entry_kind_mask() {
        let mut entry = SubjectEntry::default();
        let rel1 = pair(0, 100);
        let rel2 = pair(1, 200);
        let rel3 = pair(0, 300);

        entry.insert(rel1);
        assert!(entry.has_kind(0));
        assert!(!entry.has_kind(1));

        entry.insert(rel2);
        assert!(entry.has_kind(1));

        entry.insert(rel3);
        entry.remove(rel1);
        assert!(entry.has_kind(0));

        entry.remove(rel3);
        assert!(!entry.has_kind(0));
    }

    #[test]
    fn relation_registry_name_roundtrip() {
        let mut reg = RelationRegistry::new();
        let idx = reg.get_or_register::<ChildOf>();

        // Проверяем что имя сохраняется
        let name = reg.get_name(idx).unwrap();
        assert!(name.contains("ChildOf"));

        // Проверяем обратный поиск
        let found_idx = reg.get_idx_by_name(name).unwrap();
        assert_eq!(found_idx, idx);
    }

    #[test]
    fn despawn_recursive() {
        let mut world = World::new();
        world.register_component::<Position>();
        let root = world.spawn((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));
        let leaf = world.spawn((Position { x: 2.0, y: 0.0 },));

        world.add_relation(child, ChildOf, root);
        world.add_relation(leaf, ChildOf, child);

        assert_eq!(world.entity_count(), 3);
        world.despawn_recursive(ChildOf, root);
        assert_eq!(world.entity_count(), 0);
    }

    #[test]
    fn get_relation_target() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));

        world.add_relation(child, ChildOf, parent);
        assert_eq!(world.get_relation_target(child, ChildOf), Some(parent));
    }

    // ── Новые гарантии CR-M1 ───────────────────────────────────

    /// C-2: cascade-вид (ChildOf) деспавнит детей при despawn родителя.
    #[test]
    fn despawn_target_cascades() {
        let mut world = World::new();
        world.register_component::<Position>();
        let root = world.spawn((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));
        let leaf = world.spawn((Position { x: 2.0, y: 0.0 },));

        world.add_relation(child, ChildOf, root);
        world.add_relation(leaf, ChildOf, child);

        world.despawn(root);
        assert_eq!(world.entity_count(), 0, "ChildOf — cascade: дети умирают с родителем");
    }

    /// C-2: не-cascade вид — subjects живут, но связь вычищается.
    #[test]
    fn despawn_target_clears_non_cascade() {
        let mut world = World::new();
        world.register_component::<Position>();
        let owner = world.spawn((Position { x: 0.0, y: 0.0 },));
        let item = world.spawn((Position { x: 1.0, y: 0.0 },));

        world.add_relation(item, Owns, owner);
        assert!(world.has_relation(item, Owns, owner));

        world.despawn(owner);
        assert!(world.is_alive(item));
        assert_eq!(
            world.get_relation_target(item, Owns),
            None,
            "связь не должна переживать свой target"
        );
    }

    /// C-2/C-3: переиспользованный index не возвращает чужие связи.
    #[test]
    fn reused_index_does_not_leak_relations() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));
        world.add_relation(child, ChildOf, parent);

        world.despawn(parent); // cascade: child тоже умер

        // Новые entity переиспользуют оба освободившихся индекса
        let n1 = world.spawn((Position { x: 9.0, y: 9.0 },));
        let n2 = world.spawn((Position { x: 8.0, y: 8.0 },));
        let newcomer = [n1, n2]
            .into_iter()
            .find(|e| e.index() == parent.index())
            .expect("тест требует переиспользования index родителя");

        assert_eq!(world.children_of(ChildOf, newcomer).count(), 0);
        // Стейл-хэндл старого родителя тоже ничего не возвращает
        assert_eq!(world.children_of(ChildOf, parent).count(), 0);
    }

    #[test]
    fn children_of_lists_all_children() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let mut spawned: Vec<Entity> = Vec::new();
        for i in 0..30 {
            let c = world.spawn((Position { x: i as f32, y: 0.0 },));
            world.add_relation(c, ChildOf, parent);
            spawned.push(c);
        }
        let mut children: Vec<Entity> = world.children_of(ChildOf, parent).collect();
        children.sort_by_key(|e| e.index());
        spawned.sort_by_key(|e| e.index());
        assert_eq!(children, spawned);
    }

    #[test]
    fn query_relation_fetches_components() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let c1 = world.spawn((Position { x: 1.0, y: 0.0 },));
        let c2 = world.spawn((Position { x: 2.0, y: 0.0 },));
        world.add_relation(c1, ChildOf, parent);
        world.add_relation(c2, ChildOf, parent);

        let mut seen: Vec<(Entity, f32)> = world
            .query_relation::<ChildOf, crate::query::Read<Position>>(ChildOf, parent)
            .map(|(e, p)| (e, p.x))
            .collect();
        seen.sort_by(|a, b| a.1.total_cmp(&b.1));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], (c1, 1.0));
        assert_eq!(seen[1], (c2, 2.0));
    }

    #[test]
    fn query_wildcard_fetches_all_subjects() {
        let mut world = World::new();
        world.register_component::<Position>();
        let p1 = world.spawn((Position { x: 0.0, y: 0.0 },));
        let p2 = world.spawn((Position { x: 0.0, y: 1.0 },));
        let c1 = world.spawn((Position { x: 1.0, y: 0.0 },));
        let c2 = world.spawn((Position { x: 2.0, y: 0.0 },));
        world.add_relation(c1, ChildOf, p1);
        world.add_relation(c2, ChildOf, p2);

        let count = world
            .query_wildcard::<ChildOf, crate::query::Read<Position>>(ChildOf)
            .count();
        assert_eq!(count, 2);
    }

    #[test]
    fn add_relation_dead_target_is_noop() {
        let mut world = World::new();
        world.register_component::<Position>();
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        world.despawn(parent);

        world.add_relation(child, ChildOf, parent);
        assert!(!world.has_relation(child, ChildOf, parent));
        assert_eq!(world.get_relation_target(child, ChildOf), None);
    }

    /// B9: ChildOf is exclusive — a second `add_relation` re-parents the child
    /// instead of giving it two parents (the source of the propagate torn-write).
    #[test]
    fn exclusive_childof_replaces_previous_parent() {
        let mut world = World::new();
        world.register_component::<Position>();
        let child = world.spawn((Position { x: 0.0, y: 0.0 },));
        let p1 = world.spawn((Position { x: 1.0, y: 0.0 },));
        let p2 = world.spawn((Position { x: 2.0, y: 0.0 },));

        world.add_relation(child, ChildOf, p1);
        assert_eq!(world.get_relation_target(child, ChildOf), Some(p1));

        world.add_relation(child, ChildOf, p2); // exclusive → replaces p1
        assert_eq!(world.get_relation_target(child, ChildOf), Some(p2));
        assert_eq!(world.children_of(ChildOf, p1).count(), 0, "old parent lost the child");
        assert_eq!(world.children_of(ChildOf, p2).count(), 1, "new parent gained it");
    }

    /// B9: a non-exclusive kind still accumulates multiple targets.
    #[test]
    fn non_exclusive_relation_still_accumulates() {
        let mut world = World::new();
        world.register_component::<Position>();
        let hub = world.spawn((Position { x: 0.0, y: 0.0 },));
        let t1 = world.spawn((Position { x: 1.0, y: 0.0 },));
        let t2 = world.spawn((Position { x: 2.0, y: 0.0 },));
        world.add_relation(hub, Likes, t1);
        world.add_relation(hub, Likes, t2);
        assert!(world.has_relation(hub, Likes, t1));
        assert!(world.has_relation(hub, Likes, t2));
    }

    /// B9/C4: a self-relation is rejected (it would be a 1-cycle).
    #[test]
    fn self_relation_rejected() {
        let mut world = World::new();
        world.register_component::<Position>();
        let a = world.spawn((Position { x: 0.0, y: 0.0 },));
        world.add_relation(a, ChildOf, a);
        assert_eq!(world.get_relation_target(a, ChildOf), None);
    }

    /// C4: a cycle-forming ChildOf edge is rejected, so transform propagation and
    /// cascade-despawn can never spin on it.
    #[test]
    fn childof_cycle_rejected() {
        let mut world = World::new();
        world.register_component::<Position>();
        let a = world.spawn((Position { x: 0.0, y: 0.0 },));
        let b = world.spawn((Position { x: 1.0, y: 0.0 },));
        let c = world.spawn((Position { x: 2.0, y: 0.0 },));

        world.add_relation(a, ChildOf, b); // a -> b
        world.add_relation(b, ChildOf, c); // b -> c
        // c -> a would close the cycle a->b->c->a: rejected.
        world.add_relation(c, ChildOf, a);
        assert_eq!(world.get_relation_target(c, ChildOf), None, "cycle edge rejected");
        assert_eq!(world.get_relation_target(a, ChildOf), Some(b), "existing edges intact");
        assert_eq!(world.get_relation_target(b, ChildOf), Some(c));
    }

    #[test]
    fn pair_storage_dense_upgrade() {
        let mut world = World::new();
        world.register_component::<Position>();
        let hub = world.spawn((Position { x: 0.0, y: 0.0 },));
        let targets: Vec<Entity> = (0..20)
            .map(|i| world.spawn((Position { x: i as f32, y: 0.0 },)))
            .collect();
        for &t in &targets {
            world.add_relation(hub, Likes, t);
        }
        for &t in &targets {
            assert!(world.has_relation(hub, Likes, t));
        }
        // Удаление одного target'а вычищает ровно его
        world.despawn(targets[7]);
        assert!(!world.has_relation(hub, Likes, targets[7]));
        let alive = targets.iter().filter(|&&t| world.has_relation(hub, Likes, t)).count();
        assert_eq!(alive, 19);
    }
}
