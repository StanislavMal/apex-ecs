//! Attribution ladder inside `World::spawn` — a CPU-only probe.
//!
//! # Why this exists
//!
//! `spawn_wide` (10k one-at-a-time spawns of a four-component bundle) reports ONE number for a
//! path that does at least six different things per call: allocate an id, touch the entity table
//! three times (`get_location` + `ensure_record` + `set_location`), look the bundle's resolution
//! up in the cache, push into the archetype's entity vector, copy each component's bytes into its
//! column, push TWO `TickCell`s per component, and raise TWO per-column aggregates. One number
//! over six works cannot say which of them to fix (CONVENTIONS §2, lesson 25) — so this walks the
//! same 10k spawns once per rung, each rung executing one more stage of the real body, and the
//! DIFFERENCES name the stages.
//!
//! # What it found, so the next reader starts from a number and not from a guess
//!
//! 2026-08-30, on the cell's own bundle (4 components, 100 B): `entities.allocate` was 30 ns of a
//! 66 ns spawn — 46 %, and MORE than bevy's entire spawn. The stated hypothesis of the day ("we
//! pay for three touches of the entity table against bevy's two") was REFUTED by the same ladder:
//! touches 1 and 3 cost within noise. The tick cells were real but SECOND (4.1 ns per component
//! by the width fit). After ALLOC-LEASE-0830 the allocate rung is 7 ns and the spawn is 46.4.
//! Rerun it before trusting any of that: the numbers are what this file exists to re-measure.
//!
//! # Shape (same as the engine's `APEX_EXTRACT_LADDER`)
//!
//! - One timestamp per PASS, never per spawn: a probe that stamps per call would measure itself
//!   on exactly the stand it exists for.
//! - Read the DIFFERENCES between rungs, not the absolute of a rung against the live path.
//! - Every pass gets its OWN fresh `World`, so no rung runs warm on another's data — unlike the
//!   engine's re-walk ladder, whose rungs are lower bounds for that reason.
//!
//! # This is a COPY of the production body — and it says so out loud
//!
//! The rungs below re-state the statements of [`World::spawn_at`] because a stage cannot be cut
//! short from outside. Two things keep the copy honest, and both must stay:
//!
//! 1. Rung [`REAL`] is not a copy: it calls `World::spawn`. `(REAL − COPY_COMPLETE)` is printed
//!    by the probe and is the divergence of copy from original — a ladder whose top step does not
//!    meet the real path is not describing the real path.
//! 2. `ladder_copy_is_observationally_the_real_spawn` (below) asserts that the complete copy
//!    leaves a world INDISTINGUISHABLE from the one the real `spawn` builds — same locations,
//!    same component bytes, same change/added ticks, same aggregates, same live count. Drift in
//!    `spawn_at` that the copy does not follow turns that test red.
//!
//! One known structural divergence, deliberate and bounded: production interleaves
//! grow/copy/ticks/raise/len PER COMPONENT (inside `write_into_batch`), while the copy needs the
//! capacity grown before the generic data write and the bookkeeping after it — three passes over
//! the columns where production makes one. It is three and not five: the rungs above the data
//! write are switched INSIDE the single bookkeeping pass, because a rung that took a walk of the
//! columns for itself would charge that walk to the stage, and at width 8 the walk IS the
//! measurement. What remains is the price the `(REAL − COPY_COMPLETE)` line measures rather than
//! assumes — read it before reading any rung, and distrust the deltas of a run whose drift is
//! large.

use super::{ArchetypeId, Bundle, EntityLocation, TickCell, World};

/// Names of the rungs, in ladder order. Index = the `rung` argument of [`spawn_rung`].
pub const RUNGS: [&str; 12] = [
    "bundle ctor only",
    "+entities.allocate (id)",
    "+get_location (table touch 1)",
    "+ensure_record (table touch 2)",
    "+bundle cache + col scratch",
    "+archetype entities.push",
    "+component data write",
    "+tick cells (2 per component)",
    "+tick aggregates (2 per component)",
    "+set_location (table touch 3)",
    "+added-hook check == COPY COMPLETE",
    "real World::spawn",
];

