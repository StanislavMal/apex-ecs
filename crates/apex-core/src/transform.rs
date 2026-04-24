//! TransformPropagation — иерархические трансформации.
//!
//! # Архитектура
//!
//! - [`LocalTransform`] — position/rotation/scale entity (локальное пространство)
//! - [`GlobalTransform`] — итоговая мировая матрица (пересчитывается из иерархии)
//! - [`TransformDirty`] — маркерный компонент: эта entity требует пересчёта
//! - [`propagate_transforms`] — sequential система, выполняющая BFS propagation
//!
//! # Алгоритм
//!
//! 1. Собрать все entity с TransformDirty
//! 2. Для каждой: вычислить GlobalTransform = parent.GlobalTransform * self.LocalTransform
//! 3. Снять `TransformDirty` после пересчёта
//!
//! # Использование в Scheduler
//!
//! ```ignore
//! use apex_core::transform::{LocalTransform, GlobalTransform, TransformPlugin};
//! use apex_scheduler::stage::StageLabel;
//!
//! // Зарегистрировать компоненты
//! TransformPlugin::register_components(&mut world);
//!
//! // Добавить propagate_transforms как sequential систему в PostUpdate
//! scheduler.add_system_to_stage(
//!     "propagate_transforms",
//!     apex_core::transform::propagate_transforms,
//!     StageLabel::PostUpdate,
//! );
//! ```

use glam::{Mat4, Quat, Vec3};

use crate::{
    entity::Entity,
    query::Read,
    relations::ChildOf,
    world::World,
};

// ── Компоненты трансформаций ─────────────────────────────────────

/// Локальная трансформация entity (относительно родителя).
///
/// Если entity не имеет родителя (no ChildOf) — это мировая трансформация.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalTransform {
    pub translation: Vec3,
    pub rotation:    Quat,
    pub scale:       Vec3,
}

impl LocalTransform {
    /// Единичная трансформация (zero translation, identity rotation, unit scale).
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation:    Quat::IDENTITY,
        scale:       Vec3::ONE,
    };

    pub fn from_translation(t: Vec3) -> Self {
        Self {
            translation: t,
            ..Self::IDENTITY
        }
    }

    pub fn from_rotation(r: Quat) -> Self {
        Self {
            rotation: r,
            ..Self::IDENTITY
        }
    }

    pub fn from_scale(s: Vec3) -> Self {
        Self {
            scale: s,
            ..Self::IDENTITY
        }
    }

    /// Преобразовать в аффинную матрицу 4x4.
    #[inline]
    pub fn to_matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

impl Default for LocalTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Глобальная (мировая) трансформация entity.
///
/// Пересчитывается в PostUpdate системой `propagate_transforms`.
/// Не сериализуется — восстанавливается из иерархии + LocalTransform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobalTransform(pub Mat4);

impl GlobalTransform {
    pub const IDENTITY: Self = Self(Mat4::IDENTITY);

    #[inline]
    pub fn to_matrix(&self) -> &Mat4 {
        &self.0
    }
}

impl Default for GlobalTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Маркер: эта entity требует пересчёта GlobalTransform.
///
/// Устанавливается:
/// - При изменении LocalTransform у родителя (все дети помечаются рекурсивно)
/// - При добавлении/удалении ChildOf отношения
///
/// Снимается системой `propagate_transforms` после пересчёта.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransformDirty;

// ── Система Propagation ─────────────────────────────────────────

