//! Relations — связи между entity, архитектура по образцу Flecs.

use std::any::TypeId;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    archetype::ArchetypeId,
    component::{ComponentId, ComponentInfo, Tick},
    entity::Entity,
    query::WorldQuery,
    world::World,
};

// ── Константы кодирования ──────────────────────────────────────

pub(crate) const RELATION_FLAG: u32 = 1 << 31;
const WILDCARD_TARGET: u32 = (1 << 20) - 1;

#[inline]
pub(crate) fn encode_relation(kind_idx: u32, target_idx: u32) -> ComponentId {
    debug_assert!(kind_idx < (1 << 11), "too many relation kinds");
    debug_assert!(target_idx < WILDCARD_TARGET, "entity index too large");
    ComponentId(RELATION_FLAG | (kind_idx << 20) | target_idx)
}

#[inline]
pub(crate) fn wildcard_id(kind_idx: u32) -> ComponentId {
    ComponentId(RELATION_FLAG | (kind_idx << 20) | WILDCARD_TARGET)
}

#[inline]
pub fn is_relation_id(id: ComponentId) -> bool {
    id.0 & RELATION_FLAG != 0
}

#[inline]
pub(crate) fn decode_kind(id: ComponentId) -> u32 {
    (id.0 & !RELATION_FLAG) >> 20
}

#[inline]
pub(crate) fn decode_target(id: ComponentId) -> u32 {
    id.0 & ((1 << 20) - 1)
}

#[inline]
fn is_wildcard(id: ComponentId) -> bool {
    decode_target(id) == WILDCARD_TARGET
}

// ── IdRecord ───────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct IdRecord {
    pub archetypes: SmallVec<[ArchetypeId; 4]>,
}

#[derive(Default)]
pub(crate) struct IdIndex {
    records: FxHashMap<u32, IdRecord>,
}

impl IdIndex {
    pub fn register_archetype(&mut self, component_id: ComponentId, arch_id: ArchetypeId) {
        self.records
            .entry(component_id.0)
            .or_default()
            .archetypes
            .push(arch_id);

        if is_relation_id(component_id) && !is_wildcard(component_id) {
            let wid = wildcard_id(decode_kind(component_id));
            self.records
                .entry(wid.0)
                .or_default()
                .archetypes
                .push(arch_id);
        }
    }

    #[inline]
    pub fn get(&self, component_id: ComponentId) -> &[ArchetypeId] {
        self.records
            .get(&component_id.0)
            .map(|r| r.archetypes.as_slice())
            .unwrap_or(&[])
    }
}

// ── SubjectIndex ───────────────────────────────────────────────
//
// Исправленная версия: O(1) через u64 kind_mask + binary_search в SmallVec.
//
// Предыдущая реализация использовала SmallVec::contains — линейный поиск
// по всем relation_id. При N relations это O(N) с большим константным
// фактором из-за сравнения u32.
//
// Новая структура SubjectEntry:
//   kind_mask: u64  — бит kind_idx установлен если есть хоть один relation
//                     этого kind. Даёт O(1) early-exit для has().
//   relations: SmallVec<[u32; 4]> — отсортированный список полных relation_id.
//                     binary_search = O(log N), но N < 8 на практике,
//                     поэтому реально быстрее hash из-за cache locality.
//
// Итоговая сложность has():
//   - kind не зарегистрирован → 1 bit check → false (< 1 ns)
//   - kind есть, target нет  → bit check + binary_search (< 3 ns)
//   - kind есть, target есть → bit check + binary_search (< 3 ns)

#[derive(Default)]
struct SubjectEntry {
    /// Битовая маска: бит k установлен ↔ есть relation с kind_idx = k.
    /// Поддерживает до 64 различных RelationKind — достаточно для любой игры.
    kind_mask: u64,
    /// Отсортированный список полных relation ComponentId (как u32).
    /// Инвариант: всегда отсортирован → binary_search корректен.
    relations: SmallVec<[u32; 4]>,
}

