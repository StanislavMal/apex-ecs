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

// ── SubjectIndex — НОВЫЙ: entity-level обратный индекс ────────
//
// Проблема: has_relation делает get_location → archetype lookup.
// При 10k архетипов это O(1) но с большим константным фактором
// (FxHashMap lookup в column_map).
//
// Решение: храним для каждого entity множество его relation_id.
// has_relation → SubjectIndex lookup → O(1) битовая проверка.
//
// subject_index[entity.index] = set of active relation ComponentIds

pub(crate) struct SubjectIndex {
    /// entity_index → список relation ComponentId которые есть у entity
    /// SmallVec<[u32; 4]> — большинство entity имеют < 4 relations
    relations: Vec<SmallVec<[u32; 4]>>,
}

impl SubjectIndex {
    pub fn new() -> Self {
        Self { relations: Vec::new() }
    }

    fn ensure(&mut self, entity_index: usize) {
        if entity_index >= self.relations.len() {
            self.relations.resize_with(entity_index + 1, SmallVec::new);
        }
    }

    #[inline]
    pub fn add(&mut self, entity_index: u32, relation_id: ComponentId) {
        let idx = entity_index as usize;
        self.ensure(idx);
        let slot = &mut self.relations[idx];
        if !slot.contains(&relation_id.0) {
            slot.push(relation_id.0);
        }
    }

    #[inline]
    pub fn remove(&mut self, entity_index: u32, relation_id: ComponentId) {
        let idx = entity_index as usize;
        if idx < self.relations.len() {
            self.relations[idx].retain(|id| *id != relation_id.0);
        }
    }

    /// O(N) где N = кол-во relations у entity (обычно < 4) → фактически O(1)
    #[inline]
    pub fn has(&self, entity_index: u32, relation_id: ComponentId) -> bool {
        let idx = entity_index as usize;
        idx < self.relations.len()
            && self.relations[idx].contains(&relation_id.0)
    }

    // Получить все relation_id для entity (для get_relation_target)
    #[inline]
    pub fn get_all(&self, entity_index: u32) -> &[u32] {
        let idx = entity_index as usize;
        if idx < self.relations.len() {
            &self.relations[idx]
        } else {
            &[]
        }
    }

    pub fn clear_entity(&mut self, entity_index: u32) {
        let idx = entity_index as usize;
        if idx < self.relations.len() {
            self.relations[idx].clear();
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
    /// Добавить relation `(R, target)` к entity `subject`.
    pub fn add_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = self.relations.get_or_register::<R>();
        let relation_id = encode_relation(kind_idx, target.index);
        self.ensure_relation_component(relation_id);

        // Обновляем SubjectIndex ДО structural change
        self.subject_index.add(subject.index, relation_id);

        self.insert_relation_component(subject, relation_id);
    }

    /// Удалить relation `(R, target)` у entity `subject`.
    pub fn remove_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return,
        };
        let relation_id = encode_relation(kind_idx, target.index);

        // Убираем из SubjectIndex
        self.subject_index.remove(subject.index, relation_id);

        self.remove_relation_component(subject, relation_id);
    }

    /// Проверить наличие relation `(R, target)` у entity `subject`.
    /// O(1) через SubjectIndex — не зависит от числа архетипов.
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

    // ── Query по конкретному target ────────────────────────────

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

    /// Wildcard query: все entity у которых есть хоть какой-то `R` relation.
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

    /// Прямые дочерние entity. O(1) lookup через IdIndex.
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

    /// Получить target entity для relation `(R, ?)` у subject.
    /// O(1) через SubjectIndex.
    pub fn get_relation_target<R: RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
    ) -> Option<Entity> {
        let kind_idx = self.relations.get_idx::<R>()?;
        // Ищем в SubjectIndex — не трогаем архетипы вообще
        for &raw_id in self.subject_index.get_all(subject.index) {
            let cid = ComponentId(raw_id);
            if is_relation_id(cid) && decode_kind(cid) == kind_idx && !is_wildcard(cid) {
                let target_idx = decode_target(cid);
                return self.entities.get_by_index(target_idx);
            }
        }
        None
    }

    /// Каскадный despawn рекурсивно.
    pub fn despawn_recursive<R: RelationKind + Copy>(&mut self, _kind: R, entity: Entity) {
        let children: Vec<Entity> = self.children_of(_kind, entity).collect();
        for child in children {
            self.despawn_recursive(_kind, child);
        }
        self.despawn(entity);
    }

    // ── Внутренние методы ──────────────────────────────────────

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
        // ZST relation — только тик, данных нет
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

