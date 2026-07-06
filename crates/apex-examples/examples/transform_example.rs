//! Apex ECS — TransformPropagation Example
//!
//! Demonstrates hierarchical transforms (the new DX after C1/C2):
//! - [`LocalTransform`] — position/rotation/scale relative to the parent
//! - [`GlobalTransform`] — the final world matrix (created and recomputed
//!   automatically by the `propagate_transforms` system)
//! - dirty detection via `Changed<LocalTransform>` — **a manual `TransformDirty`
//!   is no longer needed**; just changing `LocalTransform` is enough.
//!
//! # Run
//!
//! ```bash
//! cargo run -p apex-examples --example transform_example
//! ```
//!
//! # Hierarchy in the example
//!
//! ```text
//! Grandparent (50, 0, 0)
//!   └── Parent (30, 0, 0)
//!         └── Child (20, 0, 0)
//! ```
//!
//! Expected result:
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

    // ── 1. Create the world and register components ─────────────
    let mut world = World::new();
    TransformPlugin::register_components(&mut world);

    // ── 2. Create a Scheduler with propagate_transforms in PostUpdate ─
    let mut sched = Scheduler::new();
    sched.add_systems(
        StageLabel::PostUpdate,
        apex_scheduler::seq("propagate_transforms", transform::propagate_transforms),
    );
    sched.compile().unwrap();

    println!("Scheduler plan:\n{}\n", sched.debug_plan());

    // ── 3. Create the Grandparent → Parent → Child hierarchy ────
    //     Spawn only LocalTransform — GlobalTransform is created by propagate.
    println!("--- Creating the hierarchy ---\n");

    let grandparent = create_transform_entity(&mut world, "Grandparent", Vec3::new(50.0, 0.0, 0.0), None);
    let parent = create_transform_entity(&mut world, "Parent", Vec3::new(30.0, 0.0, 0.0), Some(grandparent));
    let child = create_transform_entity(&mut world, "Child", Vec3::new(20.0, 0.0, 0.0), Some(parent));

    // ── 4. Run propagate_transforms ──────────────────────────────
    //     All three entities are "changed" (just spawned) → recomputed,
    //     GlobalTransform is auto-initialized.
    println!("\n--- Running propagate_transforms (PostUpdate) ---\n");
    world.tick();
    sched.run(&mut world);

    // ── 5. Check the results ─────────────────────────────────────
    println!("--- Results after propagation ---\n");
    print_entity(&world, grandparent, "Grandparent");
    print_entity(&world, parent, "Parent");
    print_entity(&world, child, "Child");

    let gp_pos = world.get::<GlobalTransform>(grandparent).unwrap().0.transform_point3(Vec3::ZERO);
    let p_pos = world.get::<GlobalTransform>(parent).unwrap().0.transform_point3(Vec3::ZERO);
    let c_pos = world.get::<GlobalTransform>(child).unwrap().0.transform_point3(Vec3::ZERO);

    assert_eq!(gp_pos, Vec3::new(50.0, 0.0, 0.0), "Grandparent should be at (50,0,0)");
    assert_eq!(p_pos, Vec3::new(80.0, 0.0, 0.0), "Parent should be at (80,0,0)");
    assert_eq!(c_pos, Vec3::new(100.0, 0.0, 0.0), "Child should be at (100,0,0)");
    println!("\n✅ All checks passed!\n");

    // ── 6. Demo: changing the parent's LocalTransform ───────────
    //     No manual markers: change LocalTransform → Changed<LocalTransform>
    //     is reliable (C1) → propagate recomputes Parent and cascades to Child.
    println!("--- Changing Parent's LocalTransform (no manual dirty) ---\n");

    // Frame model: the tick advances AT THE START of the frame (before mutations).
    // In a real application this is done by the scheduler (C7); here — manually.
    world.tick();
    if let Some(mut lt) = world.get_mut::<LocalTransform>(parent) {
        lt.translation = Vec3::new(100.0, 0.0, 0.0);
    }
    transform::propagate_transforms(&mut world);

    let p_pos2 = world.get::<GlobalTransform>(parent).unwrap().0.transform_point3(Vec3::ZERO);
    let c_pos2 = world.get::<GlobalTransform>(child).unwrap().0.transform_point3(Vec3::ZERO);

    // Parent — child of Grandparent(50): 50 + 100 = 150. Child: 150 + 20 = 170.
    println!("Parent after the change: ({:.1}, {:.1}, {:.1}) — expected (150, 0, 0)", p_pos2.x, p_pos2.y, p_pos2.z);
    println!("Child after the change:  ({:.1}, {:.1}, {:.1}) — expected (170, 0, 0)", c_pos2.x, c_pos2.y, c_pos2.z);

    assert_eq!(p_pos2, Vec3::new(150.0, 0.0, 0.0), "Parent must be at (150,0,0)");
    assert_eq!(c_pos2, Vec3::new(170.0, 0.0, 0.0), "Child must be at (170,0,0) via cascade");

    println!("\n✅ The parent's LocalTransform change cascaded down to the children!\n");

    // ── 7. Cross-stage change detection (TD-52) ─────────────────
    //     `GlobalTransform` is written in PostUpdate (`propagate_transforms`), yet a consumer often reads
    //     `Changed<GlobalTransform>` in an EARLIER stage (PreUpdate — picking/BVH, extract). A write in a
    //     late stage of frame N must be visible to an early stage of frame N+1. A per-frame change-tick
    //     used to lose this; the per-stage change-window (scheduler) fixes it. Self-contained check:
    println!("--- Cross-stage change detection (PostUpdate → PreUpdate next frame, TD-52) ---\n");
    {
        #[derive(Default)]
        struct Detected(u32); // how many GlobalTransform changes the PreUpdate observer saw

        let mut w = World::new();
        TransformPlugin::register_components(&mut w);
        w.insert_resource(Detected::default());
        let ent = w.spawn((LocalTransform::from_translation(Vec3::ZERO),));

        let mut s = Scheduler::new();
        // PreUpdate: count entities with Changed<GlobalTransform> since the last run of THIS stage.
        s.add_systems(
            StageLabel::PreUpdate,
            seq("observe_global", |w: &mut World| {
                let last = w.last_run_tick();
                let n = Query::<(Changed<GlobalTransform>,)>::new_with_tick(w, last).iter().count();
                w.resource_mut::<Detected>().0 += n as u32;
            }),
        );
        // PostUpdate: recompute world transforms (writes GlobalTransform).
        s.add_systems(StageLabel::PostUpdate, seq("propagate", transform::propagate_transforms));

        s.run(&mut w); // frame 0: PostUpdate creates GlobalTransform
        s.run(&mut w); // frame 1: PreUpdate sees the new GlobalTransform from frame 0 → baseline stabilizes
        let baseline = w.resource::<Detected>().0;

        // Move the object: LocalTransform changes NOW, GlobalTransform updates in PostUpdate of frame A.
        if let Some(mut lt) = w.get_mut::<LocalTransform>(ent) {
            lt.translation = Vec3::new(7.0, 0.0, 0.0);
        }
        s.run(&mut w); // frame A: PreUpdate (GT still old) → PostUpdate propagate writes the new GT
        let mid = w.resource::<Detected>().0;
        s.run(&mut w); // frame B: PreUpdate DETECTS the GT written in PostUpdate of frame A (cross-stage!)
        let end = w.resource::<Detected>().0;

        println!("  GlobalTransform changes seen by PreUpdate: baseline={baseline}, frame A={mid}, frame B={end}");
        assert_eq!(mid, baseline, "PreUpdate of the move frame runs BEFORE the GT write in PostUpdate of the same frame");
        assert_eq!(end, baseline + 1, "PreUpdate of the NEXT frame must see that GT (cross-stage, TD-52)");
        println!("\n✅ Cross-stage change detection: the PostUpdate write of frame A is visible to PreUpdate of frame B.\n");
    }

    println!("=== Done ===");
    println!("Final entities:   {}", world.entity_count());
    println!("Final tick:       {:?}", world.current_tick());
}

// ── Helper functions ────────────────────────────────────────────

/// Creates an entity with a single `LocalTransform` and attaches it to a parent (if given).
/// `GlobalTransform` is created automatically by the `propagate_transforms` system.
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
        println!("  [{}] created, parent={:?}, local=({:.1}, {:.1}, {:.1})", name, p, translation.x, translation.y, translation.z);
    } else {
        println!("  [{}] created (root), local=({:.1}, {:.1}, {:.1})", name, translation.x, translation.y, translation.z);
    }

    entity
}

/// Prints an entity's name and world position.
fn print_entity(world: &World, entity: Entity, label: &str) {
    let name = world.get::<DebugName>(entity).map(|n| n.0).unwrap_or("?");
    if let Some(gt) = world.get::<GlobalTransform>(entity) {
        let pos = gt.0.transform_point3(Vec3::ZERO);
        println!("  {:<24} pos=({:6.1}, {:6.1}, {:6.1})", format!("{} ({})", name, label), pos.x, pos.y, pos.z);
    }
}

// ── Debug component ──────────────────────────────────────────────

#[derive(Component, Clone, Copy, Debug)]
struct DebugName(&'static str);