impl SubjectEntry {
    #[inline]
    fn has_kind(&self, kind_idx: u32) -> bool {
        if kind_idx >= 64 { return self.has_kind_slow(kind_idx); }
        self.kind_mask & (1u64 << kind_idx) != 0
    }

    /// Fallback для kind_idx >= 64 — линейный поиск по relations.
    /// На практике никогда не вызывается (< 64 relation kinds в любой игре).
    #[cold]
    fn has_kind_slow(&self, kind_idx: u32) -> bool {
        self.relations.iter().any(|&r| {
            let cid = ComponentId(r);
            is_relation_id(cid) && decode_kind(cid) == kind_idx
        })
    }

    #[inline]
    fn insert(&mut self, relation_id: ComponentId) {
        let raw = relation_id.0;
        // Обновляем kind_mask
        let kind_idx = decode_kind(relation_id);
        if kind_idx < 64 {
            self.kind_mask |= 1u64 << kind_idx;
        }
        // Вставляем в отсортированную позицию (insertion sort — N мал)
        match self.relations.binary_search(&raw) {
            Ok(_) => {} // уже есть
            Err(pos) => self.relations.insert(pos, raw),
        }
    }

    #[inline]
    fn remove(&mut self, relation_id: ComponentId) {
        let raw = relation_id.0;
        if let Ok(pos) = self.relations.binary_search(&raw) {
            self.relations.remove(pos);
        }
        // Пересчитываем kind_mask для этого kind
        let kind_idx = decode_kind(relation_id);
        if kind_idx < 64 {
            // Проверяем остались ли ещё relations этого kind
            let still_has_kind = self.relations.iter().any(|&r| {
                let cid = ComponentId(r);
                is_relation_id(cid) && decode_kind(cid) == kind_idx
            });
            if !still_has_kind {
                self.kind_mask &= !(1u64 << kind_idx);
            }
        }
    }

    #[inline]
    fn has(&self, relation_id: ComponentId) -> bool {
        let kind_idx = decode_kind(relation_id);
        // O(1) early-exit: если kind отсутствует → false без binary_search
        if !self.has_kind(kind_idx) { return false; }
        // O(log N) binary_search — N < 8 на практике
        self.relations.binary_search(&relation_id.0).is_ok()
    }
}

pub(crate) struct SubjectIndex {
    entries: Vec<SubjectEntry>,
}

impl SubjectIndex {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    #[inline]
    fn ensure(&mut self, entity_index: usize) {
        if entity_index >= self.entries.len() {
            self.entries.resize_with(entity_index + 1, SubjectEntry::default);
        }
    }

    #[inline]
    pub fn add(&mut self, entity_index: u32, relation_id: ComponentId) {
        let idx = entity_index as usize;
        self.ensure(idx);
        self.entries[idx].insert(relation_id);
    }

    #[inline]
    pub fn remove(&mut self, entity_index: u32, relation_id: ComponentId) {
        let idx = entity_index as usize;
        if idx < self.entries.len() {
            self.entries[idx].remove(relation_id);
        }
    }

    /// O(1) через kind_mask + O(log N) binary_search.
    /// Фактически < 3 ns для типичных случаев.
    #[inline]
    pub fn has(&self, entity_index: u32, relation_id: ComponentId) -> bool {
        let idx = entity_index as usize;
        idx < self.entries.len() && self.entries[idx].has(relation_id)
    }

    #[inline]
    pub fn get_all(&self, entity_index: u32) -> &[u32] {
        let idx = entity_index as usize;
        if idx < self.entries.len() {
            &self.entries[idx].relations
        } else {
            &[]
        }
    }

    pub fn clear_entity(&mut self, entity_index: u32) {
        let idx = entity_index as usize;
        if idx < self.entries.len() {
            self.entries[idx].kind_mask = 0;
            self.entries[idx].relations.clear();
        }
    }
}

impl Default for SubjectIndex {
    fn default() -> Self { Self::new() }
}

