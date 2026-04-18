//! Relations — связи между entity, как в Flecs.
//!
//! Relation — это направленная связь вида `(Kind, Target)`:
//! - `ChildOf(parent)` — иерархия сцены
//! - `Likes(other)`    — произвольные связи
//! - `Owns(item)`      — владение
//!
//! ## Архитектура
//!
//! Relation кодируется как специальный `ComponentId` вида:
//!   `relation_id = RELATION_FLAG | (kind_id << 16) | target_index`
//!
//! Это позволяет хранить relations в обычных архетипах без отдельного
//! хранилища — архетип `[Position, ChildOf(parent1)]` отличается от
//! `[Position, ChildOf(parent2)]` как разные ComponentId.
//!
//! ## Пример
//! ```ignore
//! let parent = world.spawn_bundle((Transform::default(),));
//! let child  = world.spawn_bundle((Transform::default(),));
//! world.add_relation(child, ChildOf, parent);
//!
//! // Query: все дочерние entity конкретного родителя
//! for (entity, transform) in world.query_relation::<ChildOf, Read<Transform>>(parent) {
//!     println!("{entity} is child of {parent}");
//! }
//!
//! // Query: все entity у которых есть хоть какой-то ChildOf
//! for entity in world.query_has_relation::<ChildOf>() { ... }
//! ```

use std::any::TypeId;
use rustc_hash::FxHashMap;

use crate::{
    component::{ComponentId, ComponentInfo, Tick},
    entity::Entity,
    query::WorldQuery,
    world::World,
};

// ── RelationKind trait ─────────────────────────────────────────

/// Маркер типа связи. Реализуется для unit-struct'ов.
///
/// ```ignore
/// struct ChildOf;
/// impl RelationKind for ChildOf {}
///
/// struct Likes;
/// impl RelationKind for Likes {}
/// ```
pub trait RelationKind: Send + Sync + 'static {}

// ── RelationId ─────────────────────────────────────────────────

/// Флаг в старшем бите ComponentId — отличает relation от обычного компонента
const RELATION_FLAG: u32 = 1 << 31;

/// Кодирует (kind_type_index, target_entity_index) в один u32
/// Формат: `RELATION_FLAG | (kind_idx << 20) | target_idx`
/// Поддерживает до 4096 видов relation и до 1M entity
#[inline]
fn encode_relation(kind_idx: u32, target_idx: u32) -> ComponentId {
    debug_assert!(kind_idx < (1 << 11), "too many relation kinds");
    debug_assert!(target_idx < (1 << 20), "entity index too large for relation");
    ComponentId(RELATION_FLAG | (kind_idx << 20) | target_idx)
}

#[inline]
pub fn is_relation_id(id: ComponentId) -> bool {
    id.0 & RELATION_FLAG != 0
}

#[inline]
fn decode_kind(id: ComponentId) -> u32 {
    (id.0 & !RELATION_FLAG) >> 20
}

#[inline]
fn decode_target(id: ComponentId) -> u32 {
    id.0 & ((1 << 20) - 1)
}

// ── RelationRegistry ───────────────────────────────────────────

/// Реестр видов relation — отображает TypeId → kind_index
pub struct RelationRegistry {
    type_to_idx: FxHashMap<TypeId, u32>,
    next_idx: u32,
}

impl RelationRegistry {
    pub fn new() -> Self {
        Self { type_to_idx: FxHashMap::default(), next_idx: 0 }
    }

    pub fn get_or_register<R: RelationKind>(&mut self) -> u32 {
        let type_id = TypeId::of::<R>();
        if let Some(&idx) = self.type_to_idx.get(&type_id) {
            return idx;
        }
        let idx = self.next_idx;
        self.next_idx += 1;
        self.type_to_idx.insert(type_id, idx);
        idx
    }

    pub fn get_idx<R: RelationKind>(&self) -> Option<u32> {
        self.type_to_idx.get(&TypeId::of::<R>()).copied()
    }
}

impl Default for RelationRegistry {
    fn default() -> Self { Self::new() }
}

// ── World extension — relation ops ─────────────────────────────

/// Расширение World для работы с relations.
/// Реализовано как отдельный impl блок в world.rs через pub(crate) поля.
impl World {
    /// Добавить relation `(R, target)` к entity `subject`.
    ///
    /// Relation хранится как ZST-компонент с уникальным ComponentId
    /// кодирующим (kind, target). Это создаёт новый архетип если нужно.
    pub fn add_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = self.relations.get_or_register::<R>();
        let target_idx = target.index;
        let relation_id = encode_relation(kind_idx, target_idx);

