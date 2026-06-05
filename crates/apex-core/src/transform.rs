//! TransformPropagation — иерархические трансформации.
//!
//! # Архитектура
//!
//! - [`LocalTransform`] — position/rotation/scale entity (локальное пространство)
//! - [`GlobalTransform`] — итоговая мировая матрица (пересчитывается из иерархии)
//! - [`propagate_transforms`] — эксклюзивная система, выполняющая иерархический пересчёт
//!
//! # DX (после C1/C2)
//!
//! Ручной `TransformDirty` **удалён**: dirty-детекция идёт через
//! `Changed<LocalTransform>` — достоверно для мутаций и через `Query<Write>`, и
//! через `World::get_mut` (C1). Достаточно изменить `LocalTransform` — пересчёт
//! произойдёт автоматически, каскадируясь на потомков. Достаточно заспавнить
//! entity с одним `LocalTransform` — `GlobalTransform` создаётся **системой
//! `propagate_transforms` при первом проходе** (не в момент спавна; до этого
//! `get::<GlobalTransform>` вернёт `None`). См. doc у [`GlobalTransform`].
//!
//! # Алгоритм
//!
//! 1. Собрать entity с `Changed<LocalTransform>` (с прошлого запуска).
//! 2. Для каждой: `GlobalTransform = parent.GlobalTransform * self.LocalTransform`.
//! 3. Каскадировать пересчёт на детей изменённых entity.
//!
//! # Использование в Scheduler
//!
//! ```ignore
//! use apex_core::transform::{LocalTransform, GlobalTransform, TransformPlugin};
//! use apex_scheduler::stage::StageLabel;
//!
//! TransformPlugin::register_components(&mut world);
//!
//! scheduler.add_system_to_stage(
//!     "propagate_transforms",
//!     apex_core::transform::propagate_transforms,
//!     StageLabel::PostUpdate,
//! );
//! ```

use glam::{Mat4, Quat, Vec3};
use rustc_hash::FxHashSet;

use crate::{
    component::Tick,
    entity::Entity,
    query::{Changed, Query},
    relations::ChildOf,
    world::World,
};

// ── Компоненты трансформаций ─────────────────────────────────────