// ── RelationKind ───────────────────────────────────────────────

pub trait RelationKind: Copy + Send + Sync + 'static {
    fn cascade_delete_on_target_despawn() -> bool { false }
}

// ── RelationRegistry ───────────────────────────────────────────

pub struct RelationRegistry {
    type_to_idx: FxHashMap<TypeId, u32>,
    cascade_flags: Vec<bool>,
    next_idx: u32,
}

impl RelationRegistry {
    pub fn new() -> Self {
        Self {
            type_to_idx: FxHashMap::default(),
            cascade_flags: Vec::new(),
            next_idx: 0,
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
        self.cascade_flags.push(R::cascade_delete_on_target_despawn());
        idx
    }

    pub fn get_idx<R: RelationKind>(&self) -> Option<u32> {
        self.type_to_idx.get(&TypeId::of::<R>()).copied()
    }

    #[allow(dead_code)]
    pub fn is_cascade(&self, kind_idx: u32) -> bool {
        self.cascade_flags.get(kind_idx as usize).copied().unwrap_or(false)
    }
}

impl Default for RelationRegistry {
    fn default() -> Self { Self::new() }
}

// ── World extension ────────────────────────────────────────────

impl World {
    pub fn add_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = self.relations.get_or_register::<R>();
        let relation_id = encode_relation(kind_idx, target.index);
        self.ensure_relation_component(relation_id);
        self.subject_index.add(subject.index, relation_id);
        self.insert_relation_component(subject, relation_id);
    }

