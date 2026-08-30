//! Attribution ladder inside the `Commands` insert path — a CPU-only probe.
//!
//! # Why this exists
//!
//! `commands_insert` (10k deferred `insert` + one `apply`) runs 0.89x the reference while the
//! DIRECT form of the very same 10k inserts (`add_remove_component`, `World::insert`) runs 1.58x
//! AHEAD of it. The contrast is the whole diagnosis: the archetype move is not the subject, the
//! machinery around it is. But "the machinery" was a subtraction between two cells, never a
//! measurement — this ladder is that measurement.
//!
//! The path has two halves and they are timed apart, because they are fixed by opposite means:
//!
//! - RECORD: `arena.alloc(payload)` + `queue.push(Command::Insert{..})`, 10k times.
//! - APPLY: `flush_reserved`, the walk of the queue with its per-command grouping probe, the
//!   payload read out of the arena, the component id resolve, and `World::insert` itself.
//!
//! # What it found, so the next reader starts from a number and not from a guess
//!
//! 2026-08-30, 10k inserts, everything inside one window (ns per insert): direct `World::insert`
//! 35.2 against bevy's 43.6 — we are AHEAD; the APPLY half 36.0 against 48.2 — ahead again; the
//! RECORD half 21.2 against 7.4 — **2.85x BEHIND, and it is the only place that is**. Inside the
//! record: `arena.alloc` 3.2, `queue.push` 16.1. The two variants below split that 16.1 in half
//! and name both halves: into a RESERVED queue 13.2 (so 6.7 ns is regrowing an allocation `apply`
//! throws away every time), into a 12-byte record instead of the 48-byte `Command` 5.7 (so ~7.5 ns
//! is the width of the record itself).
//!
//! Both halves were then closed (CMD-RECORD-0830): `size_of::<Command>()` 48 -> 32, and `apply`
//! hands the queue's allocation back instead of giving it to an iterator. The record half on the
//! SECOND frame — the only kind the scheduler runs — went 21.2 -> 7.5 against the reference's 7.4,
//! the apply half's queue walk 12.4 -> 2.8, and the deferred path as a whole is 1.20x ahead. The
//! third arm below ("second frame") exists because the cell cannot see any of that: it builds a
//! fresh `Commands` per pass, so it measures the COLD path and nothing else.
//!
//! # Shape
//!
//! Same discipline as [`crate::world::spawn_ladder`]: one timestamp per PASS, read the
//! DIFFERENCES, and the top rung of each half is the REAL production call so the copy is
//! measured against the original rather than trusted. `commands_ladder_copy_matches_the_real_path`
//! (below) is the gate that catches drift in `Commands::apply` the copy has not followed.
//!
//! # Scope, stated out loud
//!
//! The apply copy describes a HOMOGENEOUS queue of `Insert` commands on DISTINCT entities — the
//! shape the cell runs. It refuses anything else instead of quietly measuring a different path:
//! a burst on ONE entity takes the grouped road (`apply_insert_group`, one archetype move for the
//! whole burst), which is a different subject with different costs.

use super::{Command, Commands, InsertMeta, insert_target};
use crate::component::Component;
use crate::entity::Entity;
use crate::world::World;

/// Names of the RECORD rungs. Index = the `rung` argument of [`record_rung`].
pub const RECORD_RUNGS: [&str; 4] = [
    "payload ctor only",
    "+arena.alloc",
    "+queue.push == COPY COMPLETE",
    "real Commands::insert",
];

/// Names of the APPLY rungs. Index = the `rung` argument of [`apply_rung`].
pub const APPLY_RUNGS: [&str; 5] = [
    "flush + queue walk + group probe",
    "+payload read from arena",
    "+component id resolve",
    "+World::insert == COPY COMPLETE",
    "real Commands::apply",
];

