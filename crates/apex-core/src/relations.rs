//! Relations — links between entities.
//!
//! # Model (CR-M1, 2026-06-11)
//!
//! The `(kind, target)` pair is NOT a component and does not participate in
//! archetype identity. The truth about relations lives in two world indexes:
//!
//! - [`SubjectIndex`]: `entity.index` → set of [`RelationPair`] (kind + target
//!   **in full**, with generation) — backs `has_relation`,
//!   `target_of`, serialization;
//! - [`TargetIndex`]: `(kind, target.index)` → subjects — backs
//!   `targets_of` (O(children)), `query_relation`/`query_wildcard` and
//!   **cascade delete on `despawn(target)`**.
//!
//! Consequences:
//! - `add_relation` = two index insertions, WITHOUT any structural change
//!   (no archetype move, no new archetype, no QueryCache invalidation);
//! - `despawn(target)` clears every relation where the entity is the target; for
//!   kinds with `cascade_delete_on_target_despawn()` the subjects are despawned
//!   cascadingly;
//! - generation-correctness: the indexes store the whole `Entity`, so
//!   reusing `entity.index` for a new generation does not return foreign
//!   relations; the encoding limits (2^20 for index, 2^11 for kind) are gone.
//!
//! # Sibling order (core ADR-008, 2026-07-13)
//!
//! The subject list of every `(kind, target)` entry is ORDERED and the order is a
//! public guarantee for ALL kinds:
//!
//! - `add_relation` appends; removal is order-preserving (no `swap_remove` holes);
//! - [`World::insert_relation_at`] / [`World::set_relation_index`] /
//!   [`World::relation_index`] position subjects explicitly (UI z-order, editor
//!   sibling order, scene determinism);
//! - `targets_of` yields subjects in exactly that order; snapshot/restore
//!   reproduces it (target-major emission in `WorldSerializer`).
//!
//! The one deliberate exception: `query_relation`/`query_wildcard` re-group
//! subjects by (archetype, row) for fetch efficiency and do NOT preserve sibling
//! order — callers that need both order and component data iterate `targets_of`
//! and fetch per entity.
//!
//! The historical model where "the pair is encoded into a ComponentId and forms
//! part of the archetype" fragmented the world into an archetype-per-parent
//! (many_foxes @1000 — 22k archetypes) and has been removed; see
//! apex-engine/plans/CORE_REFACTORING.md (C-1..C-3).

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

/// A single relation of a subject: kind + target in full (index + generation).
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

/// Threshold for switching from Sparse to Dense relation storage.
/// When the number of relations per entity exceeds DENSE_THRESHOLD,
/// the storage switches to a HashSet for O(1) operations.
const DENSE_THRESHOLD: usize = 8;

/// Upper bound on the ancestor walk used for cycle detection in `add_relation`.
/// A well-formed hierarchy is far shallower; exceeding it means a pre-existing
/// cycle (defensive) and the edge is rejected. Bounds the cost against a
/// pathological chain while never limiting any real scene graph.
const MAX_ANCESTOR_WALK: usize = 1 << 20;