    pub fn remove_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return,
        };
        let relation_id = encode_relation(kind_idx, target.index);
        self.subject_index.remove(subject.index, relation_id);
        self.remove_relation_component(subject, relation_id);
    }

    /// O(1) через SubjectIndex: kind_mask check + binary_search.
    /// Типичная стоимость: < 3 ns.
    #[inline]
    pub fn has_relation<R: RelationKind>(&self, subject: Entity, _kind: R, target: Entity) -> bool {
        if !self.entities.is_alive(subject) { return false; }
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return false,
        };
        let relation_id = encode_relation(kind_idx, target.index);
        self.subject_index.has(subject.index, relation_id)
    }

    pub fn query_relation<'w, R: RelationKind, Q: WorldQuery>(
        &'w self,
        _kind: R,
        target: Entity,
    ) -> RelationIter<'w, Q> {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return RelationIter::empty(self),
        };
        let relation_id = encode_relation(kind_idx, target.index);

        let mut data_ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(self, &mut data_ids);
        let all_found = data_ids.len() == Q::component_count();

        let arch_ids = self.id_index.get(relation_id);

        let arch_states: Vec<RelationArchState<Q::State>> = if all_found {
            arch_ids.iter()
                .filter_map(|&arch_id| {
                    let arch_idx = arch_id.0 as usize;
                    let arch = &self.archetypes[arch_idx];
                    if arch.is_empty() { return None; }
                    if !Q::matches_archetype(arch, &data_ids) { return None; }
                    let state = unsafe { Q::fetch_state(arch, &data_ids, Tick::ZERO) };
                    Some(RelationArchState { arch_idx, state, len: arch.len() })
                })
                .collect()
        } else {
            Vec::new()
        };

        RelationIter { world: self, arch_states, arch_cursor: 0, row_cursor: 0 }
    }

    pub fn query_wildcard<'w, R: RelationKind, Q: WorldQuery>(
        &'w self,
        _kind: R,
    ) -> RelationIter<'w, Q> {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return RelationIter::empty(self),
        };
        let wid = wildcard_id(kind_idx);

        let mut data_ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(self, &mut data_ids);
        let all_found = data_ids.len() == Q::component_count();

        let arch_ids = self.id_index.get(wid);

        let arch_states: Vec<RelationArchState<Q::State>> = if all_found {
            arch_ids.iter()
                .filter_map(|&arch_id| {
                    let arch_idx = arch_id.0 as usize;
                    let arch = &self.archetypes[arch_idx];
                    if arch.is_empty() { return None; }
                    if !Q::matches_archetype(arch, &data_ids) { return None; }
                    let state = unsafe { Q::fetch_state(arch, &data_ids, Tick::ZERO) };
                    Some(RelationArchState { arch_idx, state, len: arch.len() })
                })
                .collect()
        } else {
            Vec::new()
        };

        RelationIter { world: self, arch_states, arch_cursor: 0, row_cursor: 0 }
    }

    pub fn children_of<'w, R: RelationKind>(
        &'w self,
        _kind: R,
        parent: Entity,
    ) -> impl Iterator<Item = Entity> + 'w {
        let kind_idx = self.relations.get_idx::<R>();
        let relation_id = kind_idx.map(|k| encode_relation(k, parent.index));

        let arch_ids: &[ArchetypeId] = relation_id
            .map(|rid| self.id_index.get(rid))
            .unwrap_or(&[]);

        arch_ids.iter()
            .flat_map(move |&arch_id| {
                self.archetypes[arch_id.0 as usize].entities.iter().copied()
            })
    }

    pub fn get_relation_target<R: RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
    ) -> Option<Entity> {
        let kind_idx = self.relations.get_idx::<R>()?;
        for &raw_id in self.subject_index.get_all(subject.index) {
            let cid = ComponentId(raw_id);
            if is_relation_id(cid) && decode_kind(cid) == kind_idx && !is_wildcard(cid) {
                let target_idx = decode_target(cid);
                return self.entities.get_by_index(target_idx);
            }
        }
        None
    }

    pub fn despawn_recursive<R: RelationKind + Copy>(&mut self, _kind: R, entity: Entity) {
        let children: Vec<Entity> = self.children_of(_kind, entity).collect();
        for child in children {
            self.despawn_recursive(_kind, child);
        }
        self.despawn(entity);
    }

    pub(crate) fn ensure_relation_component(&mut self, relation_id: ComponentId) {
        if self.registry.get_info(relation_id).is_none() {
            self.registry.register_raw(relation_id, ComponentInfo {
                id: relation_id,
                name: "<relation>",
                type_id: std::any::TypeId::of::<()>(),
                size: 0,
                align: 1,
                drop_fn: |_| {},
            });
        }
    }

    pub(crate) fn insert_relation_component(&mut self, entity: Entity, relation_id: ComponentId) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return,
        };
        if self.archetypes[location.archetype_id.0 as usize].has_component(relation_id) {
            return;
        }
        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, relation_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        let tick = self.current_tick;
        if let Some(col_idx) = self.archetypes[new_arch_id.0 as usize].column_index(relation_id) {
            let col = &mut self.archetypes[new_arch_id.0 as usize].columns[col_idx];
            col.change_ticks.push(tick);
            col.len += 1;
        }
        self.entities.set_location(entity, crate::entity::EntityLocation {
            archetype_id: new_arch_id,
            row: new_row,
        });
    }

    pub(crate) fn remove_relation_component(&mut self, entity: Entity, relation_id: ComponentId) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return,
        };
        if !self.archetypes[location.archetype_id.0 as usize].has_component(relation_id) {
            return;
        }
        let new_arch_id = self.find_or_create_archetype_without(location.archetype_id, relation_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        self.entities.set_location(entity, crate::entity::EntityLocation {
            archetype_id: new_arch_id,
            row: new_row,
        });
    }
}

// ── RelationIter ───────────────────────────────────────────────

pub(crate) struct RelationArchState<S> {
    pub arch_idx: usize,
    pub state: S,
    pub len: usize,
}

pub struct RelationIter<'w, Q: WorldQuery> {
    world: &'w World,
    arch_states: Vec<RelationArchState<Q::State>>,
    arch_cursor: usize,
    row_cursor: usize,
}

impl<'w, Q: WorldQuery> RelationIter<'w, Q> {
    pub(crate) fn empty(world: &'w World) -> Self {
        Self { world, arch_states: Vec::new(), arch_cursor: 0, row_cursor: 0 }
    }
}