/// Локальная трансформация entity (относительно родителя).
///
/// Если entity не имеет родителя (no ChildOf) — это мировая трансформация.
#[derive(Debug, Clone, Copy, PartialEq, apex_macros::Component)]
pub struct LocalTransform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl LocalTransform {
    /// Единичная трансформация (zero translation, identity rotation, unit scale).
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
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
/// # Когда появляется (важно!)
///
/// `GlobalTransform` **НЕ добавляется в момент спавна** — достаточно заспавнить
/// entity с одним [`LocalTransform`]. Компонент создаётся **автоматически** системой
/// [`propagate_transforms`] при её **первом проходе** после спавна (для entity, у
/// которых есть `LocalTransform`, но ещё нет `GlobalTransform`).
///
/// Практически: между `world.spawn((LocalTransform...,))` и первым запуском
/// `propagate_transforms` (PostUpdate) `world.get::<GlobalTransform>(e)` вернёт
/// `None`. После первого прохода — `Some(..)` с корректной матрицей. Рендер и
/// прочие потребители читают `GlobalTransform` уже после propagate в том же кадре.
///
/// Если `GlobalTransform` нужен **немедленно** при спавне — добавьте его в bundle
/// явно: `world.spawn((LocalTransform::from_translation(t), GlobalTransform::IDENTITY))`
/// (его значение всё равно будет пересчитано propagate).
///
/// Пересчитывается в PostUpdate системой `propagate_transforms`.
/// Не сериализуется — восстанавливается из иерархии + LocalTransform.
#[derive(Debug, Clone, Copy, PartialEq, apex_macros::Component)]
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

// ── Система Propagation ─────────────────────────────────────────

/// Scratch-буферы + состояние change-detection для [`propagate_transforms`].
/// Переиспользуются каждый кадр, избегая Vec-аллокаций в горячем пути.
#[derive(Default)]
pub struct TransformScratch {
    /// Тик предыдущего запуска propagate — база для `Changed<LocalTransform>`.
    pub(crate) last_run: Tick,
    /// Список dirty entity из query (шаг 1)
    pub(crate) dirty_entities: Vec<Entity>,
    /// Set для O(1) проверки dirty (по entity.index)
    pub(crate) dirty_set: FxHashSet<u32>,
    /// Топологически отсортированные entity (шаг 2–3)
    pub(crate) ordered: Vec<Entity>,
    /// Множество уже обработанных entity (для DFS)
    pub(crate) seen: FxHashSet<u32>,
    /// Стек для итеративного DFS
    pub(crate) stack: Vec<Entity>,
    /// Временный буфер для children (шаг 3)
    pub(crate) children: Vec<Entity>,
}

/// Эксклюзивная система: пересчитывает `GlobalTransform` для всех entity,
/// чей `LocalTransform` **изменился** с прошлого запуска (`Changed<LocalTransform>`),
/// каскадируя на потомков. Ручной `TransformDirty` больше не нужен — после C1
/// `Changed<LocalTransform>` достоверен на всех путях мутации (`Query<Write>` и
/// `World::get_mut`). Выполняется в PostUpdate.
///
/// # Алгоритм
///
/// 1. Собрать entity с `Changed<LocalTransform>` (с прошлого `last_run`).
/// 2. Топосортировать (корни → листья), каскадируя dirty на детей изменённых.
/// 3. `GlobalTransform = parent.GlobalTransform * self.LocalTransform`;
///    если `GlobalTransform` ещё нет — авто-инициализировать (DX: спавн с одним
///    `LocalTransform` достаточно).
///
/// # Change-detection
///
/// База — `scratch.last_run`; в конце пишется `world.current_tick()`. Требует
/// покадрового продвижения тика (`world.tick()` перед `run()`; авто — в C7).
///
/// # Ресурсы
///
/// Использует [`TransformScratch`] для переиспользования буферов между кадрами.
pub fn propagate_transforms(world: &mut World) {
    // Извлекаем scratch-буфер из ресурсов (или создаём новый при первом вызове)
    // remove_resource перемещает значение в локальную переменную, освобождая
    // заимствование world — это позволяет вызывать world.get()/world.insert()
    // без конфликта borrow checker.
    let mut scratch = world
        .remove_resource::<TransformScratch>()
        .unwrap_or_default();

    let last_run = scratch.last_run;
    let this_run = world.current_tick();

    // Очищаем все буферы (емкость сохраняется — аллокации переиспользуются)
    scratch.dirty_entities.clear();
    scratch.dirty_set.clear();
    scratch.ordered.clear();
    scratch.seen.clear();
    scratch.stack.clear();
    scratch.children.clear();

    // 1. Собираем entity с Changed<LocalTransform> (с прошлого запуска) и строим set.
    //    Используем не-кэшированный Query::new_with_tick + for_each (надёжный путь;
    //    см. TD-1 про CachedQuery::iter).
    {
        let q = Query::<Changed<LocalTransform>>::new_with_tick(world, last_run);
        q.for_each(|e, _| {
            scratch.dirty_entities.push(e);
            scratch.dirty_set.insert(e.index);
        });
    } // query Q дропается здесь

    if scratch.dirty_entities.is_empty() {
        // Ничего не изменилось — фиксируем тик и выходим.
        scratch.last_run = this_run;
        world.insert_resource(scratch);
        return;
    }

    // 2. Топологическая сортировка dirty entity (корни → листья)
    //    Итеративный DFS: для каждого dirty entity поднимаемся по предкам
    //    и добавляем их в порядке от корня к листьям.
    for &entity in &scratch.dirty_entities {
        if !scratch.dirty_set.contains(&entity.index) {
            // O(1), без world lookup
            continue;
        }

        // Явный стек для итеративного DFS (очищаем перед каждым entity)
        scratch.stack.clear();
        scratch.stack.push(entity);

        while let Some(top) = scratch.stack.last().copied() {
            if scratch.seen.contains(&top.index) {
                scratch.stack.pop();
                continue;
            }

            // Есть ли dirty родитель, который ещё не в `seen`?
            let parent = world.get_relation_target(top, ChildOf);
            let need_parent = parent
                .map(|p| {
                    scratch.dirty_set.contains(&p.index)  // O(1) вместо world.get
                        && !scratch.seen.contains(&p.index)
                })
                .unwrap_or(false);

            if need_parent {
                scratch.stack.push(parent.unwrap());
            } else {
                scratch.seen.insert(top.index);
                scratch.ordered.push(top);
                scratch.stack.pop();
            }
        }
    }

    // 3. Sequential обработка от корней к листьям с каскадированием dirty на детей
    //    Используем while i < ordered.len(), т.к. ordered динамически растёт
    //    при добавлении детей dirty-родителя.
    let mut i = 0;
    while i < scratch.ordered.len() {
        let entity = scratch.ordered[i];

        if !world.is_alive(entity) {
            i += 1;
            continue;
        }

        let local = match world.get::<LocalTransform>(entity) {
            Some(l) => *l,
            None => {
                i += 1;
                continue;
            }
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

        // Записываем новый GlobalTransform; если его ещё нет — авто-инициализируем
        // (DX: достаточно заспавнить entity с одним LocalTransform — issue #3/#17).
        if world.get::<GlobalTransform>(entity).is_some() {
            if let Some(gt) = world.get_mut::<GlobalTransform>(entity) {
                gt.0 = global_matrix;
            }
        } else {
            world.insert(entity, GlobalTransform(global_matrix));
        }

        scratch.dirty_set.remove(&entity.index); // поддерживаем set актуальным

        // ── Каскадирование dirty на детей ──────────────────────────
        // Если у этой entity есть дети (ChildOf), помечаем их dirty (в scratch-set,
        // без компонента-маркера), чтобы их GlobalTransform пересчитался в этом же
        // проходе. Решает «пользователь изменил только родителя» (issue #2).
        scratch.children.clear();
        for child in world.children_of(ChildOf, entity) {
            scratch.children.push(child);
        }
        for &child in &scratch.children {
            if !world.is_alive(child) {
                continue;
            }
            if scratch.dirty_set.insert(child.index) {
                // вставка вернула true ⇒ ребёнка ещё не было в наборе
                scratch.ordered.push(child);
            }
        }

        i += 1;
    }

    // Фиксируем тик этого запуска и возвращаем scratch для переиспользования.
    scratch.last_run = this_run;
    world.insert_resource(scratch);
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
pub struct TransformPlugin;

impl TransformPlugin {
    /// (Опционально) пред-инициализировать состояние Transform в World.
    ///
    /// **Регистрация компонентов больше не нужна** — `LocalTransform`/`GlobalTransform`
    /// помечены `#[derive(Component)]` и авто-регистрируются при `World::new()`
    /// (linkme). `TransformDirty` и write-hook удалены (dirty-детекция — через
    /// `Changed<LocalTransform>`, C1). Эта функция лишь пред-создаёт scratch-буфер
    /// `propagate_transforms` (он также создаётся лениво при первом запуске).
    pub fn register_components(world: &mut World) {
        world.insert_resource(TransformScratch::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// C6: `LocalTransform`/`GlobalTransform` авто-регистрируются при `World::new()`
    /// через `#[derive(Component)]` (linkme) — без ручного `register_component`.
    #[test]
    fn transform_components_auto_registered() {
        let world = World::new();
        assert!(
            world.registry().get_id::<LocalTransform>().is_some(),
            "LocalTransform должен авто-регистрироваться через derive(Component)"
        );
        assert!(
            world.registry().get_id::<GlobalTransform>().is_some(),
            "GlobalTransform должен авто-регистрироваться через derive(Component)"
        );
    }

    #[test]
    fn propagate_single_entity_auto_init_global() {
        // БЕЗ register_components: derive авто-регистрирует компоненты,
        // scratch создаётся лениво в propagate.
        let mut world = World::new();

        // Спавн с ОДНИМ LocalTransform — без GlobalTransform, без TransformDirty.
        let entity = world.spawn((LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)),));

        // GlobalTransform ещё не существует.
        assert!(world.get::<GlobalTransform>(entity).is_none());

        propagate_transforms(&mut world);

        // propagate авто-инициализировал GlobalTransform = LocalTransform.
        let gt = world.get::<GlobalTransform>(entity).unwrap();
        assert_eq!(gt.0.transform_point3(Vec3::ZERO), Vec3::new(10.0, 0.0, 0.0));
    }

    #[test]
    fn propagate_parent_child_chain() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        // Иерархия parent → child, оба с одним LocalTransform (GlobalTransform авто).
        let parent = world.spawn((LocalTransform::from_translation(Vec3::new(100.0, 0.0, 0.0)),));
        let child = world.spawn((LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)),));
        world.add_relation(child, ChildOf, parent);

        propagate_transforms(&mut world);

        // child.Global = parent.Global * child.Local = (100 + 10) = 110 по X.
        let child_gt = world.get::<GlobalTransform>(child).unwrap();
        assert_eq!(
            child_gt.0.transform_point3(Vec3::ZERO),
            Vec3::new(110.0, 0.0, 0.0),
            "Child должен быть на 110.0 по X (100 parent + 10 local)"
        );
        let parent_gt = world.get::<GlobalTransform>(parent).unwrap();
        assert_eq!(parent_gt.0.transform_point3(Vec3::ZERO), Vec3::new(100.0, 0.0, 0.0));
    }

