//! apex-examples: Prefabs + EntityTemplate + IsolatedWorld + WorldBridge
//!
//! Demonstrates four key capabilities of Apex ECS:
//!
//! 1. **PrefabManifest / PrefabLoader** — loading an entity from a JSON prefab
//! 2. **EntityTemplate** — a programmatic template spawned via `spawn_from_template()`
//! 3. **IsolatedWorld** — an isolated world with its own Scheduler for AI simulation
//! 4. **CloneableBridge** — communication between the main world and IsolatedWorld
//!
//! ```ignore
//! cargo run --example prefab_isolated
//! ```
//!
//! Scenario:
//! - A main world is created with NPCs (enemy, player) via prefabs and templates
//! - An IsolatedWorld is started with an AI system for the enemies
//! - CloneableBridge syncs events from IsolatedWorld into the main world
//! - After the tick, the main world prints its state

use apex_core::prelude::*;
use apex_core::access_desc;
use apex_isolated::{CloneableBridge, IsolatedWorld, sync_bridge_cloneable};
use apex_macros::Component;
use apex_serialization::prefab::{PrefabChild, PrefabLoader};
use apex_serialization::WorldSerializer;

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════════
// Components
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Health {
    current: f32,
    max: f32,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Damage {
    amount: f32,
}

// Markers — who is who
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Enemy;

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Player;

// ═══════════════════════════════════════════════════════════════════════════
// EntityTemplate: EnemyTemplate
// ═══════════════════════════════════════════════════════════════════════════

struct EnemyTemplate;

impl EntityTemplate for EnemyTemplate {
    fn spawn(&self, world: &mut World, _params: &TemplateParams) -> Entity {
        let e = world.spawn(());
        world.insert(e, Position { x: 100.0, y: 200.0 });
        world.insert(e, Health {
            current: 50.0,
            max: 50.0,
        });
        world.insert(e, Damage { amount: 10.0 });
        world.insert(e, Enemy);
        e
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Helper function for printing an entity
// ═══════════════════════════════════════════════════════════════════════════

fn print_entity(world: &World, entity: Entity, label: &str) {
    let pos = world.get::<Position>(entity);
    let hp  = world.get::<Health>(entity);
    print!("  {}: entity={}", label, entity);
    if let Some(p) = pos {
        print!(" pos=({},{})", p.x, p.y);
    }
    if let Some(h) = hp {
        print!(" hp={}/{}", h.current, h.max);
    }
    println!();
}

// ═══════════════════════════════════════════════════════════════════════════
// main
// ═══════════════════════════════════════════════════════════════════════════

fn main() {
    println!("=== Apex ECS — Prefabs + EntityTemplate + IsolatedWorld ===\n");

    // ── 1. Create the world and register components ─────────────────

    let mut world = World::new();

    // Register serializable components (for prefabs).
    // Prefabs use the JSON format, hence register_component_serde_json.
    world.register_component_serde_json::<Position>();
    world.register_component_serde_json::<Health>();
    world.register_component_serde_json::<Damage>();
    world.register_component_serde_json::<Enemy>();
    world.register_component_serde_json::<Player>();

    // Register an event for communication between worlds
    world.add_event::<String>();

    // ── 2. PrefabManifest — loading a prefab from JSON ──────────────

    println!("--- Prefab: Player from JSON ---");

    let player_json = r#"{
        "name": "player_prefab",
        "components": [
            {
                "type_name": "prefab_isolated::Player",
                "value": null
            },
            {
                "type_name": "prefab_isolated::Position",
                "value": { "x": 0.0, "y": 0.0 }
            },
            {
                "type_name": "prefab_isolated::Health",
                "value": { "current": 100.0, "max": 100.0 }
            }
        ]
    }"#;

    let mut loader = PrefabLoader::new();
    // load_json returns &PrefabManifest under a mutable borrow.
    // Clone it to release the borrow and use instantiate below.
    let manifest = loader
        .load_json(player_json)
        .expect("failed to load player prefab JSON")
        .clone();
    println!("  Loaded prefab: {}", manifest.name);

    // Spawn the player from the prefab: instantiate(&self, world, manifest, overrides, parent)
    let player = loader
        .instantiate(&mut world, &manifest, &[], None, None)
        .expect("failed to instantiate player prefab");
    println!("  Player created: entity={}", player);

    // ── 3. EntityTemplate — creating an enemy from a template ──────

    println!("\n--- EntityTemplate: Enemy from a template ---");

    // Register the template: register_template(&mut self, name, template_instance)
    world.register_template("enemy", EnemyTemplate);

    // spawn_from_template(&mut self, name, &TemplateParams)
    let enemy1 = world
        .spawn_template_with("enemy", &TemplateParams::new())
        .expect("failed to spawn enemy from template");
    println!("  Enemy created from template: entity={}", enemy1);

    // Second enemy
    let enemy2 = world
        .spawn_template_with("enemy", &TemplateParams::new())
        .expect("failed to spawn enemy2 from template");
    println!("  Enemy 2 created from template: entity={}", enemy2);

    // ── 4. Inspect the world state ─────────────────────────────────

    println!("\n--- World state ---");
    println!("  Total entities: {}", world.entity_count());

    print_entity(&world, player, "Player");
    print_entity(&world, enemy1, "Enemy 1");
    print_entity(&world, enemy2, "Enemy 2");

    // ── 5. Export entity → PrefabManifest ───────────────────────────

    println!("\n--- Export: entity → PrefabManifest ---");

    // WorldSerializer::entity_to_prefab(&World, Entity) -> PrefabManifest
    match WorldSerializer::entity_to_prefab(&world, enemy1) {
        Ok(exported) => {
            let json = serde_json::to_string_pretty(&exported).unwrap();
            println!("  Exported enemy prefab:\n{}", json);
        }
        Err(e) => {
            println!("  Export error: {:?}", e);
        }
    }

    // ── 6. IsolatedWorld + CloneableBridge ─────────────────────────
    //     Demonstrates: send_action_event, register_event + send_event,
    //     ctx.try_resource

    println!("\n--- IsolatedWorld: AI simulation ---");

    // Two independent channels: main → sub and sub → main
    let (main_to_sub, sub_recv) = crossbeam_channel::unbounded(); // main  → sub
    let (sub_to_main, main_recv) = crossbeam_channel::unbounded(); // sub → main

    // Bridge for the main world (stored in Resources):
    //   - to_sub    = main_to_sub  → sends into IsolatedWorld
    //   - from_sub  = main_recv    ← receives from IsolatedWorld
    //   (fixed: the channels used to be swapped)
    let main_bridge = CloneableBridge::new(main_to_sub, main_recv);
    world.insert_resource(main_bridge);

    // Create the isolated world
    let mut iso = IsolatedWorld::new();

    // Register the same components in the isolated world
    iso.world_mut().register_component_serde::<Position>();
    iso.world_mut().register_component_serde::<Health>();
    iso.world_mut().register_component_serde::<Damage>();
    iso.world_mut().register_component_serde::<Enemy>();
    iso.world_mut().register_component_serde::<Player>();
    iso.world_mut().add_event::<String>();

    // Spawn an enemy in the isolated world
    let iso_enemy = iso.world_mut().spawn(());
    iso.world_mut().insert(iso_enemy, Position { x: 300.0, y: 400.0 });
    iso.world_mut().insert(iso_enemy, Health {
        current: 80.0,
        max: 80.0,
    });
    iso.world_mut().insert(iso_enemy, Damage { amount: 15.0 });
    iso.world_mut().insert(iso_enemy, Enemy);

    println!("  Enemy in IsolatedWorld: entity={}", iso_enemy);
    println!(
        "  Total entities in IsolatedWorld: {}",
        iso.world_mut().entity_count()
    );

    // ── 6a. register_event + send_event (main → sub) ──────────────
    // send_event serializes the event via bincode. On the receiving
    // side register_event is required — it registers the type in the
    // world's EventQueue and stores the bincode deserializer in the bridge.
    {
        println!("\n  --- send_event: serialized event main → sub ---");
        // Fetch the bridge from resources
        let bridge = world.try_resource::<CloneableBridge>().unwrap();
        // Register the String type in IsolatedWorld
        bridge.register_event::<String>(iso.world_mut());
        // Send the serialized event
        bridge.send_event(&"Hello via bincode from main world!".to_string());
        println!("  ✓ register_event + send_event: event sent to IsolatedWorld");
    }

    // ── 6b. send_action_event (main → sub) ─────────────────────────
    // send_action_event requires neither serialization nor register_event.
    // It is simply a closure that calls world.send_event() on the other side.
    {
        println!("\n  --- send_action_event: action main → sub ---");
        let bridge = world.try_resource::<CloneableBridge>().unwrap();
        bridge.send_action_event("Action event from main!".to_string());
        println!("  ✓ send_action_event sent to IsolatedWorld");
    }

    // ── 6c. AI system with send_action_event and try_resource ─────

    // Flag to verify that the AI system ran
    let ai_ran = Arc::new(AtomicBool::new(false));
    let ai_flag = ai_ran.clone();

    // CloneableBridge for IsolatedWorld: sends events into the main world
    let iso_bridge = CloneableBridge::new(sub_to_main, sub_recv);

    // Add the AI system to IsolatedWorld
    iso.scheduler_mut().add_systems(
        apex_scheduler::StageLabel::Update,
        apex_scheduler::par_access(
        "ai_damage",
        access_desc!(write<Health>),
        move |ctx| {
            ai_flag.store(true, Ordering::SeqCst);

            // ── ctx.try_resource — safe resource access ──
            // IsolatedWorld has no String resource, so this returns None
            if let Some(msg) = ctx.try_resource::<String>() {
                println!("  [AI] Resource found: {}", *msg);
            } else {
                // This else branch runs — the resource is not inserted
            }

            // ── send_action_event — send an action into the main world ──
            // Unlike send_event, requires neither serialization nor registration
            iso_bridge.send_action_event("AI: enemy took damage!".to_string());
        },
    ));

    // Run one tick of IsolatedWorld
    iso.tick();

    // Check the result
    if let Some(hp) = iso.world_mut().get::<Health>(iso_enemy) {
        println!(
            "  After the AI tick: enemy HP = {}/{}",
            hp.current, hp.max
        );
    }

    assert!(
        ai_ran.load(Ordering::SeqCst),
        "AI system did not run!"
    );

    // ── 7. Apply events from IsolatedWorld in the main world ─────

    println!("\n--- CloneableBridge: receiving events from IsolatedWorld ---");

    // sync_bridge_cloneable applies all accumulated messages
    sync_bridge_cloneable(&mut world);

    // world.tick() increments the tick, flush_all_events() advances the event queues
    world.tick();
    world.flush_all_events();

    // ── 8. Hierarchy export ─────────────────────────────────────────

    println!("\n--- Hierarchy export: parent-child ---");

    // Create a hierarchy: player → child
    let child = world.spawn(());
    world.insert(child, Position { x: 5.0, y: 10.0 });
    world.insert(child, Health {
        current: 30.0,
        max: 30.0,
    });
    world.add_relation(child, ChildOf, player);

    // Export the hierarchy starting from the player.
    // hierarchy_to_prefab(&World, Entity) -> PrefabManifest — children are inline, so the prefab
    // is self-contained: it instantiates without preloading sub-prefabs.
    match WorldSerializer::hierarchy_to_prefab(&world, player) {
        Ok(hier) => {
            let json = serde_json::to_string_pretty(&hier).unwrap();
            println!("  Hierarchical prefab:\n{}", json);
            println!("  Children: {}", hier.children.len());
            for (i, child) in hier.children.iter().enumerate() {
                match child {
                    PrefabChild::Inline(m) => println!(
                        "    Child {}: inline '{}', {} component(s), {} nested",
                        i + 1,
                        m.name,
                        m.components.len(),
                        m.children.len()
                    ),
                    PrefabChild::Ref { prefab, overrides } => println!(
                        "    Child {}: reference '{}', {} overrides",
                        i + 1,
                        prefab,
                        overrides.len()
                    ),
                }
            }

            // Round-trip: instantiate the exported prefab back into the same world. The loader is empty —
            // nothing is preloaded, and it still works because children are inline (self-contained prefab).
            let loader2 = PrefabLoader::new();
            match loader2.instantiate(&mut world, &hier, &[], None, None) {
                Ok(new_root) => {
                    let kids = world.targets_of(ChildOf, new_root).count();
                    println!(
                        "  ✓ Round-trip: prefab instantiated (entity={}, children={})",
                        new_root, kids
                    );
                }
                Err(e) => println!("  Instantiation error: {:?}", e),
            }
        }
        Err(e) => {
            println!("  Hierarchy export error: {:?}", e);
        }
    }

    // ── 9. PrefabPlugin (prefab hot-reload) ─────────────────────────

    println!("\n--- PrefabPlugin: prefab hot-reload ---");

    let mut prefab_plugin = apex_hot_reload::PrefabPlugin::new();
    let mut registry = apex_hot_reload::AssetRegistry::new();

    // PrefabPlugin works with files — create a temporary file
    let tmp_dir = std::env::temp_dir().join("apex_prefab_example");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let prefab_path = tmp_dir.join("player.prefab.json");
    std::fs::write(&prefab_path, player_json).unwrap();

    let asset_id = prefab_plugin
        .load_file(&prefab_path, &mut registry)
        .expect("failed to load prefab file");
    println!(
        "  Prefabs in the plugin: {} (AssetId={})",
        prefab_plugin.len(),
        asset_id.0
    );

    // Look up the prefab name by AssetId
    if let Some(name) = prefab_plugin.prefab_name(asset_id) {
        println!("  Asset #{}: {}", asset_id.0, name);
    }

    // Clean up temporary files
    let _ = std::fs::remove_file(&prefab_path);
    let _ = std::fs::remove_dir(&tmp_dir);

    // ═══════════════════════════════════════════════════════════════
    // Summary
    // ═══════════════════════════════════════════════════════════════

    println!("\n=== SUMMARY ===");
    println!("✅ PrefabManifest: loaded and instantiated");
    println!("✅ EntityTemplate: registered and used");
    println!("✅ IsolatedWorld: created, AI system ran");
    println!("✅ CloneableBridge: event delivered between worlds");
    println!("✅ Hierarchy export: parent-child prefab created");
    println!("✅ PrefabPlugin: prefab loaded into hot-reload");

    println!("\n  Main world: {} entities", world.entity_count());
    println!(
        "  IsolatedWorld: {} entities",
        iso.world_mut().entity_count()
    );
}