/// The last rung that still runs the copy — the one the real path is compared against.
pub const COPY_COMPLETE: u8 = 10;
/// The rung that calls production `World::spawn` itself.
pub const REAL: u8 = 11;

/// Spawn `count` entities into `world`, executing the stages of `World::spawn` up to and
/// including `rung` (see [`RUNGS`]).
///
/// The caller creates and drops the world OUTSIDE its timed region, and times exactly this call.
///
/// # Panics
///
/// - if `B` is an empty bundle (the ladder describes the normal path, not the `spawn(())` fast
///   path) or if `B` has a `Drop` component: rungs below the data write DROP the bundle where
///   production moves it into a column, and a `Drop` bundle would make those rungs pay a price
///   the real path does not — the rungs would no longer be comparable.
/// - if `rung >= RUNGS.len()`.
pub fn spawn_rung<B, F>(world: &mut World, count: usize, rung: u8, mut make: F)
where
    B: Bundle,
    F: FnMut(usize) -> B,
{
    assert!(
        (rung as usize) < RUNGS.len(),
        "spawn ladder: rung {rung} is above the top of the ladder ({})",
        RUNGS.len() - 1
    );
    assert!(
        !B::needs_drop(),
        "spawn ladder: bundle `{}` has a Drop component — the rungs below the data write drop \
         the bundle where the real path moves it into a column, so their costs would not be \
         comparable with the rungs above",
        std::any::type_name::<B>()
    );

    for i in 0..count {
        let bundle = make(i);

        if rung == REAL {
            world.spawn(bundle);
            continue;
        }

        // Rung 0 — nothing but the bundle the caller built. Every later rung carries it.
        if rung == 0 {
            std::hint::black_box(&bundle);
            drop(bundle);
            continue;
        }

        // Rung 1 — the id. Split off from the constructor on purpose: `allocate` takes the shared
        // lease (a read-lock plus an `Arc` clone) and two atomics even on this `&mut self` path,
        // and a rung that carried the constructor with it could not say so.
        let entity = world.entities.allocate();
        if rung == 1 {
            std::hint::black_box(&bundle);
            drop(bundle);
            continue;
        }

        // Rung 2 — the §0.2a guard: is this entity already located?
        let live = world.entities.get_location(entity).is_some();
        debug_assert!(!live, "spawn ladder: fresh id came back already located");
        std::hint::black_box(live);
        if rung == 2 {
            drop(bundle);
            continue;
        }

        // Rung 3 — the record must exist (a no-op for a direct allocate, paid all the same).
        world.entities.ensure_record(entity.index());
        if rung == 3 {
            drop(bundle);
            continue;
        }

        // Rung 4 — the bundle's resolution (one hash lookup per TYPE since `BundleCache`) plus
        // the copy of its column indices into the world's scratch.
        let slot = world.bundle_slot::<B>();
        assert!(
            !world.bundles.infos[slot].decl_ids.is_empty(),
            "spawn ladder: bundle `{}` is empty — the ladder describes the normal path, and the \
             empty-bundle fast path shares none of its stages",
            std::any::type_name::<B>()
        );
        let archetype_id = world.bundles.infos[slot].archetype;
        let mut cols = std::mem::take(&mut world.bundle_cols_scratch);
        cols.clear();
        cols.extend_from_slice(&world.bundles.infos[slot].cols);
        let arch_idx = archetype_id.0 as usize;
        let row = world.archetypes[arch_idx].entities.len();
        let tick = world.current_tick;
        if rung == 4 {
            world.bundle_cols_scratch = cols;
            drop(bundle);
            continue;
        }

        // Rung 5 — the entity joins the archetype's row list.
        world.archetypes[arch_idx].entities.push(entity);
        if rung == 5 {
            world.bundle_cols_scratch = cols;
            drop(bundle);
            continue;
        }

        // Rungs 6..8 — the row itself, in the fewest passes over the columns a cuttable ladder
        // allows: capacity must be grown BEFORE the generic data write, and the bookkeeping can
        // only follow it, so three passes are structural. The stages above the data write are
        // switched INSIDE the one bookkeeping pass rather than each taking a pass of its own —
        // otherwise every added rung would charge its own walk of the columns to itself, and at
        // width 8 that walk is the thing being measured.
        {
            let arch = &mut world.archetypes[arch_idx];
            for &c in cols.iter() {
                let col = &mut arch.columns[c];
                if col.len >= col.capacity {
                    col.grow();
                }
            }
        }
        // Rung 6 — the component bytes. `len` bookkeeping belongs with it and not above it:
        // without it the next row would compute its pointer past the allocation, so a rung that
        // "only writes data" cannot exist.
        bundle.write_data_into_batch(world, archetype_id, row, tick, &cols);
        {
            let arch = &mut world.archetypes[arch_idx];
            for &c in cols.iter() {
                let col = &mut arch.columns[c];
                // Rung 7 — the per-row tick cells: TWO pushes per component, each with its own
                // capacity check, against a reference that writes into a pre-sized row.
                if rung >= 7 {
                    col.change_ticks.push(TickCell::new(tick));
                    col.added_ticks.push(TickCell::new(tick));
                }
                // Rung 8 — the per-column aggregates that buy `changed_iter` its O(1) skip.
                if rung >= 8 {
                    col.max_change_tick.raise(tick);
                    col.max_added_tick.raise(tick);
                }
                col.len += 1;
            }
        }

        world.bundle_cols_scratch = cols;

        // Rung 9 — the third touch of the entity table.
        if rung >= 9 {
            world.entities.set_location(
                entity,
                EntityLocation {
                    archetype_id,
                    row: row as u32,
                },
            );
        }

        // Rung 10 — the added-hook check (one predictable branch unless hooks are registered).
        if rung >= 10 && world.registry.any_flags() {
            let ids = world.bundles.infos[slot].sorted_ids.clone();
            world.queue_added_hooks(entity, &ids);
            world.flush_hooks();
        }
    }
}