/// The last RECORD rung that still runs the copy.
pub const RECORD_COPY_COMPLETE: u8 = 2;
/// The RECORD rung that calls production `Commands::insert`.
pub const RECORD_REAL: u8 = 3;
/// The last APPLY rung that still runs the copy.
pub const APPLY_COPY_COMPLETE: u8 = 3;
/// The APPLY rung that calls production `Commands::apply`.
pub const APPLY_REAL: u8 = 4;

/// Record one `insert` per entity of `entities`, executing the stages of `Commands::insert` up to
/// and including `rung` (see [`RECORD_RUNGS`]).
///
/// # Panics
///
/// If `T` has a `Drop` impl: the rungs below the queue push leave the arena payload orphaned
/// (`Commands::apply` never sees a command pointing at it), which would leak for a `Drop` type
/// and make the rung pay a price the real path does not.
pub fn record_rung<T, F>(cmds: &mut Commands, entities: &[Entity], rung: u8, mut make: F)
where
    T: Component + Send + 'static,
    F: FnMut(usize) -> T,
{
    assert!(
        (rung as usize) < RECORD_RUNGS.len(),
        "commands ladder: record rung {rung} is above the top of the ladder ({})",
        RECORD_RUNGS.len() - 1
    );
    assert!(
        !std::mem::needs_drop::<T>(),
        "commands ladder: component `{}` has a Drop impl — the rungs below the queue push leave \
         its payload orphaned in the arena, which would leak instead of measuring",
        std::any::type_name::<T>()
    );

    for (i, &entity) in entities.iter().enumerate() {
        let component = make(i);

        if rung == RECORD_REAL {
            cmds.insert(entity, component);
            continue;
        }
        if rung == 0 {
            std::hint::black_box(&component);
            drop(component);
            continue;
        }

        let offset = cmds.arena.alloc(component);
        if rung == 1 {
            std::hint::black_box(offset);
            continue;
        }

        cmds.queue.push(Command::Insert {
            entity,
            offset,
            vtable: &<T as InsertMeta>::VTABLE,
        });
    }
}

