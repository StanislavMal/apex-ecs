//! U: unified `system!` for parallel and exclusive systems + a single
//! registration entry point `add_systems` with bare identifiers (name derived from fn).

use apex_core::prelude::*;
use apex_core::{system, World};
use apex_macros::Component;
use apex_scheduler::{Scheduler, StageLabel};

#[derive(Component)]
struct Counter(u32);

// Parallel system — access derived from the parameters (&mut Counter).
system! {
    fn parallel_inc(q: (&mut Counter,)) {
        q.for_each_mut(|_, (mut c,)| {
            c.0 += 1;
        });
    }
}

// Exclusive system — THE SAME macro, mode chosen by `world: &mut World`.
system! {
    fn exclusive_spawn(world: &mut World) {
        world.spawn((Counter(100),));
    }
}

/// `add_systems(stage, (parallel, exclusive))` — bare identifiers,
/// names derived from fn; marker disambiguation distinguishes Auto/Exclusive.
#[test]
fn unified_add_systems_bare_identifiers() {
    let mut world = World::new();
    world.spawn((Counter(0),));

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, (parallel_inc, exclusive_spawn));
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);

    let counters: Vec<u32> = Query::<&Counter>::new(&world)
        .iter()
        .map(|c| c.0)
        .collect();

    // exclusive_spawn created a second entity ⇒ 2 in total.
    assert_eq!(counters.len(), 2, "exclusive system should have spawned an entity");
    // parallel_inc incremented the original Counter(0) ⇒ no counter stayed at 0.
    assert!(
        counters.iter().all(|&v| v > 0),
        "parallel system should have run (counters={counters:?})"
    );
}

// State without a required Default (U.5) — pub fields, constructed manually.
system! {
    struct Accumulator { step: u32 }
    fn run(s: &mut Self, q: (&mut Counter,)) {
        q.for_each_mut(|_, (mut c,)| {
            c.0 += s.step;
        });
    }
}

/// A stateful system without `Default`: registered by value via `add_systems`.
#[test]
fn stateful_system_without_default() {
    let mut world = World::new();
    world.spawn((Counter(0),));

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, Accumulator { step: 5 });
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    sched.run_sequential(&mut world);

    let v = Query::<&Counter>::new(&world)
        .iter()
        .next()
        .map(|c| c.0)
        .unwrap();
    assert_eq!(v, 10, "stateful system without Default should accumulate (2×step)");
}

/// An exclusive system runs and sees/mutates the whole world.
#[test]
fn exclusive_system_full_world_access() {
    let mut world = World::new();
    world.spawn((Counter(5),));
    world.spawn((Counter(7),));

    let mut sched = Scheduler::new();
    // Bare identifier of an exclusive system! in the unified add_systems entry.
    sched.add_systems(StageLabel::Update, exclusive_spawn);
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);

    assert_eq!(world.entity_count(), 3, "exclusive system should spawn 1 entity");
}

// ── TD-9: reliable Changed<T> inside systems ──────────────────

struct ChangedCount(usize);

system! {
    fn detect_changed(q: (Changed<Counter>, &Counter), out: ResMut<ChangedCount>) {
        let mut n = 0;
        q.for_each(|_, _| n += 1);
        out.0 = n;
    }
}

/// `Changed<T>` inside a system detects only current-frame mutations —
/// the scheduler advances the change-tick at the frame boundary (base is
/// `last_run_tick`).
#[test]
fn changed_in_system_detects_only_mutated() {
    let mut world = World::new();
    let e0 = world.spawn((Counter(0),));
    let _e1 = world.spawn((Counter(0),));
    world.insert_resource(ChangedCount(0));

    let mut sched = Scheduler::new();
    sched.add_systems(StageLabel::Update, detect_changed);

    // Frame 1: all entities are "new" (base last_run = 0) → detect sees both.
    sched.run_sequential(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 2, "frame 1: all entities new");

    // Mutate ONLY e0 between frames.
    world.get_mut::<Counter>(e0).unwrap().0 = 99;

    // Frame 2: detect sees only the changed e0.
    sched.run_sequential(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 1, "frame 2: only the changed one");

    // Frame 3: no changes → 0.
    sched.run_sequential(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 0, "frame 3: no changes");
}

