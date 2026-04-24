//! Apex ECS — TransformPropagation Example
//!
//! Демонстрирует иерархические трансформации:
//! - [`LocalTransform`] — позиция/поворот/масштаб относительно родителя
//! - [`GlobalTransform`] — итоговая мировая матрица (пересчитывается автоматически)
//! - [`TransformDirty`] — маркер для инкрементального пересчёта
//! - [`propagate_transforms`] — система, выполняющая DFS propagation в PostUpdate
//!
//! # Запуск
//!
//! ```bash
//! cargo run -p apex-examples --example transform_example
//! ```
//!
//! # Иерархия в примере
//!
//! ```text
//! Grandparent (50, 0, 0)
//!   └── Parent (30, 0, 0)
//!         └── Child (20, 0, 0)
//! ```
//!
//! Ожидаемый результат:
//! - Grandparent.Global = (50, 0, 0)
//! - Parent.Global     = (80, 0, 0)   ← 50 + 30
//! - Child.Global      = (100, 0, 0)  ← 80 + 20
//! - TransformDirty снят со всех entity после пропагации

use apex_core::entity::Entity;
use apex_core::prelude::*;
use apex_core::transform::{self, LocalTransform, GlobalTransform, TransformDirty, TransformPlugin};
use apex_scheduler::{Scheduler, StageLabel};
use glam::Vec3;