/// Apply the queue `cmds` holds, executing the stages of `Commands::apply` up to and including
/// `rung` (see [`APPLY_RUNGS`]). The queue is consumed either way, so the caller rebuilds it
/// before each timed pass.
///
/// # Panics
///
/// If the queue holds anything but `Insert`, or two consecutive inserts target the SAME entity —
/// both take roads this ladder does not describe (see the module header).
pub fn apply_rung<T: Component>(cmds: &mut Commands, world: &mut World, rung: u8) {
    assert!(
        (rung as usize) < APPLY_RUNGS.len(),
        "commands ladder: apply rung {rung} is above the top of the ladder ({})",
        APPLY_RUNGS.len() - 1
    );

    if rung == APPLY_REAL {
        cmds.apply(world);
        return;
    }

    world.flush_reserved();
    // The same swap production does (see `Commands::spare`): the buffer comes back at the end, so
    // the copy does not quietly measure a different walk from the road it describes. Before this
    // followed along, the copy read 5.9 ns per insert DEARER than the real `apply` — the
    // `copy drift` line is what said so.
    let mut queue = std::mem::take(&mut cmds.queue);
    let len = queue.len();
    // SAFETY: ownership of the `len` commands passes to this loop by raw pointer; the length is
    // zeroed first so a panic leaks rather than double-drops (exactly as production does).
    unsafe { queue.set_len(0) };
    let base = queue.as_ptr();
    // One scalar sink for every rung that stops short, folded once after the loop. A per-rung
    // `black_box` of a tuple was the first shape and it cost the bottom rung ~9 ns of stack
    // traffic that no rung above it paid — enough to print rung 0 as DEARER than rung 1, which
    // does strictly more. A sink that every rung pays identically cannot invert an order.
    let mut sink = 0u64;

    let mut i = 0usize;
    while i < len {
        // SAFETY: `i < len`, and nothing writes to the buffer while this loop walks it.
        let cmd = unsafe { std::ptr::read(base.add(i)) };
        let next_target = if i + 1 < len {
            insert_target(unsafe { &*base.add(i + 1) })
        } else {
            None
        };
        i += 1;
        // The grouping probe production pays on EVERY command, whether or not a burst follows.
        let target = insert_target(&cmd);
        assert!(target.is_some(), "commands ladder: non-insert command in the queue");
        assert!(
            next_target != target,
            "commands ladder: two consecutive inserts on one entity take the GROUPED road, which \
             this ladder does not describe"
        );

        let (entity, offset, vtable) = match cmd {
            Command::Insert {
                entity,
                offset,
                vtable,
            } => (entity, offset, vtable),
            _ => unreachable!("guarded by insert_target above"),
        };
        sink ^= (entity.index() as u64) << 32 | offset as u64;

        // Rungs 1 and 2 are PREVIEWS of two costs that live inside the typed apply, taken here
        // because a function pointer cannot be cut in half. They run INSTEAD of rung 3, never
        // before it: a preview that also ran on the way to the full stage would charge its read
        // twice and inflate the very rung it exists to explain.
        if rung < 3 {
            if rung == 0 {
                continue;
            }
            // SAFETY: `offset` points at a valid `T` written by `record_rung`/`Commands::insert`;
            // reading it takes ownership exactly once (the arena will not drop it), and the value
            // is forgotten rather than dropped, as the real apply's move into the column would.
            let component: T = unsafe { std::ptr::read(cmds.arena.get_ptr(offset) as *const T) };
            if rung == 1 {
                sink ^= &component as *const T as u64;
                std::mem::forget(component);
                continue;
            }
            // The `TypeId` hash `World::insert` pays on entry.
            sink ^= world.registry.get_or_register::<T>().0 as u64;
            std::mem::forget(component);
            continue;
        }

        // Rung 3 — production's own statement, through the command's OWN function pointer, not a
        // direct monomorphic call: the indirect call is part of what deferring costs, and a copy
        // that inlined it would hand that cost to the `copy drift` line as a mystery.
        // SAFETY: the pointer was written by `Commands::insert` for this exact `T` and offset.
        unsafe { (vtable.apply)(cmds.arena.get_ptr(offset), world, entity) };
    }
    std::hint::black_box(sink);
    cmds.arena.reset();
    // Hand the buffer back, as production does — a copy that dropped it would make the NEXT pass
    // pay for regrowth the road does not pay.
    queue.clear();
    cmds.queue = queue;
}

/// Give the command queue room for `additional` commands BEFORE a timed pass.
///
/// The record half rebuilds its queue from capacity zero on every `apply` — `Commands::apply`
/// consumes it with `into_iter`, so 10k pushes regrow it through ~19 reallocations every time.
/// Reserving outside the clock and re-running the same rung is what separates "the 48-byte write
/// costs this much" from "growing the queue costs this much". Neither is assumed; both are read.
pub fn reserve_queue(cmds: &mut Commands, additional: usize) {
    cmds.queue.reserve(additional);
}

/// The record half's payload written into a 12-byte per-command record instead of the 48-byte
/// `Command` — the shape the queue would have if the three function pointers every `Insert`
/// carries (`apply`, `drop`, `cid_fn`) were held once per TYPE instead of once per COMMAND.
///
/// This is NOT a road: it writes into a caller-owned vector and applies nothing. It exists so
/// that the size of the prize is measured BEFORE anyone builds the machinery to claim it — a
/// promised saving is not a number.
pub fn record_narrow<T, F>(
    cmds: &mut Commands,
    scratch: &mut Vec<(Entity, u32)>,
    entities: &[Entity],
    mut make: F,
) where
    T: Component + Send + 'static,
    F: FnMut(usize) -> T,
{
    assert!(
        !std::mem::needs_drop::<T>(),
        "commands ladder: component `{}` has a Drop impl — the narrow variant applies nothing, \
         so its payloads would leak instead of measuring",
        std::any::type_name::<T>()
    );
    for (i, &entity) in entities.iter().enumerate() {
        let offset = cmds.arena.alloc(make(i));
        scratch.push((entity, offset));
    }
}