#[derive(Clone, Copy)]
pub struct ChildOf;
impl RelationKind for ChildOf {
    fn cascade_delete_on_target_despawn() -> bool { true }
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
    fn query_relation_o1() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let c1 = world.spawn_bundle((Position { x: 1.0, y: 0.0 },));
        let c2 = world.spawn_bundle((Position { x: 2.0, y: 0.0 },));
        let other_parent = world.spawn_bundle((Position { x: 99.0, y: 0.0 },));
        let c3 = world.spawn_bundle((Position { x: 3.0, y: 0.0 },));

        world.add_relation(c1, ChildOf, parent);
        world.add_relation(c2, ChildOf, parent);
        world.add_relation(c3, ChildOf, other_parent);

        let children: Vec<Entity> = world
            .query_relation::<ChildOf, Read<Position>>(ChildOf, parent)
            .map(|(e, _)| e)
            .collect();

        assert_eq!(children.len(), 2);
        assert!(children.contains(&c1));
        assert!(children.contains(&c2));
        assert!(!children.contains(&c3));
    }

    #[test]
    fn wildcard_query() {
        let mut world = World::new();
        world.register_component::<Position>();
        let p1 = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let p2 = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let c1 = world.spawn_bundle((Position { x: 1.0, y: 0.0 },));
        let c2 = world.spawn_bundle((Position { x: 2.0, y: 0.0 },));
        let standalone = world.spawn_bundle((Position { x: 9.0, y: 0.0 },));

        world.add_relation(c1, ChildOf, p1);
        world.add_relation(c2, ChildOf, p2);

        let all_children: Vec<Entity> = world
            .query_wildcard::<ChildOf, Read<Position>>(ChildOf)
            .map(|(e, _)| e)
            .collect();

        assert_eq!(all_children.len(), 2);
        assert!(all_children.contains(&c1));
        assert!(all_children.contains(&c2));
        assert!(!all_children.contains(&standalone));
/*************  ✨ Windsurf Command ⭐  *************/
// Recursively despawns all entities that have a given relation (e.g. ChildOf)
// with the given entity, and all their children, and so on.
//
// This is useful for cleaning up complex entity hierarchies.
//
// # Example
//
/*******  820ec2b9-4631-4537-b89a-c2ca812ae617  *******/    }

    #[test]
    fn children_of_o1() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let c1 = world.spawn_bundle((Position { x: 1.0, y: 0.0 },));
        let c2 = world.spawn_bundle((Position { x: 2.0, y: 0.0 },));

        world.add_relation(c1, ChildOf, parent);
        world.add_relation(c2, ChildOf, parent);

        let children: Vec<Entity> = world.children_of(ChildOf, parent).collect();
        assert_eq!(children.len(), 2);
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
    fn has_relation_fast_path() {
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

        // has_relation через SubjectIndex — O(1)
        for i in 0..100 {
            assert!(world.has_relation(children[i], ChildOf, parents[i]));
            // Неправильный parent — должно быть false
            let other = (i + 1) % 100;
            assert!(!world.has_relation(children[i], ChildOf, parents[other]));
        }
    }

    #[test]
    fn get_relation_target() {
        let mut world = World::new();
        world.register_component::<Position>();
        let parent = world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let child  = world.spawn_bundle((Position { x: 1.0, y: 0.0 },));

        world.add_relation(child, ChildOf, parent);

        let target = world.get_relation_target(child, ChildOf);
        assert_eq!(target, Some(parent));
    }
}