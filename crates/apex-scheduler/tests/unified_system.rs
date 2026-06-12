//! U: единый `system!` для параллельных и эксклюзивных систем + единый вход
//! регистрации `add_systems` с bare-идентификаторами (имя выводится из fn).

use apex_core::prelude::*;
use apex_core::{system, World};
use apex_macros::Component;
use apex_scheduler::{Scheduler, StageLabel};

#[derive(Component)]
struct Counter(u32);

// Параллельная система — доступ выведен из параметров (Write<Counter>).
system! {
    fn parallel_inc(q: Write<Counter>) {
        q.for_each(|_, mut c| {
            c.0 += 1;
        });
    }
}

// Эксклюзивная система — ТОТ ЖЕ макрос, режим выбран по `world: &mut World`.
system! {
    fn exclusive_spawn(world: &mut World) {
        world.spawn((Counter(100),));
    }
}

/// `add_systems(stage, (parallel, exclusive))` — bare-идентификаторы,
/// имена выведены из fn; маркер-дизамбигуация различает Auto/Exclusive.
#[test]
fn unified_add_systems_bare_identifiers() {
    let mut world = World::new();
    world.spawn((Counter(0),));

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, (parallel_inc, exclusive_spawn));
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);

    let counters: Vec<u32> = Query::<Read<Counter>>::new(&world)
        .iter()
        .map(|c| c.0)
        .collect();

    // exclusive_spawn создал второй entity ⇒ всего 2.
    assert_eq!(counters.len(), 2, "exclusive система должна была заспавнить entity");
    // parallel_inc проинкрементил исходный Counter(0) ⇒ ни один счётчик не остался 0.
    assert!(
        counters.iter().all(|&v| v > 0),
        "параллельная система должна была исполниться (counters={counters:?})"
    );
}

// State без обязательного Default (U.5) — поля pub, конструируется вручную.
system! {
    struct Accumulator { step: u32 }
    fn run(s: &mut Self, q: Write<Counter>) {
        q.for_each(|_, mut c| {
            c.0 += s.step;
        });
    }
}

/// Система со state без `Default`: регистрируется значением через `add_systems`.
#[test]
fn stateful_system_without_default() {
    let mut world = World::new();
    world.spawn((Counter(0),));

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, Accumulator { step: 5 });
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    sched.run_sequential(&mut world);

    let v = Query::<Read<Counter>>::new(&world)
        .iter()
        .next()
        .map(|c| c.0)
        .unwrap();
    assert_eq!(v, 10, "state-система без Default должна аккумулировать (2×step)");
}

/// Эксклюзивная система исполняется и видит/меняет весь мир.
#[test]
fn exclusive_system_full_world_access() {
    let mut world = World::new();
    world.spawn((Counter(5),));
    world.spawn((Counter(7),));

    let mut sched = Scheduler::new();
    // Регистрация через builder-метод (для .id()/условий).
    sched.add_exclusive_system(exclusive_spawn);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);

    assert_eq!(world.entity_count(), 3, "эксклюзивная система должна заспавнить 1 entity");
}

// ── TD-9: достоверный Changed<T> внутри систем ──────────────────

struct ChangedCount(usize);

system! {
    fn detect_changed(q: (Changed<Counter>, Read<Counter>), out: ResMut<ChangedCount>) {
        let mut n = 0;
        q.for_each(|_, _| n += 1);
        out.0 = n;
    }
}

/// `Changed<T>` внутри системы детектит только мутации текущего кадра —
/// планировщик продвигает change-tick на границе кадра (база — `last_run_tick`).
#[test]
fn changed_in_system_detects_only_mutated() {
    let mut world = World::new();
    let e0 = world.spawn((Counter(0),));
    let _e1 = world.spawn((Counter(0),));
    world.insert_resource(ChangedCount(0));

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, detect_changed);

    // Кадр 1: все entity «новые» (база last_run = 0) → detect видит обе.
    sched.run_sequential(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 2, "кадр 1: все entity новые");

    // Мутируем ТОЛЬКО e0 между кадрами.
    world.get_mut::<Counter>(e0).unwrap().0 = 99;

    // Кадр 2: detect видит только изменённый e0.
    sched.run_sequential(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 1, "кадр 2: только изменённый");

    // Кадр 3: изменений нет → 0.
    sched.run_sequential(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 0, "кадр 3: изменений нет");
}

/// То же, но через параллельный путь `run()` (run_hybrid_parallel).
#[test]
fn changed_in_system_parallel_path() {
    let mut world = World::new();
    let e0 = world.spawn((Counter(0),));
    for _ in 0..50 {
        world.spawn((Counter(0),));
    }
    world.insert_resource(ChangedCount(0));

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, detect_changed);

    sched.run(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 51, "кадр 1: все новые (parallel)");

    world.get_mut::<Counter>(e0).unwrap().0 = 7;

    sched.run(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 1, "кадр 2: только изменённый (parallel)");
}
