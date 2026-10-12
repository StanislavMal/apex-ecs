//! A world and a schedule that must RUN on `wasm32-unknown-unknown`, not only compile (ADR-020).
//!
//! `cargo check --target wasm32-unknown-unknown` was green while the first `Scheduler::run` in a
//! browser trapped: the scheduler read `std::time::Instant`, which panics on that target. This
//! crate is the witness. CI builds it as wasm and runs [`smoke_run`] under Node (`run.mjs`); the
//! native test below pins the same hash, so the two builds must also agree bit for bit.
//!
//! The work is integer only - spawn, parallel-eligible systems, despawn and respawn (entity id
//! reuse), a `ChildOf` hierarchy through `propagate_transforms` - and the hash covers entity
//! indices and component values in iteration order.

use apex_core::prelude::*;
use apex_core::transform::{propagate_transforms, GlobalTransform, LocalTransform};
use apex_macros::Component;
use apex_scheduler::{sys, Scheduler, StageLabel};

#[derive(Component, Clone, Copy, Debug)]
struct Pos {
    x: i32,
    y: i32,
}

#[derive(Component, Clone, Copy, Debug)]
struct Vel {
    x: i32,
    y: i32,
}

#[derive(Component, Clone, Copy, Debug)]
struct Hp(i32);

#[derive(Clone, Copy, Debug)]
struct Hash(u64);

#[derive(Clone, Copy, Debug)]
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> i32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as i32
    }
}

fn mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x100000001b3)
}

fn unit(r: i32, hp: i32) -> (Pos, Vel, Hp) {
    (
        Pos { x: r & 1023, y: (r >> 10) & 1023 },
        Vel { x: (r & 7) - 3, y: ((r >> 3) & 7) - 3 },
        Hp(hp),
    )
}

system! {
    fn movement(q: (&Vel, &mut Pos)) {
        q.for_each_mut(|_, (v, mut p)| {
            p.x += v.x;
            p.y += v.y;
        });
    }
}

system! {
    fn attrition(q: (&Pos, &mut Hp)) {
        q.for_each_mut(|_, (p, mut hp)| hp.0 -= (p.x ^ p.y) & 7);
    }
}

system! {
    fn reaper(world: &mut World) {
        let mut dead: Vec<Entity> = Vec::new();
        world.query::<(&Hp,)>().for_each(|e, (hp,)| {
            if hp.0 <= 0 {
                dead.push(e);
            }
        });
        let mut lcg = *world.resource::<Lcg>();
        for e in dead {
            world.despawn(e);
            world.spawn(unit(lcg.next(), 100));
        }
        *world.resource_mut::<Lcg>() = lcg;
    }
}

system! {
    fn transforms(world: &mut World) {
        propagate_transforms(world);
    }
}

system! {
    fn hasher(world: &mut World) {
        let mut h = world.resource::<Hash>().0;
        world.query::<(&Pos, &Hp)>().for_each(|e, (p, hp)| {
            h = mix(h, e.index() as u64);
            h = mix(h, p.x as u32 as u64);
            h = mix(h, p.y as u32 as u64);
            h = mix(h, hp.0 as u32 as u64);
        });
        let mut globals = 0u64;
        world.query::<(&GlobalTransform,)>().for_each(|_, _| globals += 1);
        world.resource_mut::<Hash>().0 = mix(h, globals);
    }
}

/// Runs the smoke world: `units` entities for `ticks` scheduler runs; returns the state hash.
pub fn run(units: u32, ticks: u32) -> u64 {
    let mut world = World::new();
    world.insert_resource(Hash(0xcbf29ce484222325));
    world.insert_resource(Lcg(42));

    let mut lcg = Lcg(7);
    for _ in 0..units {
        let r = lcg.next();
        world.spawn(unit(r, 50 + (r & 63)));
    }
    // A hierarchy, so the transform pass descends (and reads its clock on every branch).
    let root = world.spawn((LocalTransform::from_xyz(1.0, 0.0, 0.0),));
    for i in 0..4 {
        let child = world.spawn((LocalTransform::from_xyz(0.0, i as f64, 0.0),));
        world.add_relation(child, ChildOf, root);
    }

    let mut sched = Scheduler::new();
    sched.add_systems(
        StageLabel::Update,
        (sys("movement", movement), sys("attrition", attrition)),
    );
    sched.add_systems(StageLabel::PostUpdate, (reaper, transforms, hasher));
    sched
        .chain(&["reaper", "transforms", "hasher"])
        .expect("the smoke schedule chains");
    sched
        .compile_with_world(&world)
        .expect("the smoke schedule compiles");
    for _ in 0..ticks {
        sched.run(&mut world);
    }
    world.resource::<Hash>().0
}

/// The entry point `run.mjs` calls.
#[no_mangle]
pub extern "C" fn smoke_run() -> u64 {
    run(4000, 200)
}

#[cfg(test)]
mod tests {
    /// The native build computes the hash the wasm build must reproduce (`run.mjs` reads the same
    /// file). A change of the smoke world changes both; a change of the core that moves the hash
    /// is a change of behaviour and is looked at, not re-pinned blindly.
    #[test]
    fn the_native_hash_is_the_pinned_one() {
        let expected = include_str!("../expected_hash.txt").trim();
        let got = format!("{:016x}", super::smoke_run());
        assert_eq!(got, expected, "native smoke hash moved");
    }
}