/// How many commands the queue holds — the probe reports it so a variant that recorded nothing
/// is visible as such instead of reading as a fast one.
pub fn queue_len(cmds: &Commands) -> usize {
    cmds.queue.len()
}

/// The byte width of one queued command — printed next to the narrow variant, so the reader can
/// see what the variant traded away.
pub fn command_bytes() -> usize {
    std::mem::size_of::<Command>()
}

/// Hand a `Commands` back its allocations without applying anything — the probe's way of asking
/// "what does the SECOND frame cost", which is the frame the scheduler actually runs.
///
/// The record half's cheapest form is a queue that already has room, and `Commands::apply` now
/// leaves it that way; a probe that builds a fresh `Commands` per pass can never see it, and the
/// cell (`commands_insert`) does exactly that. So the probe warms one and reuses it.
pub fn warmed_commands<T, F>(world: &mut World, entities: &[Entity], mut make: F) -> Commands
where
    T: Component + Send + 'static,
    F: FnMut(usize) -> T,
{
    let mut cmds = Commands::new();
    for (i, &entity) in entities.iter().enumerate() {
        cmds.insert(entity, make(i));
    }
    cmds.apply(world);
    cmds
}

/// The control arm: the SAME inserts, straight into the world, with no Commands in the way. The
/// difference between this and [`APPLY_REAL`] + [`RECORD_REAL`] is what deferring costs — the
/// number the whole probe exists to name.
pub fn direct_insert<T, F>(world: &mut World, entities: &[Entity], mut make: F)
where
    T: Component,
    F: FnMut(usize) -> T,
{
    for (i, &entity) in entities.iter().enumerate() {
        world.insert(entity, make(i));
    }
}

/// A `Commands` with a fresh queue and arena — the probe builds one per timed pass.
pub fn fresh_commands() -> Commands {
    Commands::new()
}

