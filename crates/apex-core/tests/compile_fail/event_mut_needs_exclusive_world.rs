//! S3 / ADR-002: reading or writing an event queue mutates its cursor, so doing
//! it through a shared `&World` is a safe-reachable data race (`World: Sync` lets
//! safe code share `&World` across threads). `World::event_reader`/`event_writer`
//! take `&mut self`, so both calls below must fail to compile from a `&World`.
#![allow(dead_code)]

use apex_core::World;

struct Ping;

fn read_from_shared(world: &World) {
    let _r = world.event_reader::<Ping>();
}

fn write_from_shared(world: &World) {
    let _w = world.event_writer::<Ping>();
}

fn main() {}
