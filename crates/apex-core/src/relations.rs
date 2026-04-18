//! Relations — связи между entity, архитектура по образцу Flecs.
//!
//! ## Ключевые идеи из Flecs
//!
//! 1. **Id record (обратный индекс)** — каждый ComponentId (включая relation_id)
//!    имеет запись `IdRecord { archetypes: Vec<ArchetypeId> }`. При создании
//!    архетипа с relation_id он немедленно регистрируется в этом индексе.
//!    Это делает `query_relation` O(1) вместо O(archetypes).
//!
//! 2. **Wildcard** — `query_wildcard::<ChildOf>()` находит все entity у которых
//!    есть хоть какой-то ChildOf, независимо от target. Реализуется через
//!    отдельный wildcard_id = `RELATION_FLAG | (kind_idx << 20) | WILDCARD_TARGET`.
//!
//! 3. **Каскадный despawn** — при `despawn(parent)` все entity с `ChildOf(parent)`
//!    тоже despawn'ятся рекурсивно. Как `OnDeleteTarget::Delete` в Flecs.
//!
//! 4. **RelationData<R, D>** — relation может нести данные (не только ZST).
//!    Например `Distance(f32)` между двумя entity.

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

/// Старший бит = relation flag
pub(crate) const RELATION_FLAG: u32 = 1 << 31;

/// Зарезервированный target для wildcard запросов
/// `query_wildcard::<ChildOf>()` → ищет ChildOf(*)
const WILDCARD_TARGET: u32 = (1 << 20) - 1; // все биты target = 1

/// Кодирует (kind_idx, target_entity_index) → ComponentId
#[inline]
pub(crate) fn encode_relation(kind_idx: u32, target_idx: u32) -> ComponentId {
    debug_assert!(kind_idx < (1 << 11), "too many relation kinds (max 2048)");
    debug_assert!(target_idx < WILDCARD_TARGET, "entity index too large for relation");
    ComponentId(RELATION_FLAG | (kind_idx << 20) | target_idx)
}

/// Wildcard ComponentId для данного kind — матчит любой target
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

// ── IdRecord — обратный индекс ─────────────────────────────────

/// Запись для одного ComponentId: список архетипов которые его содержат.
/// Это ключевая структура из Flecs — делает query O(1).
#[derive(Default)]
pub(crate) struct IdRecord {
    /// Архетипы содержащие этот ComponentId (точный match)
    pub archetypes: SmallVec<[ArchetypeId; 4]>,
}

/// Глобальный обратный индекс: ComponentId → IdRecord
#[derive(Default)]
pub(crate) struct IdIndex {
    records: FxHashMap<u32, IdRecord>,
}

impl IdIndex {
    /// Зарегистрировать архетип для данного ComponentId.
    /// Вызывается при создании нового архетипа.
    pub fn register_archetype(&mut self, component_id: ComponentId, arch_id: ArchetypeId) {
        self.records
            .entry(component_id.0)
            .or_default()
            .archetypes
            .push(arch_id);

        // Если это relation — также регистрируем в wildcard записи
        if is_relation_id(component_id) && !is_wildcard(component_id) {
            let wid = wildcard_id(decode_kind(component_id));
            self.records
                .entry(wid.0)
                .or_default()
                .archetypes
                .push(arch_id);
        }
    }

    /// Получить список архетипов для ComponentId. O(1).
    #[inline]
    pub fn get(&self, component_id: ComponentId) -> &[ArchetypeId] {
        self.records
            .get(&component_id.0)
            .map(|r| r.archetypes.as_slice())
            .unwrap_or(&[])
    }

    /// Удалить архетип из всех записей (при инвалидации — не используется,
    /// архетипы никогда не удаляются, только растут).
    #[allow(dead_code)]
    pub fn remove_archetype(&mut self, component_id: ComponentId, arch_id: ArchetypeId) {
        if let Some(record) = self.records.get_mut(&component_id.0) {
            record.archetypes.retain(|id| *id != arch_id);
        }
    }
}

// ── RelationKind trait ─────────────────────────────────────────

/// Маркер типа связи. Реализуется для unit-struct'ов.
///
/// ```ignore
/// struct ChildOf;
/// impl RelationKind for ChildOf {}
/// ```
pub trait RelationKind: Copy + Send + Sync + 'static {
    /// Если true — при despawn target каскадно despawn'ятся все subjects.
    /// Аналог `OnDeleteTarget::Delete` в Flecs.
    fn cascade_delete_on_target_despawn() -> bool { false }
}