    #[test]
    fn propagate_deep_hierarchy() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        // grandparent → parent → child
        let grandparent = world.spawn((
            LocalTransform::from_translation(Vec3::new(50.0, 0.0, 0.0)),
            GlobalTransform::default(),
        ));

        let parent = world.spawn((LocalTransform::from_translation(Vec3::new(30.0, 0.0, 0.0)),));
        let child = world.spawn((LocalTransform::from_translation(Vec3::new(20.0, 0.0, 0.0)),));

        world.add_relation(parent, ChildOf, grandparent);
        world.add_relation(child, ChildOf, parent);

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

    /// Ключевой C1+C2: мутация `LocalTransform` через `Query<Write<_>>` (без
    /// ручного `TransformDirty`) триггерит пересчёт `GlobalTransform`.
    #[test]
    fn changed_local_via_write_query_triggers_recompute() {
        use crate::query::Write;

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let e = world.spawn((LocalTransform::from_translation(Vec3::new(1.0, 0.0, 0.0)),));

        // Первый проход: авто-init GlobalTransform = (1,0,0).
        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(e).unwrap().0.transform_point3(Vec3::ZERO),
            Vec3::new(1.0, 0.0, 0.0)
        );

        // Продвигаем тик (как делает кадр) и мутируем через Query<Write> — без
        // какого-либо ручного маркера.
        world.tick();
        {
            let q = Query::<Write<LocalTransform>>::new(&world);
            q.for_each(|_, mut lt| {
                lt.translation = Vec3::new(42.0, 0.0, 0.0);
            });
        }