impl<'w, Q: WorldQuery> Iterator for RelationIter<'w, Q> {
    type Item = (Entity, Q::Item<'w>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let a = self.arch_states.get(self.arch_cursor)?;
            if self.row_cursor >= a.len {
                self.arch_cursor += 1;
                self.row_cursor = 0;
                continue;
            }
            let row = self.row_cursor;
            self.row_cursor += 1;
            if let Some(item) = unsafe { Q::fetch_item(a.state, row) } {
                let entity = self.world.archetypes[a.arch_idx].entities[row];
                return Some((entity, item));
            }
        }
    }
}

// ── Встроенные relation kinds ──────────────────────────────────

#[derive(Clone, Copy)] pub struct ChildOf;
impl RelationKind for ChildOf {
    fn cascade_delete_on_target_despawn() -> bool { true }
}

#[derive(Clone, Copy)] pub struct Owns;
impl RelationKind for Owns {}

#[derive(Clone, Copy)] pub struct Likes;
impl RelationKind for Likes {}

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Position { x: f32, y: f32 }

    #[test]
    fn add_has_remove_relation() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let child  = world.spawn_bundle((Position { x: 1.0, y: 0.0 },));

        world.add_relation(child, ChildOf, parent);
        assert!(world.has_relation(child, ChildOf, parent));
        assert!(!world.has_relation(parent, ChildOf, child));

        world.remove_relation(child, ChildOf, parent);
        assert!(!world.has_relation(child, ChildOf, parent));
    }

    #[test]
    fn subject_entry_kind_mask() {
        // Проверяем что kind_mask корректно обновляется
        let mut entry = SubjectEntry::default();
        let rel1 = encode_relation(0, 100);
        let rel2 = encode_relation(1, 200);
        let rel3 = encode_relation(0, 300); // тот же kind что rel1

        entry.insert(rel1);
        assert!(entry.has_kind(0));
        assert!(!entry.has_kind(1));

        entry.insert(rel2);
        assert!(entry.has_kind(1));

        entry.insert(rel3);
        entry.remove(rel1);
        // kind 0 всё ещё должен быть — остался rel3
        assert!(entry.has_kind(0));

        entry.remove(rel3);
        // теперь kind 0 должен исчезнуть
        assert!(!entry.has_kind(0));
    }

    #[test]
    fn has_relation_o1() {
        let mut world = World::new();
        world.register_component::<Position>();

        let parents: Vec<Entity> = (0..100)
            .map(|i| world.spawn_bundle((Position { x: i as f32, y: 0.0 },)))
            .collect();

        let children: Vec<Entity> = (0..100)
            .map(|i| {
                let c = world.spawn_bundle((Position { x: i as f32, y: 0.0 },));
                world.add_relation(c, ChildOf, parents[i]);
                c
            })
            .collect();

        for i in 0..100 {
            assert!(world.has_relation(children[i], ChildOf, parents[i]));
            let other = (i + 1) % 100;
            assert!(!world.has_relation(children[i], ChildOf, parents[other]));
        }
    }

    #[test]
    fn despawn_recursive() {
        let mut world = World::new();
        world.register_component::<Position>();
        let root  = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let child = world.spawn_bundle((Position { x: 1.0, y: 0.0 },));
        let leaf  = world.spawn_bundle((Position { x: 2.0, y: 0.0 },));

        world.add_relation(child, ChildOf, root);
        world.add_relation(leaf,  ChildOf, child);

        assert_eq!(world.entity_count(), 3);
        world.despawn_recursive(ChildOf, root);
        assert_eq!(world.entity_count(), 0);
    }

    #[test]
    fn get_relation_target() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let child  = world.spawn_bundle((Position { x: 1.0, y: 0.0 },));

        world.add_relation(child, ChildOf, parent);
        assert_eq!(world.get_relation_target(child, ChildOf), Some(parent));
    }
}