// ── RelationRegistry ───────────────────────────────────────────

pub struct RelationRegistry {
    type_to_idx: FxHashMap<TypeId, u32>,
    /// Флаги каскадного удаления по kind_idx
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

    pub fn is_cascade(&self, kind_idx: u32) -> bool {
        self.cascade_flags.get(kind_idx as usize).copied().unwrap_or(false)
    }
}

impl Default for RelationRegistry {
    fn default() -> Self { Self::new() }
}

// ── World extension ────────────────────────────────────────────

impl World {
    // ── Добавление / удаление ──────────────────────────────────

    /// Добавить relation `(R, target)` к entity `subject`.
    pub fn add_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = self.relations.get_or_register::<R>();
        let relation_id = encode_relation(kind_idx, target.index);
        self.ensure_relation_component(relation_id);
        self.insert_relation_component(subject, relation_id);
    }

    /// Удалить relation `(R, target)` у entity `subject`.
    pub fn remove_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return,
        };
        let relation_id = encode_relation(kind_idx, target.index);
        self.remove_relation_component(subject, relation_id);
    }

    /// Проверить наличие relation `(R, target)` у entity `subject`. O(1).
    #[inline]
    pub fn has_relation<R: RelationKind>(&self, subject: Entity, _kind: R, target: Entity) -> bool {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return false,
        };
        let relation_id = encode_relation(kind_idx, target.index);
        let location = match self.entities.get_location(subject) {
            Some(loc) => loc,
            None => return false,
        };
        self.archetypes[location.archetype_id.0 as usize].has_component(relation_id)
    }

    // ── Query по конкретному target ────────────────────────────

    /// Итерация по всем entity у которых есть `(R, target)`.
    /// O(1) lookup через IdIndex, затем итерация только по matching архетипам.
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

        // O(1) — берём список архетипов из IdIndex
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
    /// O(1) lookup через wildcard IdRecord.
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

    /// Прямые дочерние entity (ChildOf relation). O(1) lookup.
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

        // Собираем entity из всех matching архетипов
        arch_ids.iter()
            .flat_map(move |&arch_id| {
                let arch = &self.archetypes[arch_id.0 as usize];
                arch.entities.iter().copied()
            })
    }

    /// Получить target entity для relation `(R, ?)` у subject. O(1).
    /// Возвращает первый найденный target (обычно у entity один target на kind).
    pub fn get_relation_target<R: RelationKind>(
        &self,
        subject: Entity,
        _kind: R,
    ) -> Option<Entity> {
        let kind_idx = self.relations.get_idx::<R>()?;
        let location = self.entities.get_location(subject)?;
        let arch = &self.archetypes[location.archetype_id.0 as usize];

        // Ищем первый relation_id с нужным kind в архетипе
        for &cid in &arch.component_ids {
            if is_relation_id(cid) && decode_kind(cid) == kind_idx && !is_wildcard(cid) {
                let target_idx = decode_target(cid);
                // Восстанавливаем Entity — нужен обратный lookup generation
                return self.entities_by_index(target_idx);
            }
        }
        None
    }

    /// Каскадный despawn: уничтожить entity и всех его потомков
    /// у которых есть cascade relation (например ChildOf).
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
        let tick = self.current_tick;
        if let Some(col_idx) = self.archetypes[new_arch_id.0 as usize].column_index(relation_id) {
            self.archetypes[new_arch_id.0 as usize].columns[col_idx].change_ticks.push(tick);
            self.archetypes[new_arch_id.0 as usize].columns[col_idx].len += 1;
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

    /// Найти Entity по индексу (O(1) через EntityAllocator).
    fn entities_by_index(&self, index: u32) -> Option<Entity> {
        self.entities.get_by_index(index)
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

/// Иерархия сцены. Каскадный despawn при удалении родителя.
#[derive(Clone, Copy)]
pub struct ChildOf;
impl RelationKind for ChildOf {
    fn cascade_delete_on_target_despawn() -> bool { true }
}

/// Владение item entity.
#[derive(Clone, Copy)]
pub struct Owns;
impl RelationKind for Owns {}

/// Произвольная направленная связь.
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
    }

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
}