/// How much room the arena holds — the probe reports it so a growth cliff inside a timed pass is
/// visible rather than blamed on a rung.
pub fn arena_bytes(cmds: &Commands) -> usize {
    cmds.arena.capacity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Debug)]
    struct A(f32);
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct B(f32);

    impl Component for A {}
    impl Component for B {}

    fn world_with(n: usize) -> (World, Vec<Entity>) {
        let mut world = World::new();
        let entities = world.spawn_many(n, |_| (A(0.0),));
        (world, entities)
    }

    /// The gate that keeps the copy honest: recording and applying through the ladder's COMPLETE
    /// copy must leave the world exactly where the real `Commands::insert` + `Commands::apply`
    /// leaves it. Drift in either production body that the ladder has not followed turns this red.
    #[test]
    fn commands_ladder_copy_matches_the_real_path() {
        const N: usize = 128;

        let (mut w_copy, e_copy) = world_with(N);
        let mut c_copy = fresh_commands();
        record_rung::<B, _>(&mut c_copy, &e_copy, RECORD_COPY_COMPLETE, |i| B(i as f32));
        apply_rung::<B>(&mut c_copy, &mut w_copy, APPLY_COPY_COMPLETE);

        let (mut w_real, e_real) = world_with(N);
        let mut c_real = fresh_commands();
        record_rung::<B, _>(&mut c_real, &e_real, RECORD_REAL, |i| B(i as f32));
        apply_rung::<B>(&mut c_real, &mut w_real, APPLY_REAL);

        assert_eq!(w_copy.entities.len(), w_real.entities.len(), "live count");
        for (i, (&ec, &er)) in e_copy.iter().zip(e_real.iter()).enumerate() {
            let lc = w_copy.entities.get_location(ec).expect("copy lost a location");
            let lr = w_real.entities.get_location(er).expect("real lost a location");
            assert_eq!(lc.archetype_id.0, lr.archetype_id.0, "entity {i}: archetype");
            assert_eq!(lc.row, lr.row, "entity {i}: row");
            assert_eq!(
                w_copy.get::<B>(ec).copied(),
                w_real.get::<B>(er).copied(),
                "entity {i}: inserted value"
            );
            assert_eq!(
                w_copy.get::<A>(ec).copied(),
                w_real.get::<A>(er).copied(),
                "entity {i}: carried value"
            );
        }
    }

    /// Every rung must leave the world usable: a truncated apply may write nothing, but it may
    /// not write half a row.
    #[test]
    fn every_rung_leaves_a_consistent_world() {
        for rec in 0..RECORD_RUNGS.len() as u8 {
            for app in 0..APPLY_RUNGS.len() as u8 {
                let (mut w, entities) = world_with(16);
                let mut c = fresh_commands();
                record_rung::<B, _>(&mut c, &entities, rec, |i| B(i as f32));
                apply_rung::<B>(&mut c, &mut w, app);
                // The carried component survives every rung; the inserted one appears only when
                // both halves ran to the top.
                for &e in &entities {
                    assert!(w.get::<A>(e).is_some(), "rec {rec} / app {app}: A lost");
                }
                let inserted = entities.iter().filter(|&&e| w.get::<B>(e).is_some()).count();
                let expect_all = rec >= RECORD_COPY_COMPLETE && app >= APPLY_COPY_COMPLETE;
                assert_eq!(
                    inserted,
                    if expect_all { entities.len() } else { 0 },
                    "rec {rec} ({}) / app {app} ({}): inserted count",
                    RECORD_RUNGS[rec as usize],
                    APPLY_RUNGS[app as usize]
                );
                drop(w);
            }
        }
    }

    /// A burst on ONE entity takes the grouped road; the ladder refuses it out loud rather than
    /// reporting the grouped path's numbers under the single path's names (§0.2a).
    #[test]
    #[should_panic(expected = "GROUPED road")]
    fn a_burst_on_one_entity_is_refused() {
        let (mut w, entities) = world_with(4);
        let mut c = fresh_commands();
        let doubled: Vec<Entity> = vec![entities[0], entities[0]];
        record_rung::<B, _>(&mut c, &doubled, RECORD_COPY_COMPLETE, |i| B(i as f32));
        apply_rung::<B>(&mut c, &mut w, APPLY_COPY_COMPLETE);
    }

    /// The control arm must land where the deferred one does — otherwise the difference between
    /// them measures a difference in OUTCOME, not the price of deferring.
    #[test]
    fn the_direct_arm_lands_where_the_deferred_one_does() {
        const N: usize = 64;
        let (mut w_direct, e_direct) = world_with(N);
        direct_insert::<B, _>(&mut w_direct, &e_direct, |i| B(i as f32));

        let (mut w_def, e_def) = world_with(N);
        let mut c = fresh_commands();
        record_rung::<B, _>(&mut c, &e_def, RECORD_REAL, |i| B(i as f32));
        apply_rung::<B>(&mut c, &mut w_def, APPLY_REAL);

        for (i, (&ed, &ef)) in e_direct.iter().zip(e_def.iter()).enumerate() {
            let ld = w_direct.entities.get_location(ed).unwrap();
            let lf = w_def.entities.get_location(ef).unwrap();
            assert_eq!(ld.archetype_id.0, lf.archetype_id.0, "entity {i}: archetype");
            assert_eq!(ld.row, lf.row, "entity {i}: row");
            assert_eq!(
                w_direct.get::<B>(ed).copied(),
                w_def.get::<B>(ef).copied(),
                "entity {i}: value"
            );
        }
    }
}