/// Sequential-система: пересчитывает GlobalTransform для всех entity с TransformDirty.
///
/// Выполняется в PostUpdate этапе.
///
/// # Алгоритм
///
/// 1. Найти все entity с TransformDirty (через query_typed)
/// 2. Для каждой: вычислить GlobalTransform = parent.GlobalTransform * self.LocalTransform
/// 3. Снять флаг TransformDirty
pub fn propagate_transforms(world: &mut World) {
    // 1. Собираем dirty entity
    let dirty_entities: Vec<Entity> = {
        let q = world.query_typed::<Read<TransformDirty>>();
        let mut entities = Vec::new();
        q.for_each(|e, _| entities.push(e));
        entities
    };

    if dirty_entities.is_empty() {
        return;
    }

    // 2. Топологическая сортировка dirty entity (корни → листья)
    //    Итеративный DFS: для каждого dirty entity поднимаемся по предкам
    //    и добавляем их в порядке от корня к листьям.
    use rustc_hash::FxHashSet;

    let mut ordered = Vec::with_capacity(dirty_entities.len());
    let mut seen = FxHashSet::default();

    for &entity in &dirty_entities {
        if !world.get::<TransformDirty>(entity).is_some() {
            continue;
        }

        // Явный стек для итеративного DFS
        let mut stack = vec![entity];

        while let Some(top) = stack.last().copied() {
            if seen.contains(&top.index) {
                stack.pop();
                continue;
            }

            // Есть ли dirty родитель, который ещё не в `seen`?
            let parent = world.get_relation_target(top, ChildOf);
            let need_parent = parent
                .map(|p| {
                    world.get::<TransformDirty>(p).is_some() && !seen.contains(&p.index)
                })
                .unwrap_or(false);

            if need_parent {
                stack.push(parent.unwrap());
            } else {
                seen.insert(top.index);
                ordered.push(top);
                stack.pop();
            }
        }
    }

    // 3. Sequential обработка от корней к листьям с каскадированием dirty на детей
    //    Используем while i < ordered.len(), т.к. ordered динамически растёт
    //    при добавлении детей dirty-родителя.
    let mut i = 0;
    while i < ordered.len() {
        let entity = ordered[i];

        if !world.is_alive(entity) {
            i += 1;
            continue;
        }

        let local = match world.get::<LocalTransform>(entity) {
            Some(l) => *l,
            None => { i += 1; continue; }
        };

        let parent = world.get_relation_target(entity, ChildOf);

        let global_matrix = if let Some(parent_entity) = parent {
            match world.get::<GlobalTransform>(parent_entity) {
                Some(pg) => pg.0 * local.to_matrix(),
                None => local.to_matrix(),
            }
        } else {
            local.to_matrix()
        };

        // Записываем новый GlobalTransform
        if let Some(gt) = world.get_mut::<GlobalTransform>(entity) {
            gt.0 = global_matrix;
        }

        // Снимаем TransformDirty
        world.remove::<TransformDirty>(entity);

        // ── Каскадирование TransformDirty на детей ─────────────────
        // Если у этой entity есть дети (ChildOf), помечаем их как dirty,
        // чтобы их GlobalTransform тоже пересчитался.
        // Это решает проблему "пользователь пометил только родителя" (Feature 2, issue #2).
        let children: Vec<Entity> = world.children_of(ChildOf, entity).collect();
        for child in children {
            if !world.is_alive(child) {
                continue;
            }
            if world.get::<TransformDirty>(child).is_none() {
                world.insert(child, TransformDirty);
                // Добавляем в ordered-список для обработки в этом же проходе
                ordered.push(child);
            }
        }

        i += 1;
    }
}

// ── Plugin ───────────────────────────────────────────────────────

/// Plugin для регистрации Transform компонентов.
///
/// Регистрирует [`LocalTransform`], [`GlobalTransform`] и [`TransformDirty`].
///
/// # Добавление системы
///
/// Система `propagate_transforms` добавляется в Scheduler вручную:
///
/// ```ignore
/// use apex_scheduler::stage::StageLabel;
///
/// scheduler.add_system_to_stage(
///     "propagate_transforms",
///     apex_core::transform::propagate_transforms,
///     StageLabel::PostUpdate,
/// );
/// ```
/// Функция-хук: автоматически вставляет TransformDirty при изменении LocalTransform.
///
/// Вызывается из `World::get_mut::<LocalTransform>()` после обновления tick изменения.
fn mark_local_transform_dirty(entity: Entity, world: &mut World) {
    // Вставляем TransformDirty, если ещё нет (insert дедуплицируется).
    // Если entity уже имеет TransformDirty, это no-op (просто обновит tick).
    world.insert(entity, TransformDirty);
}

pub struct TransformPlugin;