        // Регистрируем как ZST компонент если ещё нет
        self.ensure_relation_component(relation_id);
        self.insert_relation_component(subject, relation_id);
    }

    /// Удалить relation `(R, target)` у entity `subject`
    pub fn remove_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return,
        };
        let relation_id = encode_relation(kind_idx, target.index);
        self.remove_relation_component(subject, relation_id);
    }

    /// Проверить наличие relation `(R, target)` у entity `subject`
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

    /// Итерация по всем entity у которых есть relation `(R, specific_target)`.
    /// Возвращает итератор по (Entity, Q::Item).
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

        let arch_states: Vec<RelationArchState<Q::State>> = if all_found {
            self.archetypes
                .iter()
                .enumerate()
                .filter(|(_, arch)| {
                    !arch.is_empty()
                        && arch.has_component(relation_id)
                        && Q::matches_archetype(arch, &data_ids)
                })
                .map(|(arch_idx, arch)| {
                    let state = unsafe { Q::fetch_state(arch, &data_ids, Tick::ZERO) };
                    RelationArchState { arch_idx, state, len: arch.len() }
                })
                .collect()
        } else {
            Vec::new()
        };

        RelationIter { world: self, arch_states, arch_cursor: 0, row_cursor: 0 }
    }

    /// Итерация по всем entity у которых есть хоть какой-то `R` relation
    /// (независимо от target).
    pub fn query_has_relation<'w, R: RelationKind>(&'w self, _kind: R) -> HasRelationIter<'w> {
        let kind_idx = match self.relations.get_idx::<R>() {
            Some(idx) => idx,
            None => return HasRelationIter { world: self, arch_states: Vec::new(), arch_cursor: 0, row_cursor: 0 },
        };

        // Собираем все архетипы у которых есть хотя бы один relation данного kind
        let arch_states: Vec<HasRelArchState> = self.archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| {
                !arch.is_empty() && arch.component_ids.iter().any(|&cid| {
                    is_relation_id(cid) && decode_kind(cid) == kind_idx
                })
            })
            .map(|(arch_idx, arch)| {
                // Собираем все target entity для этого архетипа
                let targets: Vec<Entity> = arch.component_ids.iter()
                    .filter(|&&cid| is_relation_id(cid) && decode_kind(cid) == kind_idx)
                    .map(|&cid| {
                        let target_idx = decode_target(cid);
                        // Восстанавливаем Entity из индекса (generation неизвестен — используем 0)
                        // В реальном коде нужен обратный lookup, здесь упрощение
                        Entity { index: target_idx, generation: 0 }
                    })
                    .collect();
                HasRelArchState { arch_idx, len: arch.len(), targets }
            })
            .collect();

        HasRelationIter { world: self, arch_states, arch_cursor: 0, row_cursor: 0 }
    }

    /// Получить всех прямых потомков entity (ChildOf relation)
    pub fn children_of<'w, R: RelationKind>(&'w self, _kind: R, parent: Entity) -> impl Iterator<Item = Entity> + 'w {
        let kind_idx = self.relations.get_idx::<R>();
        let relation_id = kind_idx.map(|k| encode_relation(k, parent.index));

        self.archetypes.iter().enumerate()
            .filter(move |(_, arch)| {
                !arch.is_empty() && relation_id.map_or(false, |rid| arch.has_component(rid))
            })
            .flat_map(|(_, arch)| arch.entities.iter().copied())
    }

    // ── Внутренние методы ──────────────────────────────────────

    fn ensure_relation_component(&mut self, relation_id: ComponentId) {
        // Relation хранится как ZST — size=0, align=1
        if self.registry.get_info(relation_id).is_none() {
            // Регистрируем напрямую в registry как ZST
            self.registry.register_raw(relation_id, ComponentInfo {
                id: relation_id,
                name: "<relation>",
                type_id: std::any::TypeId::of::<()>(), // placeholder
                size: 0,
                align: 1,
                drop_fn: |_| {},
            });
        }
    }

    fn insert_relation_component(&mut self, entity: Entity, relation_id: ComponentId) {
        let location = match self.entities.get_location(entity) {
            Some(loc) => loc,
            None => return,
        };
        if self.archetypes[location.archetype_id.0 as usize].has_component(relation_id) {
            return; // уже есть
        }
        let new_arch_id = self.find_or_create_archetype_with(location.archetype_id, relation_id);
        let new_row = self.move_entity(entity, location, new_arch_id);
        let tick = self.current_tick;
        // ZST — данных нет, только тик
        if let Some(col_idx) = self.archetypes[new_arch_id.0 as usize].column_index(relation_id) {
            self.archetypes[new_arch_id.0 as usize].columns[col_idx].change_ticks.push(tick);
            self.archetypes[new_arch_id.0 as usize].columns[col_idx].len += 1;
        }
        self.entities.set_location(entity, crate::entity::EntityLocation {
            archetype_id: new_arch_id,
            row: new_row,
        });
    }

    fn remove_relation_component(&mut self, entity: Entity, relation_id: ComponentId) {
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

struct RelationArchState<S> {
    arch_idx: usize,
    state: S,
    len: usize,
}

pub struct RelationIter<'w, Q: WorldQuery> {
    world: &'w World,
    arch_states: Vec<RelationArchState<Q::State>>,
    arch_cursor: usize,
    row_cursor: usize,
}

impl<'w, Q: WorldQuery> RelationIter<'w, Q> {
    fn empty(world: &'w World) -> Self {
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

// ── HasRelationIter ────────────────────────────────────────────

struct HasRelArchState {
    arch_idx: usize,
    len: usize,
    #[allow(dead_code)]
    targets: Vec<Entity>,
}

pub struct HasRelationIter<'w> {
    world: &'w World,
    arch_states: Vec<HasRelArchState>,
    arch_cursor: usize,
    row_cursor: usize,
}

impl<'w> Iterator for HasRelationIter<'w> {
    type Item = Entity;

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
            return Some(self.world.archetypes[a.arch_idx].entities[row]);
        }
    }
}

// ── Встроенные relation kinds ──────────────────────────────────

/// Иерархия сцены: child `ChildOf` parent
pub struct ChildOf;
impl RelationKind for ChildOf {}

/// Владение: owner `Owns` item
pub struct Owns;
impl RelationKind for Owns {}

/// Произвольная связь
pub struct Likes;
impl RelationKind for Likes {}
