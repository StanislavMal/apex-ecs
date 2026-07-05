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
//! 2. Оставить «dirty-корни» — изменённые entity без изменённого предка
//!    (остальные лежат внутри их поддеревьев и будут пересчитаны спуском).
//! 3. Для каждого dirty-корня спуститься по всему поддереву (DFS), передавая
//!    мировую матрицу родителя по значению: `Global = parent_global * Local`.
//!    Каждый узел посещается ровно один раз; stale-чтений родителя нет по
//!    построению (родитель всегда вычислен до ребёнка).
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

use glam::{Mat3, Mat4, Quat, Vec3};

use crate::{component::Tick, entity::Entity, relations::ChildOf, world::World};

/// Генерационная «карта посещений» по `entity.index`: O(1) mark/contains без
/// хэширования и без пер-кадровой очистки (сравнение поколений; на wrap'е — сброс).
/// 21k hash-вставок FxHashSet на кадр стоили дороже всего остального propagate.
///
/// Публичная утилита (CR-M4) для паттерна «множество entity на кадр» — замена
/// `FxHashSet<Entity>`-на-кадр в потребителях (extract движка: shadow_markers,
/// skins active-set). Ключ — `entity.index()`; generation entity не участвует,
/// поэтому корректна только для короткоживущих (кадровых) множеств.
#[derive(Default)]
pub struct IndexStamp {
    stamps: Vec<u32>,
    generation: u32,
}

impl IndexStamp {
    /// Начать новое поколение (прежние отметки мгновенно «забываются»).
    pub fn next_generation(&mut self) {
        let (g, wrapped) = self.generation.overflowing_add(1);
        self.generation = g;
        if wrapped || g == 0 {
            // Раз в 2^32 кадров: нулевое поколение совпадает с дефолтом ячеек — чистим.
            self.stamps.iter_mut().for_each(|s| *s = u32::MAX);
            self.generation = 1;
        }
    }

    /// Отметить индекс в текущем поколении.
    #[inline]
    pub fn mark(&mut self, index: u32) {
        let i = index as usize;
        if i >= self.stamps.len() {
            self.stamps.resize(i + 1, self.generation.wrapping_sub(1));
        }
        self.stamps[i] = self.generation;
    }