impl TransformPlugin {
    /// Зарегистрировать Transform-компоненты в World.
    pub fn register_components(world: &mut World) {
        world.register_component::<LocalTransform>();
        world.register_component::<GlobalTransform>();
        world.register_component::<TransformDirty>();

        // Регистрируем write_hook для автоматической пометки TransformDirty
        // при любом вызове get_mut::<LocalTransform>().
        // Это решает проблему "забыл пометить Dirty" (Feature 2, issue #1).
        world.register_write_hook::<LocalTransform>(mark_local_transform_dirty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Read;
    use crate::world::World;

    #[test]
    fn local_transform_default_is_identity() {
        let lt = LocalTransform::default();
        assert_eq!(lt.translation, Vec3::ZERO);
        assert_eq!(lt.rotation, Quat::IDENTITY);
        assert_eq!(lt.scale, Vec3::ONE);
    }

    #[test]
    fn local_transform_to_matrix() {
        let lt = LocalTransform::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let m = lt.to_matrix();
        // Проверяем что матрица 4x4 переводит начало координат в translation
        let origin = Vec3::ZERO;
        let transformed = m.transform_point3(origin);
        assert_eq!(transformed, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn global_transform_default_is_identity() {
        let gt = GlobalTransform::default();
        assert_eq!(*gt.to_matrix(), Mat4::IDENTITY);
    }

    #[test]
    fn propagate_single_entity_no_parent() {
        let mut world = World::new();

        // Регистрируем компоненты
        TransformPlugin::register_components(&mut world);

        let entity = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .insert(TransformDirty)
            .id();

        propagate_transforms(&mut world);

        // После пропагации GlobalTransform должен быть = LocalTransform
        let gt = world.get::<GlobalTransform>(entity).unwrap();
        assert_eq!(gt.0.transform_point3(Vec3::ZERO), Vec3::new(10.0, 0.0, 0.0));

        // TransformDirty должен быть снят
        let has_dirty = {
            let q = world.query_typed::<Read<TransformDirty>>();
            let mut count = 0;
            q.for_each(|_, _| count += 1);
            count
        };
        assert_eq!(has_dirty, 0, "TransformDirty должен быть снят");
    }

    #[test]
    fn propagate_parent_child_chain() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        // Создаём иерархию: parent → child
        let parent = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(100.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .id();

        let child = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .insert(TransformDirty)
            .id();

        world.add_relation(child, ChildOf, parent);

        // Сначала propagate родителя (parent не dirty, но нужно обновить его GlobalTransform вручную)
        let parent_local = *world.get::<LocalTransform>(parent).unwrap();
        if let Some(gt) = world.get_mut::<GlobalTransform>(parent) {
            gt.0 = parent_local.to_matrix();
        }

        propagate_transforms(&mut world);

        // После пропагации: child.Global = parent.Global * child.Local
        // parent на (100,0,0), child локально на (10,0,0) → итог (110,0,0)
        let child_gt = world.get::<GlobalTransform>(child).unwrap();
        assert_eq!(
            child_gt.0.transform_point3(Vec3::ZERO),
            Vec3::new(110.0, 0.0, 0.0),
            "Child должен быть на 110.0 по X (100 parent + 10 local)"
        );

        // TransformDirty снят
        let has_dirty = {
            let q = world.query_typed::<Read<TransformDirty>>();
            let mut count = 0;
            q.for_each(|_, _| count += 1);
            count
        };
        assert_eq!(has_dirty, 0);
    }

    #[test]
    fn propagate_deep_hierarchy() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        // grandparent → parent → child
        let grandparent = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(50.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .id();

        let parent = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(30.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .insert(TransformDirty)
            .id();

        let child = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(20.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .insert(TransformDirty)
            .id();

        world.add_relation(parent, ChildOf, grandparent);
        world.add_relation(child, ChildOf, parent);

        // Устанавливаем GlobalTransform для grandparent вручную
        let grandparent_local = *world.get::<LocalTransform>(grandparent).unwrap();
        if let Some(gt) = world.get_mut::<GlobalTransform>(grandparent) {
            gt.0 = grandparent_local.to_matrix();
        }

        propagate_transforms(&mut world);

        // parent = 50 + 30 = 80
        let parent_gt = world.get::<GlobalTransform>(parent).unwrap();
        assert_eq!(
            parent_gt.0.transform_point3(Vec3::ZERO),
            Vec3::new(80.0, 0.0, 0.0),
            "Parent должен быть на 80.0"
        );

        // child = 80 + 20 = 100
        let child_gt = world.get::<GlobalTransform>(child).unwrap();
        assert_eq!(
            child_gt.0.transform_point3(Vec3::ZERO),
            Vec3::new(100.0, 0.0, 0.0),
            "Child должен быть на 100.0"
        );
    }

    #[test]
    fn no_transform_dirty_skips_propagation() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        // Entity без TransformDirty
        let entity = world
            .spawn()
            .insert(LocalTransform::from_translation(Vec3::new(5.0, 0.0, 0.0)))
            .insert(GlobalTransform::default())
            .id();

        propagate_transforms(&mut world);

        // GlobalTransform не должен измениться (остаётся identity)
        let gt = world.get::<GlobalTransform>(entity).unwrap();
        assert_eq!(*gt.to_matrix(), Mat4::IDENTITY,
            "GlobalTransform не должен измениться без TransformDirty");
    }
}