        propagate_transforms(&mut world);

        // GlobalTransform пересчитан без TransformDirty.
        assert_eq!(
            world.get::<GlobalTransform>(e).unwrap().0.transform_point3(Vec3::ZERO),
            Vec3::new(42.0, 0.0, 0.0),
            "Changed<LocalTransform> через Query<Write> должен триггерить пересчёт"
        );
    }

    /// Изменение только родителя каскадирует пересчёт на детей.
    #[test]
    fn parent_change_cascades_to_children() {
        use crate::query::Write;

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let parent = world.spawn((LocalTransform::from_translation(Vec3::new(0.0, 0.0, 0.0)),));
        let child = world.spawn((LocalTransform::from_translation(Vec3::new(5.0, 0.0, 0.0)),));
        world.add_relation(child, ChildOf, parent);

        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(child).unwrap().0.transform_point3(Vec3::ZERO),
            Vec3::new(5.0, 0.0, 0.0)
        );

        // Двигаем ТОЛЬКО родителя.
        world.tick();
        {
            let q = Query::<Write<LocalTransform>>::new(&world);
            q.for_each(|e, mut lt| {
                if e == parent {
                    lt.translation = Vec3::new(100.0, 0.0, 0.0);
                }
            });
        }
        propagate_transforms(&mut world);

        // Ребёнок пересчитан каскадно: 100 (parent) + 5 (local) = 105.
        assert_eq!(
            world.get::<GlobalTransform>(child).unwrap().0.transform_point3(Vec3::ZERO),
            Vec3::new(105.0, 0.0, 0.0),
            "изменение родителя должно каскадно пересчитать ребёнка"
        );
    }
}