/// Same, but via the parallel path `run()` (run_hybrid_parallel).
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
    assert_eq!(world.resource::<ChangedCount>().0, 51, "frame 1: all new (parallel)");

    world.get_mut::<Counter>(e0).unwrap().0 = 7;

    sched.run(&mut world);
    assert_eq!(world.resource::<ChangedCount>().0, 1, "frame 2: only the changed one (parallel)");
}

// ── ADR-022: visibility, attributes, and the function form ──────────────────

/// Systems declared in one module and scheduled from another — the field report (HEAD 213f303):
/// `system!` took no visibility, so systems and their schedule had to share a module.
mod systems_elsewhere {
    use super::Counter;
    use apex_core::system;

    system! {
        /// A documented public parallel system.
        pub fn public_inc(q: (&mut Counter,)) {
            q.for_each_mut(|_, (mut c,)| {
                c.0 += 10;
            });
        }
    }

    system! {
        pub(crate) fn public_spawn(world: &mut World) {
            world.spawn((Counter(1000),));
        }
    }

    system! {
        /// A stateful system visible outside its module; its fields are public (no-default
        /// form). A struct's `AutoSystem` impl names its components, so the struct can be no
        /// more visible than they are (E0446) — the function form hides them behind `impl`.
        pub(crate) struct PublicAccumulator { step: u32 }
        fn run(s: &mut Self, q: (&mut Counter,)) {
            q.for_each_mut(|_, (mut c,)| {
                c.0 += s.step;
            });
        }
    }
}

#[test]
fn pub_systems_from_another_module_register_and_order_from_here() {
    use apex_scheduler::IntoScheduleConfigs;
    use systems_elsewhere::{public_inc, public_spawn, PublicAccumulator};

    let mut world = World::new();
    world.spawn((Counter(0),));

    let mut sched = Scheduler::new();
    // Typed ordering targets resolve to the function's name — the registered one.
    sched.add_systems(
        StageLabel::Update,
        (
            public_spawn,
            public_inc.after(public_spawn),
            PublicAccumulator { step: 1 }.after(public_inc),
        ),
    );
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);

    let mut values: Vec<u32> = Query::<&Counter>::new(&world).iter().map(|c| c.0).collect();
    values.sort_unstable();
    // spawn (1000) → +10 both → +1 both.
    assert_eq!(values, vec![11, 1011]);

    let plan = sched.debug_plan();
    assert!(plan.contains("public_inc") && plan.contains("public_spawn"), "{plan}");
    // The string API resolves the function name too.
    sched.before("public_spawn", "public_inc").unwrap();
}

#[test]
fn a_renamed_registration_takes_the_function_form() {
    let mut world = World::new();
    world.spawn((Counter(0),));
    let mut sched = Scheduler::new();
    sched.add_systems(
        StageLabel::Update,
        apex_scheduler::sys("renamed_inc", systems_elsewhere::public_inc),
    );
    sched.compile_with_world(&world).unwrap();
    sched.run_sequential(&mut world);
    assert!(sched.debug_plan().contains("renamed_inc"));
    assert_eq!(Query::<&Counter>::new(&world).iter().next().map(|c| c.0), Some(10));
}

/// Outer attributes reach the generated item: this system names a type that does not exist, so
/// the test crate compiles only while `#[cfg(any())]` removes it — a dropped attribute is a build
/// error, not a silent loss of documentation.
mod attributes_reach_the_system {
    use apex_core::system;

    system! {
        #[cfg(any())]
        fn never_compiled(q: (&ThereIsNoSuchComponent,)) {
            let _ = q;
        }
    }

    system! {
        #[cfg(any())]
        fn never_compiled_either(world: &mut World) {
            let _: ThereIsNoSuchType = world;
        }
    }
}