    /// Отмечен ли индекс в текущем поколении.
    #[inline]
    pub fn contains(&self, index: u32) -> bool {
        self.stamps.get(index as usize) == Some(&self.generation)
    }
}

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

    /// Трансформация из координат (самый частотный конструктор, 1:1 Bevy
    /// `Transform::from_xyz`).
    #[inline]
    pub fn from_xyz(x: f32, y: f32, z: f32) -> Self {
        Self::from_translation(Vec3::new(x, y, z))
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

    // ── Builders (1:1 Bevy Transform) ────────────────────────────

    /// Заменить translation (builder).
    #[inline]
    #[must_use]
    pub fn with_translation(mut self, translation: Vec3) -> Self {
        self.translation = translation;
        self
    }

    /// Заменить rotation (builder).
    #[inline]
    #[must_use]
    pub fn with_rotation(mut self, rotation: Quat) -> Self {
        self.rotation = rotation;
        self
    }

    /// Заменить scale (builder).
    #[inline]
    #[must_use]
    pub fn with_scale(mut self, scale: Vec3) -> Self {
        self.scale = scale;
        self
    }

    /// Повернуть так, чтобы локальный forward (−Z) смотрел на `target`,
    /// а локальный +Y был выровнен к `up` без крена (1:1 Bevy
    /// `Transform::looking_at`).
    ///
    /// ⚠ Это НЕ `Quat::from_rotation_arc` — тот оставляет произвольный roll
    /// (горизонт заваливается).
    #[inline]
    #[must_use]
    pub fn looking_at(self, target: Vec3, up: Vec3) -> Self {
        self.looking_to(target - self.translation, up)
    }

    /// Повернуть так, чтобы локальный forward (−Z) смотрел вдоль `direction`
    /// (1:1 Bevy `Transform::looking_to`). См. [`Self::looking_at`].
    #[inline]
    #[must_use]
    pub fn looking_to(mut self, direction: Vec3, up: Vec3) -> Self {
        self.rotation = look_to_rotation(direction, up);
        self
    }

    // ── Направления (мировые оси локального базиса) ──────────────

    /// Локальный forward: −Z в мировых координатах.
    #[inline]
    pub fn forward(&self) -> Vec3 {
        self.rotation * Vec3::NEG_Z
    }

    /// Локальный back: +Z в мировых координатах.
    #[inline]
    pub fn back(&self) -> Vec3 {
        self.rotation * Vec3::Z
    }

    /// Локальный right: +X в мировых координатах.
    #[inline]
    pub fn right(&self) -> Vec3 {
        self.rotation * Vec3::X
    }

    /// Локальный left: −X в мировых координатах.
    #[inline]
    pub fn left(&self) -> Vec3 {
        self.rotation * Vec3::NEG_X
    }

    /// Локальный up: +Y в мировых координатах.
    #[inline]
    pub fn up(&self) -> Vec3 {
        self.rotation * Vec3::Y
    }

    /// Локальный down: −Y в мировых координатах.
    #[inline]
    pub fn down(&self) -> Vec3 {
        self.rotation * Vec3::NEG_Y
    }

    /// Преобразовать в аффинную матрицу 4x4.
    #[inline]
    pub fn to_matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

/// Кватернион «смотреть вдоль `direction` с `up` без крена» — канонический
/// look-rotation (1:1 Bevy `Transform::looking_to`): back = −direction,
/// right = up × back, up' = back × right. Вырожденные входы (нулевой/NaN
/// direction, up ∥ direction) безопасно фоллбэчатся как у Bevy
/// (`any_orthonormal_vector`).
fn look_to_rotation(direction: Vec3, up: Vec3) -> Quat {
    let back = -direction.try_normalize().unwrap_or(Vec3::NEG_Z);
    let up = up.try_normalize().unwrap_or(Vec3::Y);
    let right = up
        .cross(back)
        .try_normalize()
        .unwrap_or_else(|| up.any_orthonormal_vector());
    let up = back.cross(right);
    Quat::from_mat3(&Mat3::from_cols(right, up, back))
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

    /// Мировая трансформация из координат (для спавна корневых entity,
    /// которым матрица нужна немедленно — до первого propagate).
    #[inline]
    pub fn from_xyz(x: f32, y: f32, z: f32) -> Self {
        Self(Mat4::from_translation(Vec3::new(x, y, z)))
    }

    /// Мировая трансформация «из `eye`, смотреть на `target`» (тот же
    /// look-rotation без крена, что [`LocalTransform::looking_at`]).
    /// Типичный кейс — спавн света/камеры матрицей.
    #[inline]
    pub fn looking_at(eye: Vec3, target: Vec3, up: Vec3) -> Self {
        Self(Mat4::from_rotation_translation(
            look_to_rotation(target - eye, up),
            eye,
        ))
    }

    #[inline]
    pub fn to_matrix(&self) -> &Mat4 {
        &self.0
    }

    /// Мировой forward (−Z), 1:1 Bevy `GlobalTransform::forward`. См. [`TransformDirections`].
    #[inline]
    pub fn forward(&self) -> Vec3 {
        self.0.forward()
    }
    /// Мировой back (+Z), 1:1 Bevy `GlobalTransform::back`. См. [`TransformDirections`].
    #[inline]
    pub fn back(&self) -> Vec3 {
        self.0.back()
    }
    /// Мировой right (+X), 1:1 Bevy `GlobalTransform::right`.
    #[inline]
    pub fn right(&self) -> Vec3 {
        self.0.right()
    }
    /// Мировой left (−X), 1:1 Bevy `GlobalTransform::left`.
    #[inline]
    pub fn left(&self) -> Vec3 {
        self.0.left()
    }
    /// Мировой up (+Y), 1:1 Bevy `GlobalTransform::up`.
    #[inline]
    pub fn up(&self) -> Vec3 {
        self.0.up()
    }
    /// Мировой down (−Y), 1:1 Bevy `GlobalTransform::down`.
    #[inline]
    pub fn down(&self) -> Vec3 {
        self.0.down()
    }
}

/// Семантические аксессоры мировых направлений для матрицы трансформации (local→world),
/// 1:1 Bevy `Transform`/`GlobalTransform`: **forward = локальный −Z**, back = +Z, right = +X,
/// left = −X, up = +Y, down = −Y (каждый нормирован).
///
/// **Использовать вместо «сырых» `±matrix.z_axis`/`x_axis`/`y_axis`.** Сырой доступ к колонкам
/// легко перепутать по знаку: например, базис view-матрицы spot-света должен строиться из
/// `back()` (+Z), а не из forward (−Z) — путаница знака зеркалила теневую карту прожектора
/// (он самозатенял свой объёмный конус). Именованные направления делают знак невозможным
/// перепутать (см. `apex-engine/plans/TECH_DEBT.md`, fix 2026-06-15).
pub trait TransformDirections {
    /// Локальный −Z в мире (куда «смотрит» объект).
    fn forward(&self) -> Vec3;
    /// Локальный +Z в мире.
    fn back(&self) -> Vec3;
    /// Локальный +X в мире.
    fn right(&self) -> Vec3;
    /// Локальный −X в мире.
    fn left(&self) -> Vec3;
    /// Локальный +Y в мире.
    fn up(&self) -> Vec3;
    /// Локальный −Y в мире.
    fn down(&self) -> Vec3;
}

impl TransformDirections for Mat4 {
    #[inline]
    fn forward(&self) -> Vec3 {
        (-self.z_axis.truncate()).normalize_or_zero()
    }
    #[inline]
    fn back(&self) -> Vec3 {
        self.z_axis.truncate().normalize_or_zero()
    }
    #[inline]
    fn right(&self) -> Vec3 {
        self.x_axis.truncate().normalize_or_zero()
    }
    #[inline]
    fn left(&self) -> Vec3 {
        (-self.x_axis.truncate()).normalize_or_zero()
    }
    #[inline]
    fn up(&self) -> Vec3 {
        self.y_axis.truncate().normalize_or_zero()
    }
    #[inline]
    fn down(&self) -> Vec3 {
        (-self.y_axis.truncate()).normalize_or_zero()
    }
}

impl From<&LocalTransform> for GlobalTransform {
    /// Мировая матрица корневой entity == её локальная TRS (без родителя).
    #[inline]
    fn from(local: &LocalTransform) -> Self {
        Self(local.to_matrix())
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
    /// O(1)-проверка dirty по entity.index (генерационный stamp вместо hash-set)
    pub(crate) dirty: IndexStamp,
    /// DFS-стек спуска по поддеревьям: (entity, мировая матрица РОДИТЕЛЯ).
    pub(crate) stack: Vec<(Entity, Mat4)>,
}

/// Эксклюзивная система: пересчитывает `GlobalTransform` для всех entity,
/// чей `LocalTransform` **изменился** с прошлого запуска (`Changed<LocalTransform>`),
/// и всех их потомков. Ручной `TransformDirty` больше не нужен — после C1
/// `Changed<LocalTransform>` достоверен на всех путях мутации (`Query<Write>` и
/// `World::get_mut`). Выполняется в PostUpdate.
///
/// # Алгоритм (поддеревья от dirty-корней)
///
/// 1. Собрать entity с `Changed<LocalTransform>` (с прошлого `last_run`).
/// 2. Оставить **dirty-корни** — изменённые entity, у которых НЕТ изменённого
///    предка (остальные внутри их поддеревьев). Подъём прекращается на первом
///    dirty-предке, поэтому в типичных сценах это 1–2 lookup'а на entity.
/// 3. От каждого dirty-корня — итеративный DFS по всему поддереву с передачей
///    мировой матрицы родителя **по значению**: `Global = parent_global * Local`;
///    если `GlobalTransform` ещё нет — авто-инициализировать (DX: спавн с одним
///    `LocalTransform` достаточно). Каждый узел посещается ровно один раз и
///    строго после родителя — stale-чтений родительской матрицы нет по построению
///    (прежняя версия пересчитывала dirty-узлы под чистым промежуточным предком
///    дважды: сперва со старой матрицей, затем повторно каскадом).
///
/// Стоимость — O(|объединение dirty-поддеревьев|): статичная сцена с одним
/// мувером посещает только его поддерево, полностью анимированная — каждый узел
/// один раз. Иерархия `ChildOf` предполагается ацикличной (как и всюду в ядре).
///
/// # Change-detection
///
/// База — `scratch.last_run`; в конце пишется `world.current_tick()`. Требует
/// покадрового продвижения тика (`world.tick()` перед `run()`; авто — в C7).
///
/// # Диагностика
///
/// `APEX_PROP_TRACE=1` — лог фаз (changed-query / спуск, число dirty/посещений).
///
/// # Ресурсы
///
/// Использует [`TransformScratch`] для переиспользования буферов между кадрами.
///
/// Defensive bound on the ChildOf ancestor walk. `add_relation` rejects
/// cycle-forming edges, so a real hierarchy never approaches this; it only stops
/// a pathological/corrupt cycle from hanging the frame.
const MAX_PROPAGATE_DEPTH: usize = 1 << 20;

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
    scratch.dirty.next_generation();
    scratch.stack.clear();

    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let trace = *TRACE.get_or_init(|| std::env::var("APEX_PROP_TRACE").is_ok_and(|v| v == "1"));
    let t0 = std::time::Instant::now();

    // 1. Собираем entity с изменённым LocalTransform (с прошлого запуска) и метим их в
    //    stamp-карте. Семантика — ровно `Query<Changed<LocalTransform>>` (тот же
    //    `is_newer_than`), но прямым линейным сканом тик-колонок архетипов: генерик-
    //    итерация запроса стоила ~35нс/строку (косвенность fetch_item + клоужер),
    //    тут — узкий цикл по `Vec<Tick>` (паритет закреплён тестом
    //    `direct_tick_scan_matches_changed_query`).
    {
        let TransformScratch {
            dirty_entities,
            dirty,
            ..
        } = &mut scratch;
        if let Some(lt_id) = world.registry.get_id::<LocalTransform>() {
            for arch in &world.archetypes {
                let Some(col_idx) = arch.column_index(lt_id) else {
                    continue;
                };
                let col = &arch.columns[col_idx];
                for (tick, &entity) in col.change_ticks.iter().zip(arch.entities.iter()) {
                    if tick.get().is_newer_than(last_run) {
                        dirty_entities.push(entity);
                        dirty.mark(entity.index);
                    }
                }
            }
        }
    }
    let t1 = std::time::Instant::now();

    if scratch.dirty_entities.is_empty() {
        // Ничего не изменилось — фиксируем тик и выходим.
        scratch.last_run = this_run;
        world.insert_resource(scratch);
        return;
    }

    // 2. Сеем стек dirty-корнями: dirty entity без dirty-предка. Подъём по предкам
    //    останавливается на первом dirty (тогда entity внутри его поддерева и будет
    //    пересчитана спуском — сеять её отдельно нельзя, иначе double-process).
    //    Фаза только читает мир → при большом dirty-set распараллеливается.
    {
        let TransformScratch {
            dirty_entities,
            dirty,
            stack,
            ..
        } = &mut scratch;
        let dirty: &IndexStamp = dirty;
        let seed = |world: &World, entity: Entity| -> Option<(Entity, Mat4)> {
            let parent = world.get_relation_target(entity, ChildOf);
            let mut ancestor = parent;
            let mut depth = 0usize;
            while let Some(p) = ancestor {
                if dirty.contains(p.index) {
                    return None; // покрыта dirty-предком — пересчитается его спуском
                }
                ancestor = world.get_relation_target(p, ChildOf);
                depth += 1;
                if depth > MAX_PROPAGATE_DEPTH {
                    // Defensive: a ChildOf cycle would loop here forever.
                    // `add_relation` now rejects cycle-forming edges, so this is
                    // unreachable in practice — treat the chain as a clean root
                    // rather than hanging the frame.
                    log::error!(
                        "propagate_transforms: ChildOf ancestor chain for {entity} exceeds depth limit — possible cycle"
                    );
                    break;
                }
            }
            // Родитель чист (или отсутствует) — его мировая матрица валидна с прошлых
            // кадров; отсутствие GlobalTransform трактуем как identity (прежняя семантика).
            let parent_global = parent
                .and_then(|p| world.get::<GlobalTransform>(p))
                .map(|g| g.0)
                .unwrap_or(Mat4::IDENTITY);
            Some((entity, parent_global))
        };
        const PAR_MIN_DIRTY: usize = 4096;
        if dirty_entities.len() >= PAR_MIN_DIRTY {
            use rayon::prelude::*;
            let world_ref: &World = world;
            stack.par_extend(
                dirty_entities
                    .par_iter()
                    .filter_map(|&e| seed(world_ref, e)),
            );
        } else {
            for &entity in dirty_entities.iter() {
                if let Some(s) = seed(world, entity) {
                    stack.push(s);
                }
            }
        }
    }
    let seeds = scratch.stack.len();
    let t2 = std::time::Instant::now();

    // 3. Спуск. Два режима с одинаковой семантикой:
    //    — мало корней / мелкие поддеревья → последовательный DFS на месте;
    //    — много независимых корней (типично: толпа анимированных персонажей) →
    //      фаза A: параллельное вычисление матриц по дизъюнктным поддеревьям
    //      (только чтение &World — Sync, как в параллельных запросах), фаза B:
    //      последовательная запись (get_mut/insert требуют &mut World).
    //      Поддеревья дизъюнктны по построению (у entity один родитель, корни не
    //      вложены друг в друга), поэтому записи не конфликтуют и порядок фазы B
    //      не важен.
    //    Стратегия: **widen-then-descend**. Параллельность по КОРНЯМ эффективна (thread-local стек,
    //    без материализации уровней), но проседает, когда корней мало, а поддеревья огромные (кольцо
    //    → 10k лис → 260k узлов: 56 корней). Поэтому СНАЧАЛА расширяем фронтир корней дешёвыми
    //    последовательными уровнями, пока он не станет широким (56 → 10000 лис — обрабатываем лишь
    //    56 узлов), ПОТОМ — параллельный спуск по независимым поддеревьям широкого фронтира. Если
    //    фронтир уже широк (10000 анимированных персонажей-корней) — расширение пропускается; если
    //    дерево узкое и не ширится (цепочка) — выходим и спускаемся как есть.
    let mut visits = 0usize;
    let gt_id = world.registry.get_id::<GlobalTransform>();
    // Авто-создание GlobalTransform (entity с одним LocalTransform): в горячем пути пусто (required
    // components); структурный insert — в конце, не во время параллельного спуска.
    let mut missing: Vec<(Entity, Mat4)> = Vec::new();
    let mut frontier: Vec<(Entity, Mat4)> = scratch.stack.drain(..).collect();

    // ── Фаза widen: последовательно обрабатываем верхние (узкие) уровни, пока фронтир не станет
    //    достаточно широким для хорошей параллельности по поддеревьям. ──
    const WIDE_ENOUGH: usize = 1024;
    loop {
        if frontier.len() >= WIDE_ENOUGH || frontier.is_empty() {
            break;
        }
        let prev_len = frontier.len();
        let mut next: Vec<(Entity, Mat4)> = Vec::new();
        for &(entity, pg) in &frontier {
            if !world.is_alive(entity) {
                continue;
            }
            // Entity без LocalTransform останавливает спуск (каскад идёт только через узлы с трансформом).
            let local = match world.get::<LocalTransform>(entity) {
                Some(l) => *l,
                None => continue,
            };
            let global = pg * local.to_matrix();
            visits += 1;
            if let Some(mut gt) = world.get_mut::<GlobalTransform>(entity) {
                gt.0 = global;
            } else {
                missing.push((entity, global));
            }
            for child in world.children_of(ChildOf, entity) {
                next.push((child, global));
            }
        }
        let grew = next.len() > prev_len;
        frontier = next;
        if !grew {
            // Уровень не расширяется (узкое/цепочечное дерево) — дальше расширять смысла нет.
            break;
        }
    }

    // ── Фаза descend: параллельный спуск по независимым поддеревьям широкого фронтира. ──
    const PAR_MIN_ROOTS: usize = 64;
    if frontier.len() >= PAR_MIN_ROOTS {
        use rayon::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Записи ПРЯМО из параллельного спуска по (archetype, row)-указателям — тот же контракт, что
        // у параллельных Write-запросов (строка пишется ровно одним потоком): поддеревья дизъюнктны,
        // entity посещается один раз; структурных изменений в фазе нет (missing откладываем).
        let roots = frontier;
        let missing_par: std::sync::Mutex<Vec<(Entity, Mat4)>> = std::sync::Mutex::new(Vec::new());
        let visited = AtomicUsize::new(0);
        let world_ref: &World = world;
        roots.par_iter().for_each(|&(root, parent_global)| {
            let mut stack: Vec<(Entity, Mat4)> = Vec::with_capacity(64);
            stack.push((root, parent_global));
            let mut local_missing: Vec<(Entity, Mat4)> = Vec::new();
            let mut n = 0usize;
            while let Some((entity, pg)) = stack.pop() {
                if !world_ref.is_alive(entity) {
                    continue;
                }
                let local = match world_ref.get::<LocalTransform>(entity) {
                    Some(l) => *l,
                    None => continue,
                };
                let global = pg * local.to_matrix();
                n += 1;
                if !write_global_parallel(world_ref, gt_id, entity, global, this_run) {
                    local_missing.push((entity, global));
                }
                for child in world_ref.children_of(ChildOf, entity) {
                    stack.push((child, global));
                }
            }
            if n > 0 {
                visited.fetch_add(n, Ordering::Relaxed);
            }
            if !local_missing.is_empty() {
                missing_par.lock().unwrap().extend(local_missing);
            }
        });
        visits += visited.load(Ordering::Relaxed);
        missing.extend(missing_par.into_inner().unwrap());
    } else {
        // Последовательный DFS остатка (узкий фронтир): каждый узел ровно один раз, после родителя.
        let mut stack = frontier;
        while let Some((entity, parent_global)) = stack.pop() {
            if !world.is_alive(entity) {
                continue;
            }
            let local = match world.get::<LocalTransform>(entity) {
                Some(l) => *l,
                None => continue,
            };
            let global = parent_global * local.to_matrix();
            visits += 1;
            if let Some(mut gt) = world.get_mut::<GlobalTransform>(entity) {
                gt.0 = global;
            } else {
                missing.push((entity, global));
            }
            for child in world.children_of(ChildOf, entity) {
                stack.push((child, global));
            }
        }
    }

    // Авто-инициализация отсутствующих GlobalTransform (entity посещается ровно один раз).
    for (entity, global) in missing {
        world.insert(entity, GlobalTransform(global));
    }

    if trace {
        log::info!(
            "PROP_TRACE: total {:.2}ms | changed-query {:.2} | seed-roots {:.2} | descend {:.2} \
             | dirty {} roots {} visits {}",
            t0.elapsed().as_secs_f64() * 1000.0,
            (t1 - t0).as_secs_f64() * 1000.0,
            (t2 - t1).as_secs_f64() * 1000.0,
            t2.elapsed().as_secs_f64() * 1000.0,
            scratch.dirty_entities.len(),
            seeds,
            visits
        );
    }

    // Фиксируем тик этого запуска и возвращаем scratch для переиспользования.
    scratch.last_run = this_run;
    world.insert_resource(scratch);
}

/// Прямая запись `GlobalTransform` по (archetype, row) из параллельного спуска.
/// Возвращает `false`, если у entity ещё НЕТ компонента (нужен отложенный insert);
/// мёртвые/невалидные строки молча игнорируются (`true`) — как is_alive-фильтр.
///
/// Контракт безопасности (тот же, что у параллельных Write-запросов): вызывающий
/// гарантирует, что (а) каждая entity пишется не более чем одним потоком и
/// (б) во время фазы нет структурных изменений мира (spawn/despawn/insert/remove).
fn write_global_parallel(
    world: &World,
    gt_id: Option<crate::component::ComponentId>,
    entity: Entity,
    global: Mat4,
    tick: Tick,
) -> bool {
    let Some(cid) = gt_id else {
        // Компонент ещё не зарегистрирован (самый первый кадр) → отложенный insert.
        return false;
    };
    let Some(loc) = world.entities.get_location(entity) else {
        return true;
    };
    let arch = &world.archetypes[loc.archetype_id.as_usize()];
    let Some(col_idx) = arch.column_index(cid) else {
        return false;
    };
    let col = &arch.columns[col_idx];
    let row = loc.row as usize;
    if row >= col.len {
        return true;
    }
    // SAFETY: строка валидна (row < len); эксклюзив на строку и отсутствие структурных
    // изменений — контракт вызывающего (см. doc). GlobalTransform: Copy, без Drop.
    unsafe {
        *(col.get_ptr(row) as *mut GlobalTransform) = GlobalTransform(global);
        col.set_change_tick(row, tick);
    }
    true
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
    use crate::query::{Changed, Query};
    use crate::world::World;

    #[test]
    fn local_transform_default_is_identity() {
        let lt = LocalTransform::default();
        assert_eq!(lt.translation, Vec3::ZERO);
        assert_eq!(lt.rotation, Quat::IDENTITY);
        assert_eq!(lt.scale, Vec3::ONE);
    }

    #[test]
    fn looking_at_points_forward_at_target_without_roll() {
        let eye = Vec3::new(3.0, 4.0, 5.0);
        let target = Vec3::new(0.0, 1.0, 0.0);
        let t = LocalTransform::from_translation(eye).looking_at(target, Vec3::Y);

        // forward (−Z) смотрит точно на target
        let expected = (target - eye).normalize();
        assert!((t.forward() - expected).length() < 1e-6);
        // без крена: right горизонтален (⊥ мировому Y)
        assert!(t.right().dot(Vec3::Y).abs() < 1e-6);
        // ортонормальность базиса
        assert!((t.up().dot(t.forward())).abs() < 1e-6);
        assert!((t.rotation.length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn looking_at_degenerate_inputs_do_not_produce_nan() {
        // direction == 0 (target == eye) и up ∥ direction — не должны дать NaN.
        let t = LocalTransform::from_xyz(1.0, 2.0, 3.0).looking_at(Vec3::new(1.0, 2.0, 3.0), Vec3::Y);
        assert!(t.rotation.is_finite());
        let t = LocalTransform::IDENTITY.looking_to(Vec3::Y, Vec3::Y);
        assert!(t.rotation.is_finite());
        assert!((t.forward() - Vec3::Y).length() < 1e-6);
    }

    #[test]
    fn global_transform_constructors_match_local() {
        let eye = Vec3::new(0.0, 10.0, 5.0);
        let target = Vec3::ZERO;
        let local = LocalTransform::from_translation(eye).looking_at(target, Vec3::Y);
        let global = GlobalTransform::looking_at(eye, target, Vec3::Y);
        let from_local: GlobalTransform = (&local).into();
        // Матрицы совпадают (scale=1 у обоих путей).
        let (sa, ra, ta) = global.0.to_scale_rotation_translation();
        let (sb, rb, tb) = from_local.0.to_scale_rotation_translation();
        assert!((sa - sb).length() < 1e-6);
        assert!((ta - tb).length() < 1e-6);
        assert!(ra.dot(rb).abs() > 1.0 - 1e-6);
        assert_eq!(
            GlobalTransform::from_xyz(1.0, 2.0, 3.0).0,
            Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0))
        );
    }

    #[test]
    fn direction_accessors_match_rotation() {
        let t = LocalTransform::from_rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
        // Поворот на +90° вокруг Y: forward (−Z) → −X.
        assert!((t.forward() - Vec3::NEG_X).length() < 1e-6);
        assert!((t.back() + t.forward()).length() < 1e-6);
        assert!((t.left() + t.right()).length() < 1e-6);
        assert!((t.down() + t.up()).length() < 1e-6);
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
    ///
    /// Под Miri (и wasm32) `linkme::distributed_slice` отключён — компоненты
    /// регистрируются лениво на spawn/insert (см. `component.rs`, TD-25), поэтому
    /// авто-регистрация при пустом `World::new()` не выполняется. Тест проверяет
    /// именно linkme-путь → скипаем в этих конфигурациях.
    #[cfg_attr(any(miri, target_arch = "wasm32"), ignore)]
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
            let mut q = Query::<Write<LocalTransform>>::new_mut(&mut world);
            q.for_each_mut(|_, mut lt| {
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

    /// Dirty-узел под ЧИСТЫМ промежуточным родителем при dirty-прародителе: спуск
    /// от dirty-корня обязан пересчитать его ровно один раз и СТРОГО после
    /// прародителя (прежний топосорт упорядочивал его до пересчёта предка и
    /// чинил результат повторным каскадным проходом).
    #[test]
    fn dirty_leaf_under_clean_intermediate_uses_fresh_ancestor_global() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let gp = world.spawn((LocalTransform::from_translation(Vec3::new(1.0, 0.0, 0.0)),));
        let mid = world.spawn((LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)),));
        let leaf = world.spawn((LocalTransform::from_translation(Vec3::new(100.0, 0.0, 0.0)),));
        world.add_relation(mid, ChildOf, gp);
        world.add_relation(leaf, ChildOf, mid);

        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(leaf).unwrap().0.transform_point3(Vec3::ZERO),
            Vec3::new(111.0, 0.0, 0.0)
        );

        // Меняем прародителя и лист; промежуточный узел остаётся чистым.
        world.tick();
        world.get_mut::<LocalTransform>(gp).unwrap().translation = Vec3::new(2.0, 0.0, 0.0);
        world.get_mut::<LocalTransform>(leaf).unwrap().translation = Vec3::new(200.0, 0.0, 0.0);
        propagate_transforms(&mut world);

        // 2 (gp) + 10 (mid, чистый) + 200 (leaf) = 212.
        assert_eq!(
            world.get::<GlobalTransform>(leaf).unwrap().0.transform_point3(Vec3::ZERO),
            Vec3::new(212.0, 0.0, 0.0),
            "лист обязан считаться от СВЕЖЕЙ матрицы прародителя через чистый промежуточный узел"
        );
        // Чистый промежуточный узел тоже пересчитан (он в поддереве dirty-корня).
        assert_eq!(
            world.get::<GlobalTransform>(mid).unwrap().0.transform_point3(Vec3::ZERO),
            Vec3::new(12.0, 0.0, 0.0)
        );
    }

    /// Прямой скан тик-колонок (фаза 1) обязан давать ровно тот же dirty-набор, что
    /// эталонный `Query<Changed<LocalTransform>>` с той же базы.
    #[test]
    fn direct_tick_scan_matches_changed_query() {
        let mut world = World::new();
        TransformPlugin::register_components(&mut world);
        let mut all = Vec::new();
        for i in 0..100 {
            all.push(world.spawn((LocalTransform::from_translation(Vec3::new(
                i as f32, 0.0, 0.0,
            )),)));
        }
        // Половина сущностей — с другим составом (другой архетип в скане).
        for (i, &e) in all.iter().enumerate() {
            if i % 2 == 0 {
                world.insert(e, GlobalTransform::IDENTITY);
            }
        }
        propagate_transforms(&mut world);
        let last_run = world.resource::<TransformScratch>().last_run;

        world.tick();
        for (i, &e) in all.iter().enumerate() {
            if i % 3 == 0 {
                world.get_mut::<LocalTransform>(e).unwrap().translation.y = 5.0;
            }
        }

        // Эталон — генерик-запрос с той же базы.
        let mut expected: Vec<Entity> = Vec::new();
        {
            let q = Query::<Changed<LocalTransform>>::new_with_tick(&world, last_run);
            q.for_each(|e, _| expected.push(e));
        }

        propagate_transforms(&mut world);
        let mut actual = world.resource::<TransformScratch>().dirty_entities.clone();
        expected.sort_by_key(|e| e.index);
        actual.sort_by_key(|e| e.index);
        assert!(!expected.is_empty());
        assert_eq!(actual, expected, "прямой скан тиков должен совпадать с Changed-запросом");
    }

    /// Параллельная ветка спуска (≥64 независимых dirty-корней): результат идентичен
    /// последовательному — у каждого ребёнка global = parent.local * child.local.
    #[test]
    fn many_dirty_roots_take_parallel_descent_and_stay_correct() {
        use crate::query::Write;

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let n = 200; // > PAR_MIN_ROOTS
        let mut pairs = Vec::new();
        for i in 0..n {
            let parent =
                world.spawn((LocalTransform::from_translation(Vec3::new(i as f32, 0.0, 0.0)),));
            let child =
                world.spawn((LocalTransform::from_translation(Vec3::new(0.0, 1.0, 0.0)),));
            world.add_relation(child, ChildOf, parent);
            pairs.push((parent, child));
        }
        propagate_transforms(&mut world);

        // Все родители dirty одновременно → n независимых поддеревьев → parallel-ветка.
        world.tick();
        {
            let mut q = Query::<Write<LocalTransform>>::new_mut(&mut world);
            q.for_each_mut(|_, mut lt| {
                if lt.translation.y == 0.0 {
                    lt.translation.x += 1000.0;
                }
            });
        }
        propagate_transforms(&mut world);

        for (i, (parent, child)) in pairs.iter().enumerate() {
            assert_eq!(
                world.get::<GlobalTransform>(*parent).unwrap().0.transform_point3(Vec3::ZERO),
                Vec3::new(1000.0 + i as f32, 0.0, 0.0)
            );
            assert_eq!(
                world.get::<GlobalTransform>(*child).unwrap().0.transform_point3(Vec3::ZERO),
                Vec3::new(1000.0 + i as f32, 1.0, 0.0),
                "child {i} должен пересчитаться от свежего родителя в параллельной ветке"
            );
        }
    }

    /// Глубокая/широкая иерархия с НЕМНОГИМИ корнями (кейс ring-parent many_foxes): параллелизм
    /// должен идти по ШИРИНЕ уровня, а не по числу корней. 1 корень → 300 детей (широкий уровень >
    /// PAR_MIN_LEVEL → параллельная ветка) → у каждого внук. Сдвиг ТОЛЬКО корня каскадирует на 600
    /// потомков; родитель уровня передаётся детям по значению через барьер уровня.
    #[test]
    fn deep_wide_hierarchy_few_roots_propagates_in_parallel_levels() {
        use crate::query::Write;

        let mut world = World::new();
        TransformPlugin::register_components(&mut world);

        let root = world.spawn((LocalTransform::from_translation(Vec3::new(10.0, 0.0, 0.0)),));
        let mut grandkids = Vec::new();
        for i in 0..300 {
            let child =
                world.spawn((LocalTransform::from_translation(Vec3::new(0.0, i as f32, 0.0)),));
            world.add_relation(child, ChildOf, root);
            let gk = world.spawn((LocalTransform::from_translation(Vec3::new(0.0, 0.0, 1.0)),));
            world.add_relation(gk, ChildOf, child);
            grandkids.push((i, gk));
        }
        propagate_transforms(&mut world);

        // Сдвигаем ТОЛЬКО корень → пересчёт каскадирует на 600 потомков через широкие уровни.
        world.tick();
        {
            let mut q = Query::<Write<LocalTransform>>::new_mut(&mut world);
            q.for_each_mut(|e, mut lt| {
                if e == root {
                    lt.translation.x = 1000.0;
                }
            });
        }
        propagate_transforms(&mut world);

        // Внук i: root(1000,0,0) + child(0,i,0) + gk(0,0,1) = (1000, i, 1).
        for (i, gk) in grandkids {
            assert_eq!(
                world.get::<GlobalTransform>(gk).unwrap().0.transform_point3(Vec3::ZERO),
                Vec3::new(1000.0, i as f32, 1.0),
                "внук {i} пересчитан от свежего корня через параллельный уровень"
            );
        }
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
            let mut q = Query::<Write<LocalTransform>>::new_mut(&mut world);
            q.for_each_mut(|e, mut lt| {
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

    #[test]
    fn transform_directions_match_bevy_signs() {
        // 1:1 Bevy: forward = −Z, back = +Z, right = +X, left = −X, up = +Y, down = −Y. Pins the
        // sign convention so the spot-shadow basis flip (TECH_DEBT 2026-06-15) can't recur silently.
        // Identity transform → world axes line up with local.
        let m = Mat4::IDENTITY;
        assert!((m.forward() - Vec3::NEG_Z).length() < 1e-6);
        assert!((m.back() - Vec3::Z).length() < 1e-6);
        assert!((m.right() - Vec3::X).length() < 1e-6);
        assert!((m.left() - Vec3::NEG_X).length() < 1e-6);
        assert!((m.up() - Vec3::Y).length() < 1e-6);
        assert!((m.down() - Vec3::NEG_Y).length() < 1e-6);

        // forward()/back() are exact opposites; `GlobalTransform` delegates to the same trait and
        // agrees with `LocalTransform` (built from the same rotation) for an arbitrary orientation.
        let lt = LocalTransform::from_xyz(1.0, 2.0, 3.0).looking_at(Vec3::new(4.0, 0.0, -1.0), Vec3::Y);
        let gt = GlobalTransform(lt.to_matrix());
        assert!((gt.forward() - lt.forward()).length() < 1e-5, "GlobalTransform.forward == LocalTransform.forward");
        assert!((gt.back() - lt.back()).length() < 1e-5);
        assert!((gt.right() - lt.right()).length() < 1e-5);
        assert!((gt.up() - lt.up()).length() < 1e-5);
        assert!((gt.forward() + gt.back()).length() < 1e-5, "forward = -back");
    }
}