fn main() {
    println!("=== Apex ECS — TransformPropagation Example ===\n");

    // ── 1. Создаём мир и регистрируем компоненты ────────────────
    let mut world = World::new();
    TransformPlugin::register_components(&mut world);

    // ── 2. Создаём Scheduler с propagate_transforms в PostUpdate ─
    let mut sched = Scheduler::new();

    // Добавляем propagate_transforms как sequential систему в PostUpdate
    sched.add_system_to_stage(
        "propagate_transforms",
        transform::propagate_transforms,
        StageLabel::PostUpdate,
    );

    sched.compile().unwrap();

    println!("Scheduler plan:\n{}\n", sched.debug_plan());

    // ── 3. Создаём иерархию Grandparent → Parent → Child ────────
    println!("--- Создание иерархии ---\n");

    let grandparent = create_transform_entity(
        &mut world, "Grandparent",
        Vec3::new(50.0, 0.0, 0.0),
        None, // нет родителя
    );

    let parent = create_transform_entity(
        &mut world, "Parent",
        Vec3::new(30.0, 0.0, 0.0),
        Some(grandparent),
    );

    let child = create_transform_entity(
        &mut world, "Child",
        Vec3::new(20.0, 0.0, 0.0),
        Some(parent),
    );

    // ── 4. Устанавливаем GlobalTransform для корня (grandparent) ─
    // Корень не имеет TransformDirty, но его GlobalTransform нужно
    // инициализировать вручную (или через propagate_transforms).
    let gp_local = *world.get::<LocalTransform>(grandparent).unwrap();
    if let Some(gt) = world.get_mut::<GlobalTransform>(grandparent) {
        gt.0 = gp_local.to_matrix();
    }

    print_entity(&world, grandparent, "Grandparent (before)");
    print_entity(&world, parent,     "Parent (before)");
    print_entity(&world, child,      "Child (before)");

    // ── 5. Запускаем propagate_transforms ────────────────────────
    println!("\n--- Запуск propagate_transforms (PostUpdate) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── 6. Проверяем результаты ──────────────────────────────────
    println!("--- Результаты после пропагации ---\n");

    print_entity(&world, grandparent, "Grandparent");
    print_entity(&world, parent,     "Parent");
    print_entity(&world, child,      "Child");

    // Проверяем, что TransformDirty снят со всех
    let dirty_count = count_dirty(&world);
    println!("Entity с TransformDirty: {} (должно быть 0)", dirty_count);

    // Валидация
    println!("\n--- Валидация ---\n");

    let gp_global = world.get::<GlobalTransform>(grandparent).unwrap();
    let p_global  = world.get::<GlobalTransform>(parent).unwrap();
    let c_global  = world.get::<GlobalTransform>(child).unwrap();

    let gp_pos = gp_global.0.transform_point3(Vec3::ZERO);
    let p_pos  = p_global.0.transform_point3(Vec3::ZERO);
    let c_pos  = c_global.0.transform_point3(Vec3::ZERO);

    assert_eq!(gp_pos, Vec3::new(50.0, 0.0, 0.0), "Grandparent должен быть на (50,0,0)");
    assert_eq!(p_pos,  Vec3::new(80.0, 0.0, 0.0), "Parent должен быть на (80,0,0)");
    assert_eq!(c_pos,  Vec3::new(100.0, 0.0, 0.0), "Child должен быть на (100,0,0)");
    assert_eq!(dirty_count, 0, "TransformDirty должен быть снят со всех entity");

    println!("✅ Все проверки пройдены!\n");

    // ── 7. Демонстрация: изменение LocalTransform у родителя ─────
    //     propagate_transforms использует top-down DFS, поэтому
    //     Parent и Child можно оба пометить dirty — порядок
    //     обработки будет корректным (сначала предки, потом потомки).
    println!("--- Изменение LocalTransform у Parent ---\n");

    // Меняем локальную позицию Parent
    if let Some(lt) = world.get_mut::<LocalTransform>(parent) {
        lt.translation = Vec3::new(100.0, 0.0, 0.0);
    }

    // Помечаем и Parent, и Child как dirty
    world.insert(parent, TransformDirty);
    world.insert(child, TransformDirty);

    // Запускаем propagate_transforms напрямую (не через Scheduler)
    println!("  Parent и Child помечены dirty, запуск propagate_transforms...\n");
    world.tick();
    transform::propagate_transforms(&mut world);

    // Проверяем новые позиции
    let p_pos2 = world.get::<GlobalTransform>(parent).unwrap()
        .0.transform_point3(Vec3::ZERO);
    let c_pos2 = world.get::<GlobalTransform>(child).unwrap()
        .0.transform_point3(Vec3::ZERO);

    println!("Parent после изменения: ({:.1}, {:.1}, {:.1}) — ожидается (150, 0, 0)",
        p_pos2.x, p_pos2.y, p_pos2.z);
    println!("Child после изменения:  ({:.1}, {:.1}, {:.1}) — ожидается (170, 0, 0)",
        c_pos2.x, c_pos2.y, c_pos2.z);

    assert_eq!(p_pos2, Vec3::new(150.0, 0.0, 0.0), "Parent должен быть на (150,0,0)");
    assert_eq!(c_pos2, Vec3::new(170.0, 0.0, 0.0), "Child должен быть на (170,0,0)");

    let dirty_count2 = count_dirty(&world);
    assert_eq!(dirty_count2, 0, "TransformDirty должен быть снят после второго запуска");

    println!("✅ Изменение LocalTransform родителя корректно распространилось на детей!\n");

    println!("=== Done ===");
    println!("Final entities:   {}", world.entity_count());
    println!("Final archetypes: {}", world.archetype_count());
    println!("Final tick:       {:?}", world.current_tick());
}

// ── Вспомогательные функции ─────────────────────────────────────

/// Создаёт entity с трансформациями и прикрепляет к родителю (если указан).
fn create_transform_entity(
    world: &mut World,
    name: &'static str,
    translation: Vec3,
    parent: Option<Entity>,
) -> Entity {
    let entity = world
        .spawn()
        .insert(transform::LocalTransform::from_translation(translation))
        .insert(transform::GlobalTransform::default())
        .insert(transform::TransformDirty)
        .id();

    // Добавляем имя для отладки
    world.insert(entity, DebugName(name));

    if let Some(p) = parent {
        world.add_relation(entity, ChildOf, p);
        println!("  [{}] создан, parent={:?}, local=({:.1}, {:.1}, {:.1})",
            name, p, translation.x, translation.y, translation.z);
    } else {
        println!("  [{}] создан (корень), local=({:.1}, {:.1}, {:.1})",
            name, translation.x, translation.y, translation.z);
    }

    entity
}

/// Печатает информацию об entity: имя, GlobalTransform, dirty-флаг.
fn print_entity(world: &World, entity: Entity, label: &str) {
    let name = world.get::<DebugName>(entity)
        .map(|n| n.0)
        .unwrap_or("?");

    let global_pos = world.get::<GlobalTransform>(entity)
        .map(|gt| gt.0.transform_point3(Vec3::ZERO));

    let has_dirty = world.get::<TransformDirty>(entity).is_some();

    if let Some(pos) = global_pos {
        let dirty_flag = if has_dirty { " [DIRTY]" } else { "" };
        println!("  {:<20} pos=({:6.1}, {:6.1}, {:6.1}){}",
            format!("{} ({})", name, label),
            pos.x, pos.y, pos.z, dirty_flag);
    }
}

/// Считает количество entity с TransformDirty.
fn count_dirty(world: &World) -> usize {
    let q = world.query_typed::<Read<TransformDirty>>();
    let mut count = 0;
    q.for_each(|_, _| count += 1);
    count
}

// ── Компонент для отладки ────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct DebugName(&'static str);
