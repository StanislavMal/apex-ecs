//! The binary and text formats under load: what a save, a load, an incremental diff and the event
//! stream between isolated worlds cost, on a world of a scene's shape.
//!
//! The stand behind a format change (engine TD-608): run the build before and the build after
//! alternately and compare medians. Every row is a median of `SAMPLES` runs; sizes are printed so
//! a faster road that writes more bytes cannot hide.
//!
//! World: `ENTITIES` entities, each with two JSON components (a struct and a string), one binary
//! component and a zero-sized marker, plus one registered resource of a thousand entries — the
//! mix an editor scene carries.
//!
//! Run: `cargo run --release -p apex-bench --bin serialization_load`

use apex_core::prelude::*;
use apex_isolated::WorldBridge;
use apex_serialization::{WorldSerializer, WorldSnapshot};
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::time::{Duration, Instant};

const SAMPLES: usize = 15;
const ENTITIES: usize = 20_000;
const EVENTS: usize = 200_000;
const BATCH: usize = 1_000;

#[derive(Component, Clone, Copy, Debug, Serialize, Deserialize)]
struct Placement {
    t: [f32; 3],
    r: [f32; 4],
    s: [f32; 3],
}

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
struct Label(String);

#[derive(Component, Clone, Copy, Debug, Serialize, Deserialize)]
struct Motion {
    velocity: [f32; 3],
    mass: f32,
    flags: u32,
}

#[derive(Component, Clone, Copy, Debug, Serialize, Deserialize)]
struct Marker;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Catalogue {
    names: Vec<String>,
    weights: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Hit {
    target: u64,
    amount: f32,
    kind: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Said(String);

fn registered_world() -> World {
    let mut w = World::new();
    w.register_component_serde_json::<Placement>();
    w.register_component_serde_json::<Label>();
    w.register_component_serde::<Motion>();
    w.register_component_serde_json::<Marker>();
    w.register_resource_serde::<Catalogue>();
    w
}

fn populated_world() -> (World, Vec<Entity>) {
    let mut w = registered_world();
    let mut ids = Vec::with_capacity(ENTITIES);
    for i in 0..ENTITIES {
        let f = i as f32;
        ids.push(w.spawn((
            Placement { t: [f * 0.5, 1.25, -f], r: [0.0, 0.3826834, 0.0, 0.9238795], s: [1.0, 1.0, 1.0] },
            Label(format!("entity_{i}")),
            Motion { velocity: [f.sin(), 0.0, f.cos()], mass: 1.0 + f * 1e-3, flags: i as u32 },
            Marker,
        )));
    }
    w.insert_resource(Catalogue {
        names: (0..1000).map(|i| format!("item_{i}")).collect(),
        weights: (0..1000).map(|i| i as f32 * 0.01).collect(),
    });
    (w, ids)
}

fn median_of<F: FnMut() -> usize>(mut f: F) -> (Duration, usize) {
    let mut times = Vec::with_capacity(SAMPLES);
    let mut sink = 0;
    for _ in 0..SAMPLES {
        let t = Instant::now();
        sink = black_box(f());
        times.push(t.elapsed());
    }
    times.sort();
    (times[SAMPLES / 2], sink)
}

fn row(name: &str, (t, bytes): (Duration, usize)) {
    println!("  {:<44} {:>10.3?}   {:>10} B", name, t, bytes);
}

fn main() {
    println!("=== serialization under load: {ENTITIES} entities, {EVENTS} events ===\n");
    let (world, _) = populated_world();
    let snap = WorldSerializer::snapshot(&world).expect("snapshot");

    let binary = snap.to_binary().expect("binary");
    let json = snap.to_json().expect("json");

    println!("[snapshot]");
    row("snapshot of the world", median_of(|| WorldSerializer::snapshot(&world).unwrap().entities.len()));
    row("write binary", median_of(|| snap.to_binary().unwrap().len()));
    row("read binary", median_of(|| WorldSnapshot::from_binary(&binary).unwrap().entities.len()));
    row("write text", median_of(|| snap.to_json().unwrap().len()));
    row("read text", median_of(|| WorldSnapshot::from_json(&json).unwrap().entities.len()));
    row(
        "read text + restore into a fresh world",
        median_of(|| {
            let s = WorldSnapshot::from_json(&json).unwrap();
            let mut w = registered_world();
            WorldSerializer::restore(&mut w, &s).unwrap();
            w.entity_count()
        }),
    );
    row(
        "read binary + restore into a fresh world",
        median_of(|| {
            let s = WorldSnapshot::from_binary(&binary).unwrap();
            let mut w = registered_world();
            WorldSerializer::restore(&mut w, &s).unwrap();
            w.entity_count()
        }),
    );

    // A tenth of the entities moved: the incremental save.
    let (mut moved, ids) = populated_world();
    for e in ids.into_iter().step_by(10) {
        if let Some(mut p) = moved.get_mut::<Placement>(e) {
            p.t[1] += 1.0;
        }
    }
    let diff = WorldSerializer::diff(&snap, &moved).expect("diff");
    let diff_bytes = diff.to_binary().expect("diff binary");
    println!("\n[diff: a tenth of the entities moved]");
    row("write diff binary", median_of(|| diff.to_binary().unwrap().len()));
    row(
        "read diff binary",
        median_of(|| apex_serialization::WorldDiff::from_binary(&diff_bytes).unwrap().modified_components.len()),
    );

    // The bridge is bounded (1024 by default) and blocks a sender whose peer does not drain, so the
    // stream is sent the way a frame sends it: a batch, then the receiving world drains it.
    println!("\n[events between isolated worlds: batches of {BATCH}]");
    row(
        "send + drain small events",
        median_of(|| {
            let (a, b) = WorldBridge::new();
            let mut w = World::new();
            b.register_event::<Hit>(&mut w);
            let mut received = 0;
            for batch in 0..EVENTS / BATCH {
                for i in 0..BATCH {
                    a.send_event(&Hit { target: (batch * BATCH + i) as u64, amount: 2.5, kind: (i % 7) as u8 });
                }
                b.apply_incoming(&mut w);
                received += w.events::<Hit>().len();
                w.events_mut::<Hit>().clear();
            }
            assert_eq!(received, EVENTS, "every event crossed");
            received
        }),
    );
    row(
        "send + drain string events (a tenth)",
        median_of(|| {
            let (a, b) = WorldBridge::new();
            let mut w = World::new();
            b.register_event::<Said>(&mut w);
            let mut received = 0;
            for batch in 0..EVENTS / 10 / BATCH {
                for i in 0..BATCH {
                    a.send_event(&Said(format!("message {batch}.{i} from the other world")));
                }
                b.apply_incoming(&mut w);
                received += w.events::<Said>().len();
                w.events_mut::<Said>().clear();
            }
            assert_eq!(received, EVENTS / 10, "every event crossed");
            received
        }),
    );
}
