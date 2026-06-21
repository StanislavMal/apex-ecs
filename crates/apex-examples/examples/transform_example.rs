//! Apex ECS — TransformPropagation Example
//!
//! Демонстрирует иерархические трансформации (новый DX после C1/C2):
//! - [`LocalTransform`] — позиция/поворот/масштаб относительно родителя
//! - [`GlobalTransform`] — итоговая мировая матрица (создаётся и пересчитывается
//!   автоматически системой `propagate_transforms`)
//! - dirty-детекция через `Changed<LocalTransform>` — **ручной `TransformDirty`
//!   больше не нужен**; достаточно изменить `LocalTransform`.
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

use apex_core::entity::Entity;
use apex_core::prelude::*;
use apex_core::query::{Changed, Query};
use apex_core::transform::{self, GlobalTransform, LocalTransform, TransformPlugin};
use apex_scheduler::{seq, Scheduler, StageLabel};
use glam::Vec3;

fn main() {
    println!("=== Apex ECS — TransformPropagation Example ===\n");

    // ── 1. Создаём мир и регистрируем компоненты ────────────────
    let mut world = World::new();
    TransformPlugin::register_components(&mut world);

    // ── 2. Создаём Scheduler с propagate_transforms в PostUpdate ─
    let mut sched = Scheduler::new();
    sched.add_systems(
        StageLabel::PostUpdate,
        apex_scheduler::seq("propagate_transforms", transform::propagate_transforms),
    );
    sched.compile().unwrap();

    println!("Scheduler plan:\n{}\n", sched.debug_plan());

    // ── 3. Создаём иерархию Grandparent → Parent → Child ────────
    //     Спавним только LocalTransform — GlobalTransform создаст propagate.
    println!("--- Создание иерархии ---\n");

    let grandparent = create_transform_entity(&mut world, "Grandparent", Vec3::new(50.0, 0.0, 0.0), None);
    let parent = create_transform_entity(&mut world, "Parent", Vec3::new(30.0, 0.0, 0.0), Some(grandparent));
    let child = create_transform_entity(&mut world, "Child", Vec3::new(20.0, 0.0, 0.0), Some(parent));

    // ── 4. Запускаем propagate_transforms ────────────────────────
    //     Все три entity «изменены» (только что заспавнены) → пересчитаются,
    //     GlobalTransform авто-инициализируется.
    println!("\n--- Запуск propagate_transforms (PostUpdate) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── 5. Проверяем результаты ──────────────────────────────────
    println!("--- Результаты после пропагации ---\n");
    print_entity(&world, grandparent, "Grandparent");
    print_entity(&world, parent, "Parent");
    print_entity(&world, child, "Child");

    let gp_pos = world.get::<GlobalTransform>(grandparent).unwrap().0.transform_point3(Vec3::ZERO);
    let p_pos = world.get::<GlobalTransform>(parent).unwrap().0.transform_point3(Vec3::ZERO);
    let c_pos = world.get::<GlobalTransform>(child).unwrap().0.transform_point3(Vec3::ZERO);

    assert_eq!(gp_pos, Vec3::new(50.0, 0.0, 0.0), "Grandparent должен быть на (50,0,0)");
    assert_eq!(p_pos, Vec3::new(80.0, 0.0, 0.0), "Parent должен быть на (80,0,0)");
    assert_eq!(c_pos, Vec3::new(100.0, 0.0, 0.0), "Child должен быть на (100,0,0)");
    println!("\n✅ Все проверки пройдены!\n");

    // ── 6. Демонстрация: изменение LocalTransform у родителя ─────
    //     Никаких ручных маркеров: меняем LocalTransform → Changed<LocalTransform>
    //     достоверен (C1) → propagate пересчитает Parent и каскадирует на Child.
    println!("--- Изменение LocalTransform у Parent (без ручного dirty) ---\n");

    // Модель кадра: тик продвигается В НАЧАЛЕ кадра (до мутаций). В реальном
    // приложении это делает планировщик (C7); здесь — вручную.
    world.tick();
    if let Some(lt) = world.get_mut::<LocalTransform>(parent) {
        lt.translation = Vec3::new(100.0, 0.0, 0.0);
    }
    transform::propagate_transforms(&mut world);

    let p_pos2 = world.get::<GlobalTransform>(parent).unwrap().0.transform_point3(Vec3::ZERO);
    let c_pos2 = world.get::<GlobalTransform>(child).unwrap().0.transform_point3(Vec3::ZERO);

    // Parent — ребёнок Grandparent(50): 50 + 100 = 150. Child: 150 + 20 = 170.
    println!("Parent после изменения: ({:.1}, {:.1}, {:.1}) — ожидается (150, 0, 0)", p_pos2.x, p_pos2.y, p_pos2.z);
    println!("Child после изменения:  ({:.1}, {:.1}, {:.1}) — ожидается (170, 0, 0)", c_pos2.x, c_pos2.y, c_pos2.z);

    assert_eq!(p_pos2, Vec3::new(150.0, 0.0, 0.0), "Parent должен быть на (150,0,0)");
    assert_eq!(c_pos2, Vec3::new(170.0, 0.0, 0.0), "Child должен быть на (170,0,0) каскадно");

    println!("\n✅ Изменение LocalTransform родителя каскадно распространилось на детей!\n");

    // ── 7. Cross-stage change detection (TD-52) ─────────────────
    //     `GlobalTransform` пишется в PostUpdate (`propagate_transforms`), а потребитель часто читает
    //     `Changed<GlobalTransform>` в более РАННЕЙ стадии (PreUpdate — пикинг/BVH, экстракт). Запись
    //     поздней стадии кадра N обязана быть видна ранней стадии кадра N+1. Раньше per-frame change-tick
    //     это терял; per-stage change-window (планировщик) — чинит. Самодостаточная проверка:
    println!("--- Cross-stage change detection (PostUpdate → PreUpdate next frame, TD-52) ---\n");
    {
        #[derive(Default)]
        struct Detected(u32); // сколько изменений GlobalTransform увидел PreUpdate-наблюдатель

        let mut w = World::new();
        TransformPlugin::register_components(&mut w);
        w.insert_resource(Detected::default());
        let ent = w.spawn((LocalTransform::from_translation(Vec3::ZERO),));

        let mut s = Scheduler::new();
        // PreUpdate: считаем сущности с Changed<GlobalTransform> с прошлого запуска ЭТОЙ стадии.
        s.add_systems(
            StageLabel::PreUpdate,
            seq("observe_global", |w: &mut World| {
                let last = w.last_run_tick();
                let n = Query::<(Changed<GlobalTransform>,)>::new_with_tick(w, last).iter().count();
                w.resource_mut::<Detected>().0 += n as u32;
            }),
        );
        // PostUpdate: пересчёт мировых трансформов (пишет GlobalTransform).
        s.add_systems(StageLabel::PostUpdate, seq("propagate", transform::propagate_transforms));

        s.run(&mut w); // кадр 0: PostUpdate создаёт GlobalTransform
        s.run(&mut w); // кадр 1: PreUpdate видит новый GlobalTransform из кадра 0 → база стабилизируется
        let baseline = w.resource::<Detected>().0;

        // Двигаем объект: LocalTransform меняется СЕЙЧАС, GlobalTransform обновится в PostUpdate кадра A.
        if let Some(lt) = w.get_mut::<LocalTransform>(ent) {
            lt.translation = Vec3::new(7.0, 0.0, 0.0);
        }
        s.run(&mut w); // кадр A: PreUpdate (GT ещё старый) → PostUpdate propagate пишет новый GT
        let mid = w.resource::<Detected>().0;
        s.run(&mut w); // кадр B: PreUpdate ДЕТЕКТИТ GT, записанный в PostUpdate кадра A (cross-stage!)
        let end = w.resource::<Detected>().0;

        println!("  GlobalTransform-изменений увидел PreUpdate: baseline={baseline}, кадр A={mid}, кадр B={end}");
        assert_eq!(mid, baseline, "PreUpdate кадра-движения идёт ДО записи GT в PostUpdate того же кадра");
        assert_eq!(end, baseline + 1, "PreUpdate СЛЕДУЮЩЕГО кадра обязан увидеть тот GT (cross-stage, TD-52)");
        println!("\n✅ Cross-stage change detection: запись PostUpdate кадра A видна PreUpdate кадра B.\n");
    }

    println!("=== Done ===");
    println!("Final entities:   {}", world.entity_count());
    println!("Final tick:       {:?}", world.current_tick());
}

// ── Вспомогательные функции ─────────────────────────────────────

/// Создаёт entity с одним `LocalTransform` и прикрепляет к родителю (если указан).
/// `GlobalTransform` будет создан автоматически системой `propagate_transforms`.
fn create_transform_entity(
    world: &mut World,
    name: &'static str,
    translation: Vec3,
    parent: Option<Entity>,
) -> Entity {
    let entity = world.spawn((LocalTransform::from_translation(translation),));
    world.insert(entity, DebugName(name));

    if let Some(p) = parent {
        world.add_relation(entity, ChildOf, p);
        println!("  [{}] создан, parent={:?}, local=({:.1}, {:.1}, {:.1})", name, p, translation.x, translation.y, translation.z);
    } else {
        println!("  [{}] создан (корень), local=({:.1}, {:.1}, {:.1})", name, translation.x, translation.y, translation.z);
    }

    entity
}

/// Печатает имя и мировую позицию entity.
fn print_entity(world: &World, entity: Entity, label: &str) {
    let name = world.get::<DebugName>(entity).map(|n| n.0).unwrap_or("?");
    if let Some(gt) = world.get::<GlobalTransform>(entity) {
        let pos = gt.0.transform_point3(Vec3::ZERO);
        println!("  {:<24} pos=({:6.1}, {:6.1}, {:6.1})", format!("{} ({})", name, label), pos.x, pos.y, pos.z);
    }
}

// ── Компонент для отладки ────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)]
struct DebugName(&'static str);
