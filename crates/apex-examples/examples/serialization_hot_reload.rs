//! apex-examples: serialization + hot reload
//!
//! Демонстрирует:
//! - register_component_serde::<T>() — регистрация сериализуемого компонента
//! - WorldSerializer::snapshot()     — снэпшот мира в JSON
//! - WorldSerializer::restore()      — восстановление мира из снэпшота
//! - HotReloadPlugin::watch_config() — горячая перезагрузка JSON-конфигов
//! - Entity remapping после restore
//! cargo run --example serialization_hot_reload --release
use apex_core::prelude::*;
use apex_serialization::WorldSerializer;
use apex_hot_reload::HotReloadPlugin;

use serde::{Deserialize, Serialize};

// ── Компоненты ─────────────────────────────────────────────────
//
// Serializable = Component + Serialize + Deserialize
// Достаточно добавить derive — регистрация через register_component_serde::<T>()

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Position { x: f32, y: f32 }

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Velocity { x: f32, y: f32 }

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Health { current: f32, max: f32 }

// Non-serializable — runtime данные, не сохраняются
struct RenderHandle(u64);

// ── Конфиг (hot reload target) ─────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PhysicsConfig {
    gravity: f32,
    dt:      f32,
}

// ── main ──────────────────────────────────────────────────────

fn main() {
    println!("=== Apex ECS — Serialization + Hot Reload ===\n");

    // ── 1. Настройка мира ──────────────────────────────────────

    let mut world = World::new();

    // Регистрируем компоненты БЕЗ сериализации — runtime only
    world.register_component::<RenderHandle>();

    // Регистрируем компоненты С сериализацией — попадут в снэпшот
    world.register_component_serde::<Position>();
    world.register_component_serde::<Velocity>();
    world.register_component_serde::<Health>();

    world.insert_resource(PhysicsConfig { gravity: 9.8, dt: 0.016 });

    // ── 2. Спавн entity ────────────────────────────────────────

    let player = world.spawn_bundle((
        Position { x: 0.0, y: 10.0 },
        Velocity { x: 1.0, y:  0.0 },
        Health   { current: 100.0, max: 100.0 },
    ));
    // RenderHandle НЕ включаем в spawn — покажем insert отдельно
    world.insert(player, RenderHandle(42));

    let enemy = world.spawn_bundle((
        Position { x: 20.0, y: 0.0 },
        Velocity { x: -0.5, y: 0.0 },
        Health   { current: 30.0, max: 30.0 },
    ));

    // Relations
    world.add_relation(enemy, apex_core::relations::ChildOf, player);

    println!("Before snapshot: {} entities", world.entity_count());

    // ── 3. Serialization: snapshot ─────────────────────────────

    let snapshot = WorldSerializer::snapshot(&world)
        .expect("snapshot failed");

    let json = snapshot.to_json().expect("to_json failed");

    println!(
        "Snapshot: {} entities, {} relations, {} bytes JSON",
        snapshot.entities.len(),
        snapshot.relations.len(),
        json.len()
    );

    // Сохранить на диск:
    // std::fs::write("save.json", &json).unwrap();
    //
    // Загрузить с диска:
    // let json = std::fs::read("save.json").unwrap();
    // let snapshot = WorldSnapshot::from_json(&json).unwrap();

    // ── 4. Serialization: restore ──────────────────────────────

    let mut world2 = World::new();
    world2.register_component::<RenderHandle>();
    world2.register_component_serde::<Position>();
    world2.register_component_serde::<Velocity>();
    world2.register_component_serde::<Health>();
    // Relations kinds нужно зарегистрировать чтобы restore смог восстановить их
    let parent = world2.spawn_bundle((Position { x: 0.0, y: 0.0 },));
    let child = world2.spawn_bundle((Position { x: 0.0, y: 0.0 },));
    world2.add_relation(
        child,
        apex_core::relations::ChildOf,
        parent,
    );
    // NOTE: В реальном проекте все relation kinds регистрируются при старте
    // мира независимо от того есть ли они в снэпшоте.

    let entity_map = WorldSerializer::restore(&mut world2, &snapshot)
        .expect("restore failed");

    println!(
        "After restore:  {} entities, entity_map size = {}",
        world2.entity_count(),
        entity_map.len()
    );

    // Проверяем что данные совпадают
    let new_player = entity_map[&player.index()];
    let pos = world2.get::<Position>(new_player).unwrap();
    println!("Restored player Position: ({:.1}, {:.1})", pos.x, pos.y);
    assert!((pos.x - 0.0).abs() < 1e-6);
    assert!((pos.y - 10.0).abs() < 1e-6);

    // RenderHandle НЕ сохранялся — его нужно пересоздать
    // world2.insert(new_player, RenderHandle(create_render_handle()));

    println!("✓ Serialization roundtrip OK\n");

    // ── 5. Hot Reload ──────────────────────────────────────────
    //
    // Создаём временный конфиг-файл для демонстрации.
    // В реальном проекте файл лежит в assets/.

    let config_dir = std::env::temp_dir().join("apex_ecs_example");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("physics.json");

    // Записываем начальный конфиг
    std::fs::write(&config_path, r#"{"gravity": 9.8, "dt": 0.016}"#).unwrap();

    // Создаём HotReloadPlugin — следит за директорией
    let mut hot = HotReloadPlugin::with_default_debounce(&config_dir)
        .expect("watcher init failed");

    // Регистрируем конфиг — немедленная начальная загрузка
    let _config_id = hot.watch_config::<PhysicsConfig>(&config_path, &mut world2)
        .expect("watch_config failed");

    println!("PhysicsConfig loaded: gravity={}", world2.resource::<PhysicsConfig>().gravity);

    // Симулируем изменение файла пользователем
    std::fs::write(&config_path, r#"{"gravity": 1.62, "dt": 0.016}"#).unwrap();

    // Небольшая задержка чтобы notify успел поймать изменение
    std::thread::sleep(std::time::Duration::from_millis(200));

    // В game loop — apply_changes() вызывается каждый кадр
    let changed = hot.apply_changes(&mut world2);

    if changed.is_empty() {
        // В CI/тестовой среде watcher может не сработать — это нормально
        println!("(no file changes detected in this run — OK in CI)");
    } else {
        println!(
            "Hot reload: {} file(s) reloaded",
            changed.len()
        );
        println!(
            "New gravity: {}",
            world2.resource::<PhysicsConfig>().gravity
        );
    }

    println!("\n=== Done ===");
}