/// The `ArchetypeId` a bundle of type `B` resolves to in `world` — the probe uses it to read the
/// storage back and check what a rung actually left behind.
pub fn archetype_of<B: Bundle>(world: &mut World) -> ArchetypeId {
    let slot = world.bundle_slot::<B>();
    world.bundles.infos[slot].archetype
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;

    #[derive(Clone, Copy, PartialEq, Debug)]
    struct A(u32);
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct B(f64);
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct C([u32; 4]);

    impl Component for A {}
    impl Component for B {}
    impl Component for C {}

    type Wide = (A, B, C);

    fn make(i: usize) -> Wide {
        (A(i as u32), B(i as f64 * 0.5), C([i as u32; 4]))
    }

    /// The gate that keeps the copy honest: the COMPLETE copy must leave a world no observer can
    /// tell from the one the real `World::spawn` builds. If a stage is ever added to, removed
    /// from or reordered inside `spawn_at` and the ladder is not followed along, this goes red —
    /// which is the whole reason the ladder is allowed to restate production statements at all.
    #[test]
    fn ladder_copy_is_observationally_the_real_spawn() {
        const N: usize = 64;

        let mut by_copy = World::new();
        spawn_rung::<Wide, _>(&mut by_copy, N, COPY_COMPLETE, make);

        let mut by_real = World::new();
        spawn_rung::<Wide, _>(&mut by_real, N, REAL, make);

        assert_eq!(
            by_copy.entities.len(),
            by_real.entities.len(),
            "live count differs"
        );
        let a_copy = archetype_of::<Wide>(&mut by_copy);
        let a_real = archetype_of::<Wide>(&mut by_real);
        assert_eq!(a_copy.0, a_real.0, "the two paths chose different archetypes");

        let arch_copy = &by_copy.archetypes[a_copy.0 as usize];
        let arch_real = &by_real.archetypes[a_real.0 as usize];
        assert_eq!(arch_copy.entities, arch_real.entities, "row order differs");
        assert_eq!(
            arch_copy.columns.len(),
            arch_real.columns.len(),
            "column count differs"
        );

        for (ci, (cc, cr)) in arch_copy
            .columns
            .iter()
            .zip(arch_real.columns.iter())
            .enumerate()
        {
            assert_eq!(cc.component_id, cr.component_id, "column {ci}: id");
            assert_eq!(cc.len, cr.len, "column {ci}: len");
            assert_eq!(
                cc.change_ticks.len(),
                cr.change_ticks.len(),
                "column {ci}: change tick count"
            );
            assert_eq!(
                cc.added_ticks.len(),
                cr.added_ticks.len(),
                "column {ci}: added tick count"
            );
            assert_eq!(
                cc.max_change_tick.get(),
                cr.max_change_tick.get(),
                "column {ci}: max change tick"
            );
            assert_eq!(
                cc.max_added_tick.get(),
                cr.max_added_tick.get(),
                "column {ci}: max added tick"
            );
            for row in 0..cc.len {
                assert_eq!(
                    cc.change_ticks[row].get(),
                    cr.change_ticks[row].get(),
                    "column {ci} row {row}: change tick"
                );
                assert_eq!(
                    cc.added_ticks[row].get(),
                    cr.added_ticks[row].get(),
                    "column {ci} row {row}: added tick"
                );
                // SAFETY: `row < len`; both columns hold the same component type, and the bytes
                // written came from the same `make(i)`.
                unsafe {
                    let pc = std::slice::from_raw_parts(cc.get_ptr(row), cc.item_size);
                    let pr = std::slice::from_raw_parts(cr.get_ptr(row), cr.item_size);
                    assert_eq!(pc, pr, "column {ci} row {row}: component bytes");
                }
            }
        }

        // Locations: the same entity id must sit in the same archetype and row in both worlds.
        for (row, &e) in arch_real.entities.iter().enumerate() {
            let lc = by_copy
                .entities
                .get_location(e)
                .unwrap_or_else(|| panic!("copy lost the location of {e}"));
            let lr = by_real.entities.get_location(e).unwrap();
            assert_eq!(lc.archetype_id.0, lr.archetype_id.0, "row {row}: archetype");
            assert_eq!(lc.row, lr.row, "row {row}: row index");
        }

        // And the components read back through the public API.
        for (row, &e) in arch_real.entities.iter().enumerate() {
            assert_eq!(
                by_copy.get::<A>(e).copied(),
                by_real.get::<A>(e).copied(),
                "row {row}: A"
            );
            assert_eq!(
                by_copy.get::<C>(e).copied(),
                by_real.get::<C>(e).copied(),
                "row {row}: C"
            );
        }
    }

    /// Every rung must leave a world that is safe to keep using and to drop: a truncated spawn
    /// must not, for example, leave a column claiming rows whose bytes were never written.
    #[test]
    fn every_rung_leaves_a_droppable_world() {
        for rung in 0..RUNGS.len() as u8 {
            let mut w = World::new();
            spawn_rung::<Wide, _>(&mut w, 32, rung, make);
            let a = archetype_of::<Wide>(&mut w);
            let arch = &w.archetypes[a.0 as usize];
            for col in &arch.columns {
                assert!(
                    col.len <= col.capacity,
                    "rung {rung} ({}): column len {} above capacity {}",
                    RUNGS[rung as usize],
                    col.len,
                    col.capacity
                );
                assert!(
                    col.change_ticks.len() == col.len || col.change_ticks.is_empty(),
                    "rung {rung} ({}): {} tick cells against {} rows — a partial row",
                    RUNGS[rung as usize],
                    col.change_ticks.len(),
                    col.len
                );
            }
            drop(w);
        }
    }

    /// The ladder refuses an empty bundle out loud rather than reporting the fast path's numbers
    /// under the normal path's names (§0.2a).
    #[test]
    #[should_panic(expected = "is empty")]
    fn empty_bundle_is_refused() {
        let mut w = World::new();
        spawn_rung::<(), _>(&mut w, 4, COPY_COMPLETE, |_| ());
    }
}