/// Relation storage for a single entity.
///
/// - `Sparse`: a sorted SmallVec for a small number of relations (≤8).
///   Uses binary_search — O(log n).
/// - `Dense`: FxHashSet for a large number of relations (>8).
///   All operations O(1) amortized.
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
    /// true if the pair was absent (inserted for the first time).
    #[inline]
    fn insert(&mut self, pair: RelationPair) -> bool {
        match self {
            Self::Sparse(sv) => {
                let pos = match sv.binary_search(&pair) {
                    Ok(_) => return false, // already exists
                    Err(pos) => pos,
                };
                sv.insert(pos, pair);
                // Auto-upgrade: switch to Dense once the threshold is exceeded
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

    /// First pair of the given kind (for Sparse — with the smallest target).
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

    /// Take all pairs, leaving the storage empty (no allocations for Sparse≤4).
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
    /// Bitmask: bit k is set ↔ there is a relation with kind_idx = k.
    kind_mask: u64,
    /// Pair storage (Sparse for ≤8, Dense for >8).
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
            // Check whether any relations of this kind remain
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

    /// true if the pair was absent (inserted for the first time).
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

    /// Take all pairs of an entity (for despawn).
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

    /// Iterate all (subject_index, pair) — for serialization.
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

/// Reverse index: `(kind, target.index)` → subjects.
///
/// The key is the bare `target.index`: entries are cleared on the target's
/// despawn, so a live entry always belongs to the CURRENT generation of the index
/// (generation-correctness is ensured by the cleanup, subjects are stored in full).
pub(crate) struct TargetIndex {
    /// kind_idx → (target.index → subjects in insertion order).
    by_kind: Vec<FxHashMap<u32, SmallVec<[Entity; 4]>>>,
    /// Number of pairs where `entity.index` is the target (fast-skip in despawn).
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
    fn bump_target_count(&mut self, target_index: u32) {
        let ti = target_index as usize;
        if ti >= self.target_counts.len() {
            self.target_counts.resize(ti + 1, 0);
        }
        self.target_counts[ti] += 1;
    }

    #[inline]
    pub fn add(&mut self, kind_idx: u32, target: Entity, subject: Entity) {
        self.ensure_kind(kind_idx)
            .entry(target.index())
            .or_default()
            .push(subject);
        self.bump_target_count(target.index());
    }

    /// Insert `subject` at `index` (clamped to the list length) of the
    /// `(kind, target)` sibling list. Same index bookkeeping as [`Self::add`].
    #[inline]
    pub fn add_at(&mut self, kind_idx: u32, target: Entity, subject: Entity, index: usize) {
        let list = self.ensure_kind(kind_idx).entry(target.index()).or_default();
        let pos = index.min(list.len());
        list.insert(pos, subject);
        self.bump_target_count(target.index());
    }

    /// Position of `subject` within the `(kind, target)` sibling list.
    #[inline]
    pub fn position(&self, kind_idx: u32, target_index: u32, subject: Entity) -> Option<usize> {
        self.by_kind
            .get(kind_idx as usize)?
            .get(&target_index)?
            .iter()
            .position(|&s| s == subject)
    }

    /// Move `subject` to `index` (clamped) within the `(kind, target)` sibling
    /// list. Pure order change: the edge set, counts and hooks are untouched.
    /// Returns `false` if the subject is not in the list.
    #[inline]
    pub fn move_to(
        &mut self,
        kind_idx: u32,
        target_index: u32,
        subject: Entity,
        index: usize,
    ) -> bool {
        let Some(map) = self.by_kind.get_mut(kind_idx as usize) else {
            return false;
        };
        let Some(list) = map.get_mut(&target_index) else {
            return false;
        };
        let Some(pos) = list.iter().position(|&s| s == subject) else {
            return false;
        };
        list.remove(pos);
        let new_pos = index.min(list.len());
        list.insert(new_pos, subject);
        true
    }

    /// `(target_index, subjects)` entries of a kind with targets sorted by index —
    /// a DETERMINISTIC target-major view (the backing map iterates in hash order).
    /// Cold-path only (snapshot): allocates the sorted key vector.
    pub fn entries_sorted(&self, kind_idx: u32) -> Vec<(u32, &[Entity])> {
        let Some(map) = self.by_kind.get(kind_idx as usize) else {
            return Vec::new();
        };
        let mut entries: Vec<(u32, &[Entity])> =
            map.iter().map(|(&t, sv)| (t, sv.as_slice())).collect();
        entries.sort_unstable_by_key(|&(t, _)| t);
        entries
    }

    /// Remove one `subject` from the `(kind, target)` entry, PRESERVING the order
    /// of the remaining subjects (sibling order is a public guarantee — see the
    /// module docs; a `swap_remove` here would teleport the last sibling into the
    /// hole and break UI z-order / editor sibling order on every removal).
    ///
    /// §1.4 compromise (documented, not a footgun): the `position` scan is O(N) in the
    /// entry's FAN-IN — how many subjects point at THIS target via THIS kind (e.g. a
    /// parent's direct children for `ChildOf`). That fan-in is small for typical
    /// relations (dozens), so the inline `SmallVec<[Entity; 4]>` scan + shift-remove
    /// beats maintaining a secondary per-entry index (a `HashSet` would add hashing +
    /// an allocation per entry, losing the cache-friendly inline path for the common
    /// small case; the ordered `remove(pos)` memmove is the same O(fan-in) class the
    /// scan already pays). It degrades only under a pathological single target with
    /// a huge fan-in whose subjects are removed one-by-one (O(N²) total) — if such a
    /// workload appears, switch this entry's list to an order-indexed structure.
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
        subjects.remove(pos);
        if subjects.is_empty() {
            map.remove(&target_index);
        }
        self.target_counts[target_index as usize] -= 1;
        true
    }

    /// Take all subjects of the entry `(kind, target.index)` (for despawn).
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

    /// All subjects of a kind (wildcard) — one occurrence per pair.
    pub fn all_subjects(&self, kind_idx: u32) -> impl Iterator<Item = Entity> + '_ {
        self.by_kind
            .get(kind_idx as usize)
            .into_iter()
            .flat_map(|m| m.values().flat_map(|sv| sv.iter().copied()))
    }

    /// Whether there is any relation where the given index is the target.
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
    /// On `despawn(target)` subjects of this kind are despawned cascadingly.
    /// For other kinds the relation is simply cleared from the indexes
    /// (no relation outlives its target).
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

/// Relation hook (W3-1): `fn(&mut World, subject, target)` — called after the
/// operation completes on a consistent world (despawn cleanup: entities may
/// already be dead). fn-pointer only; one hook per kind per event.
pub type RelationHookFn = fn(&mut crate::world::World, Entity, Entity);

pub struct RelationRegistry {
    type_to_idx: FxHashMap<TypeId, u32>,
    cascade_flags: Vec<bool>,
    exclusive_flags: Vec<bool>,
    /// kind_idx → type_name string (for serialization).
    /// Invariant: `idx_to_name[kind_idx]` is filled in `get_or_register`.
    idx_to_name: Vec<String>,
    /// type_name → kind_idx (for deserialization).
    name_to_idx: FxHashMap<String, u32>,
    next_idx: u32,
    /// Hooks per kind_idx (W3-1); `any_hooks` — fast-path gate for hot paths.
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

        // Register the name for serialization
        let name = std::any::type_name::<R>().to_string();
        self.idx_to_name.push(name.clone());
        self.name_to_idx.insert(name, idx);

        idx
    }

    // ── Relation hooks (W3-1) ──────────────────────────────────

    pub(crate) fn set_on_add(&mut self, kind_idx: u32, hook: RelationHookFn) {
        let slot = &mut self.on_add_hooks[kind_idx as usize];
        assert!(
            slot.is_none(),
            "on_relation_add hook for kind {} is already registered (one hook per kind)",
            self.idx_to_name[kind_idx as usize]
        );
        *slot = Some(hook);
        self.any_hooks = true;
    }

    pub(crate) fn set_on_remove(&mut self, kind_idx: u32, hook: RelationHookFn) {
        let slot = &mut self.on_remove_hooks[kind_idx as usize];
        assert!(
            slot.is_none(),
            "on_relation_remove hook for kind {} is already registered (one hook per kind)",
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

    /// Fast check "the kind has an on_remove hook" (despawn cleanup).
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

    // ── Serialization methods ──────────────────────────────────

    /// Get the type_name string by kind_idx.
    ///
    /// Used by `WorldSerializer::snapshot` to write the human-readable
    /// relation name into the snapshot.
    #[inline]
    pub fn get_name(&self, kind_idx: u32) -> Option<&str> {
        self.idx_to_name.get(kind_idx as usize).map(|s| s.as_str())
    }

    /// Get the kind_idx by type_name string.
    ///
    /// Used by `WorldSerializer::restore` when restoring relations.
    /// Returns `None` if the RelationKind is not registered in the current world
    /// (for example, it was removed from the code after saving).
    #[inline]
    pub fn get_idx_by_name(&self, name: &str) -> Option<u32> {
        self.name_to_idx.get(name).copied()
    }

    /// Number of registered relation kinds.
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
    /// Add a relation `(kind, target)` to a subject.
    ///
    /// Two index insertions, WITHOUT any structural change to the world (creates
    /// no archetypes, does not move entities, does not invalidate the query cache).
    ///
    /// Dead subject/target are ignored (with a warn in the log): a relation to a
    /// dead target would never be cleared and, on index reuse, would point at a
    /// foreign entity.
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

    /// Low-level variant of `add_relation` by an already-known kind_idx
    /// (hot loops, restore in apex-serialization). Appends to the sibling list.
    pub fn add_relation_by_kind_idx(&mut self, subject: Entity, kind_idx: u32, target: Entity) {
        self.add_relation_positioned(subject, kind_idx, target, None);
    }

    /// Shared add path: `position: None` appends, `Some(i)` inserts at `i`
    /// (clamped) in the `(kind, target)` sibling list. All add invariants
    /// (aliveness, cycle rejection, exclusive replace, hooks) live here once.
    fn add_relation_positioned(
        &mut self,
        subject: Entity,
        kind_idx: u32,
        target: Entity,
        position: Option<usize>,
    ) {
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
            match position {
                None => self.target_index.add(kind_idx, target, subject),
                Some(i) => self.target_index.add_at(kind_idx, target, subject, i),
            }
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

    /// Ensure the relation `(kind, target)` exists on `subject` AT `index` of the
    /// target's sibling list (clamped to the list length).
    ///
    /// - Pair already exists → pure reorder to `index` (no hooks: the edge set is
    ///   unchanged — same semantics as [`set_relation_index`](Self::set_relation_index));
    /// - pair absent → full add (aliveness/cycle/exclusive invariants identical to
    ///   [`add_relation`](Self::add_relation), including the exclusive re-parent)
    ///   with a positioned insert instead of an append.
    ///
    /// This is the write half of the sibling-order guarantee (module docs): UI
    /// z-order, editor "move child up/down", deterministic scene assembly.
    pub fn insert_relation_at<R: RelationKind>(
        &mut self,
        subject: Entity,
        _kind: R,
        target: Entity,
        index: usize,
    ) {
        let kind_idx = self.relations.get_or_register::<R>();
        if self.entities.is_alive(subject)
            && self.entities.is_alive(target)
            && self
                .subject_index
                .has(subject.index, RelationPair { kind_idx, target })
        {
            self.target_index.move_to(kind_idx, target.index, subject, index);
            return;
        }
        self.add_relation_positioned(subject, kind_idx, target, Some(index));
    }

    /// Move an EXISTING relation's subject to `index` (clamped) in the target's
    /// sibling list. Pure order change: no hooks fire, counts are untouched.
    /// Returns `false` when the pair does not exist (or either entity is dead) —
    /// this never creates a relation (use
    /// [`insert_relation_at`](Self::insert_relation_at) for ensure-at semantics).
    pub fn set_relation_index<R: RelationKind>(
        &mut self,
        subject: Entity,
        _kind: R,
        target: Entity,
        index: usize,
    ) -> bool {
        if !self.entities.is_alive(subject) || !self.entities.is_alive(target) {
            return false;
        }
        let Some(kind_idx) = self.relations.get_idx::<R>() else {
            return false;
        };
        if !self
            .subject_index
            .has(subject.index, RelationPair { kind_idx, target })
        {
            return false;
        }
        self.target_index.move_to(kind_idx, target.index, subject, index)
    }

    /// Position of `subject` in the `(kind, target)` sibling list — the read half
    /// of the sibling-order guarantee. `None` if the pair does not exist.
    pub fn relation_index<R: RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
        target: Entity,
    ) -> Option<usize> {
        if !self.entities.is_alive(subject) || !self.entities.is_alive(target) {
            return None;
        }
        let kind_idx = self.relations.get_idx::<R>()?;
        if !self
            .subject_index
            .has(subject.index, RelationPair { kind_idx, target })
        {
            return None;
        }
        self.target_index.position(kind_idx, target.index, subject)
    }

    /// O(1) check via SubjectIndex: kind_mask check + binary_search.
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

    /// Read-only resolve of a relation kind `R` to its registry index (does NOT
    /// register). `None` if `R` was never used in this world. Used by the
    /// dynamic query relation-filter (S8) to resolve terms at build time.
    pub(crate) fn relation_kind_idx<R: RelationKind>(&self) -> Option<u32> {
        self.relations.get_idx::<R>()
    }

    /// Whether `subject` has a relation of `kind_idx` to `target` (or to ANY
    /// target when `target` is `None`). Backs [`QueryBuilder`]'s relation terms
    /// (a per-entity post-filter — relations are not archetype-structural).
    ///
    /// [`QueryBuilder`]: crate::query::QueryBuilder
    pub(crate) fn subject_has_relation_idx(
        &self,
        subject: Entity,
        kind_idx: u32,
        target: Option<Entity>,
    ) -> bool {
        if !self.entities.is_alive(subject) {
            return false;
        }
        match target {
            Some(t) => self
                .subject_index
                .has(subject.index, RelationPair { kind_idx, target: t }),
            None => self
                .subject_index
                .first_with_kind(subject.index, kind_idx)
                .is_some(),
        }
    }

    /// Subjects of the relation `(kind, target)` with component data `Q`.
    ///
    /// Execution plan: subjects from TargetIndex → grouping by archetypes →
    /// `fetch_state` per archetype + pinpoint rows. O(children), not O(archetypes).
    ///
    /// **Does NOT preserve sibling order** (subjects are re-grouped by
    /// (archetype, row) for fetch efficiency) — deliberate; callers that need
    /// order AND data iterate [`targets_of`](Self::targets_of) + per-entity get.
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

    /// All subjects that have a relation of kind `R` (wildcard `(R, *)`).
    /// A subject with several targets of one kind is yielded once per pair.
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

        // Subject locations, grouped by archetype (sorted by arch, row).
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

    /// Subjects pointing at `parent` via relation `R` — O(number of subjects).
    /// The plural pairs with [`target_of`](Self::target_of) (single target of a
    /// subject); `_of` naming reads uniformly for both directions.
    ///
    /// **Order guarantee (core ADR-008):** subjects are yielded in SIBLING ORDER —
    /// insertion order, stable across removals, explicitly controllable via
    /// [`insert_relation_at`](Self::insert_relation_at) /
    /// [`set_relation_index`](Self::set_relation_index).
    pub fn targets_of<'w, R: RelationKind>(
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

    /// Target of the first relation of kind `R` from `subject` (generation-correct).
    pub fn target_of<R: RelationKind>(&self, subject: Entity, _kind: R) -> Option<Entity> {
        if !self.entities.is_alive(subject) {
            return None;
        }
        let kind_idx = self.relations.get_idx::<R>()?;
        self.subject_index
            .first_with_kind(subject.index, kind_idx)
            .map(|p| p.target)
    }

    /// Recursive despawn of a subtree by relation kind.
    ///
    /// For kinds with `cascade_delete_on_target_despawn()` a plain `despawn` of
    /// the root does the same thing automatically.
    pub fn despawn_recursive<R: RelationKind + Copy>(&mut self, _kind: R, entity: Entity) {
        // Cascading kind (e.g. ChildOf): a plain `despawn` ALREADY tears down the whole subtree
        // efficiently — its internal stack uses `take_subjects` (grabs the ENTIRE child list at once,
        // O(subtree)). The manual recursion below would tear down children FIRST, removing each from
        // the still-alive parent's target list via linear search+remove ⇒ O(n²) (for 1000 children it
        // was 2.4× slower than Bevy).
        if R::cascade_delete_on_target_despawn() {
            self.despawn(entity);
            return;
        }
        // Non-cascading kind: the relation does not tear down subjects automatically — walk manually.
        let children: Vec<Entity> = self.targets_of(_kind, entity).collect();
        for child in children {
            self.despawn_recursive(_kind, child);
        }
        self.despawn(entity);
    }

    /// All relations of the world: `(subject.index, kind_idx, target)` — subject-major
    /// (entity-index order). Does NOT encode sibling order — snapshots use
    /// [`iter_relations_target_major`](Self::iter_relations_target_major) instead.
    pub fn iter_relations(&self) -> impl Iterator<Item = (u32, u32, Entity)> + '_ {
        self.subject_index
            .iter_all()
            .map(|(subject_index, pair)| (subject_index, pair.kind_idx, pair.target))
    }

    /// All relations in DETERMINISTIC target-major order: kinds ascending, targets
    /// by index ascending, subjects in SIBLING ORDER. Yields
    /// `(subject, kind_idx, target_index)`.
    ///
    /// This is the snapshot emission order: restore re-adds relations in list
    /// order (appends), so per-`(kind, target)` sibling order round-trips exactly
    /// while the byte stream stays deterministic (the backing map iterates in
    /// hash order, hence the sorted collection). Cold path — allocates.
    pub fn iter_relations_target_major(&self) -> Vec<(Entity, u32, u32)> {
        let mut out = Vec::new();
        for kind_idx in 0..self.relations.kind_count() as u32 {
            for (target_index, subjects) in self.target_index.entries_sorted(kind_idx) {
                for &subject in subjects {
                    out.push((subject, kind_idx, target_index));
                }
            }
        }
        out
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
    /// (group index, row) per subject.
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

// ── Built-in relation kinds ────────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────

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
            "add_relation must not create archetypes"
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

        // Verify the name is stored
        let name = reg.get_name(idx).unwrap();
        assert!(name.contains("ChildOf"));

        // Verify the reverse lookup
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
        assert_eq!(world.target_of(child, ChildOf), Some(parent));
    }

    // ── New CR-M1 guarantees ───────────────────────────────────

    /// C-2: a cascade kind (ChildOf) despawns children on the parent's despawn.
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
        assert_eq!(world.entity_count(), 0, "ChildOf — cascade: children die with the parent");
    }

    /// C-2: a non-cascade kind — subjects survive, but the relation is cleared.
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
            world.target_of(item, Owns),
            None,
            "a relation must not outlive its target"
        );
    }

    /// C-2/C-3: a reused index does not return foreign relations.
    #[test]
    fn reused_index_does_not_leak_relations() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn((Position { x: 1.0, y: 0.0 },));
        world.add_relation(child, ChildOf, parent);

        world.despawn(parent); // cascade: the child died too

        // New entities reuse both freed indexes
        let n1 = world.spawn((Position { x: 9.0, y: 9.0 },));
        let n2 = world.spawn((Position { x: 8.0, y: 8.0 },));
        let newcomer = [n1, n2]
            .into_iter()
            .find(|e| e.index() == parent.index())
            .expect("the test requires reuse of the parent's index");

        assert_eq!(world.targets_of(ChildOf, newcomer).count(), 0);
        // A stale handle of the old parent also returns nothing
        assert_eq!(world.targets_of(ChildOf, parent).count(), 0);
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
        let mut children: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
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
            .query_relation::<ChildOf, &Position>(ChildOf, parent)
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
            .query_wildcard::<ChildOf, &Position>(ChildOf)
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
        assert_eq!(world.target_of(child, ChildOf), None);
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
        assert_eq!(world.target_of(child, ChildOf), Some(p1));

        world.add_relation(child, ChildOf, p2); // exclusive → replaces p1
        assert_eq!(world.target_of(child, ChildOf), Some(p2));
        assert_eq!(world.targets_of(ChildOf, p1).count(), 0, "old parent lost the child");
        assert_eq!(world.targets_of(ChildOf, p2).count(), 1, "new parent gained it");
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
        assert_eq!(world.target_of(a, ChildOf), None);
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
        assert_eq!(world.target_of(c, ChildOf), None, "cycle edge rejected");
        assert_eq!(world.target_of(a, ChildOf), Some(b), "existing edges intact");
        assert_eq!(world.target_of(b, ChildOf), Some(c));
    }

    // ── Sibling order (core ADR-008) ───────────────────────────

    /// Children are yielded in insertion order.
    #[test]
    fn children_order_is_insertion_order() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let kids: Vec<Entity> = (0..5)
            .map(|i| {
                let c = world.spawn((Position { x: i as f32, y: 0.0 },));
                world.add_relation(c, ChildOf, parent);
                c
            })
            .collect();
        let seen: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(seen, kids, "targets_of yields insertion order");
    }

    /// Removing a middle child preserves the order of the remaining siblings
    /// (the historical swap_remove teleported the last sibling into the hole).
    #[test]
    fn children_order_stable_across_removal() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let kids: Vec<Entity> = (0..5)
            .map(|i| {
                let c = world.spawn((Position { x: i as f32, y: 0.0 },));
                world.add_relation(c, ChildOf, parent);
                c
            })
            .collect();

        // Remove the middle child via remove_relation…
        world.remove_relation(kids[2], ChildOf, parent);
        let seen: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(seen, vec![kids[0], kids[1], kids[3], kids[4]]);

        // …and another via despawn (index cleanup path).
        world.despawn(kids[3]);
        let seen: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(seen, vec![kids[0], kids[1], kids[4]]);
    }

    /// insert_relation_at positions a NEW child; out-of-range clamps to append.
    #[test]
    fn insert_relation_at_positions_child() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let a = world.spawn((Position { x: 1.0, y: 0.0 },));
        let b = world.spawn((Position { x: 2.0, y: 0.0 },));
        let c = world.spawn((Position { x: 3.0, y: 0.0 },));
        let d = world.spawn((Position { x: 4.0, y: 0.0 },));

        world.add_relation(a, ChildOf, parent);
        world.add_relation(b, ChildOf, parent);
        world.insert_relation_at(c, ChildOf, parent, 0); // head
        world.insert_relation_at(d, ChildOf, parent, 99); // clamp → append
        let seen: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(seen, vec![c, a, b, d]);
        assert_eq!(world.relation_index(c, ChildOf, parent), Some(0));
        assert_eq!(world.relation_index(d, ChildOf, parent), Some(3));
    }

    /// insert_relation_at on an EXISTING pair is a pure reorder (ensure-at).
    #[test]
    fn insert_relation_at_existing_reorders() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let kids: Vec<Entity> = (0..3)
            .map(|i| {
                let c = world.spawn((Position { x: i as f32, y: 0.0 },));
                world.add_relation(c, ChildOf, parent);
                c
            })
            .collect();
        world.insert_relation_at(kids[2], ChildOf, parent, 0);
        let seen: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(seen, vec![kids[2], kids[0], kids[1]]);
    }

    /// set_relation_index reorders and reports honestly.
    #[test]
    fn set_relation_index_reorders() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn((Position { x: 0.0, y: 0.0 },));
        let kids: Vec<Entity> = (0..4)
            .map(|i| {
                let c = world.spawn((Position { x: i as f32, y: 0.0 },));
                world.add_relation(c, ChildOf, parent);
                c
            })
            .collect();

        assert!(world.set_relation_index(kids[0], ChildOf, parent, 3)); // head → tail
        assert!(world.set_relation_index(kids[3], ChildOf, parent, 0)); // (now-)tail… → head
        let seen: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(seen, vec![kids[3], kids[1], kids[2], kids[0]]);

        // Not related / dead → false, never creates an edge.
        let stranger = world.spawn((Position { x: 9.0, y: 0.0 },));
        assert!(!world.set_relation_index(stranger, ChildOf, parent, 0));
        assert_eq!(world.target_of(stranger, ChildOf), None);
    }

    /// An exclusive re-parent APPENDS to the new parent's sibling list;
    /// insert_relation_at re-parents AT the requested position.
    #[test]
    fn reparent_order_semantics() {
        let mut world = World::new();
        world.register_component::<Position>();
        let p1 = world.spawn((Position { x: 0.0, y: 0.0 },));
        let p2 = world.spawn((Position { x: 0.0, y: 1.0 },));
        let a = world.spawn((Position { x: 1.0, y: 0.0 },));
        let b = world.spawn((Position { x: 2.0, y: 0.0 },));
        let c = world.spawn((Position { x: 3.0, y: 0.0 },));
        world.add_relation(a, ChildOf, p2);
        world.add_relation(b, ChildOf, p1);
        world.add_relation(c, ChildOf, p1);

        // Plain re-parent (b: p1 → p2) appends after a.
        world.add_relation(b, ChildOf, p2);
        let seen: Vec<Entity> = world.targets_of(ChildOf, p2).collect();
        assert_eq!(seen, vec![a, b]);

        // Positioned re-parent (c: p1 → p2 at head).
        world.insert_relation_at(c, ChildOf, p2, 0);
        let seen: Vec<Entity> = world.targets_of(ChildOf, p2).collect();
        assert_eq!(seen, vec![c, a, b]);
        assert_eq!(world.targets_of(ChildOf, p1).count(), 0);
    }

    /// Target-major iteration is deterministic and preserves sibling order
    /// (the snapshot emission contract).
    #[test]
    fn iter_relations_target_major_order() {
        let mut world = World::new();
        world.register_component::<Position>();
        let p1 = world.spawn((Position { x: 0.0, y: 0.0 },));
        let p2 = world.spawn((Position { x: 0.0, y: 1.0 },));
        let a = world.spawn((Position { x: 1.0, y: 0.0 },));
        let b = world.spawn((Position { x: 2.0, y: 0.0 },));
        let c = world.spawn((Position { x: 3.0, y: 0.0 },));
        world.add_relation(b, ChildOf, p2);
        world.add_relation(a, ChildOf, p2);
        world.insert_relation_at(c, ChildOf, p2, 1);
        world.add_relation(p2, ChildOf, p1);

        let rels = world.iter_relations_target_major();
        // Targets ascending: p1 (its child p2), then p2 (children b, c, a).
        let expected: Vec<(Entity, u32)> = vec![
            (p2, p1.index()),
            (b, p2.index()),
            (c, p2.index()),
            (a, p2.index()),
        ];
        let seen: Vec<(Entity, u32)> = rels.iter().map(|&(s, _k, t)| (s, t)).collect();
        assert_eq!(seen, expected);
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
        // Removing one target clears exactly that one
        world.despawn(targets[7]);
        assert!(!world.has_relation(hub, Likes, targets[7]));
        let alive = targets.iter().filter(|&&t| world.has_relation(hub, Likes, t)).count();
        assert_eq!(alive, 19);
    }
}
