use apex_core::prelude::*;
use crate::{Position, Rotation, Transform, Velocity};
use cgmath::{Matrix4, Vector3};

// SpawnWide — 10k INDIVIDUAL spawns of a FOUR-component bundle.
//
// Why it exists (2026-08-30): every other spawn cell measures the BATCH path (`simple_insert`
// uses `spawn_many`, `commands_spawn` goes through `spawn_bundles_bulk`), and the batch path
// resolves the bundle once per batch. The one-at-a-time path — what a glTF import, an editor
// spawn and `world.spawn(..)` in user code actually run — had no cell at all, and that is
// exactly where the cost of resolving a bundle lives: it is paid per CALL, so it grows with the
// bundle's WIDTH.
//
// The gap this cell exists to hold: apex used to re-derive the composition on every spawn (one
// `TypeId` hash lookup per component in `static_component_ids`, a hash of the sorted id list for
// the archetype, then another lookup plus a `column_index` scan per component inside
// `write_into`). Three extra components cost apex 4.2x what they cost bevy. With the bundle
// resolved once per TYPE (`BundleCache`) the ratio came back to parity — and a cell is what
// keeps it there.
//
// Deliberately NOT batched and deliberately WIDE: a one-component spawn hides the defect (it
// pays two lookups instead of nine), which is why the narrow cells never caught it.
pub struct SpawnWide;

impl Default for SpawnWide {
    fn default() -> Self {
        Self::new()
    }
}

impl SpawnWide {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) {
        let mut world = World::new();
        for _ in 0..10_000 {
            world.spawn((
                Transform(Matrix4::from_scale(1.0)),
                Position(Vector3::unit_x()),
                Rotation(Vector3::unit_x()),
                Velocity(Vector3::unit_x()),
            ));
        }
    }
}
