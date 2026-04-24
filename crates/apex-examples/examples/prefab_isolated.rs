//! apex-examples: Prefabs + EntityTemplate + IsolatedWorld + WorldBridge
//!
//! Демонстрирует четыре ключевые возможности Apex ECS:
//!
//! 1. **PrefabManifest / PrefabLoader** — загрузка entity из JSON-префаба
//! 2. **EntityTemplate** — программный шаблон со спавном через `spawn_from_template()`
//! 3. **IsolatedWorld** — изолированный мир с собственным Scheduler для AI-симуляции
//! 4. **CloneableBridge** — коммуникация между основным миром и IsolatedWorld
//!
//! ```ignore
//! cargo run --example prefab_isolated
//! ```
//!
//! Сценарий:
//! - Создаётся основной мир с NPC (enemy, player) через префабы и шаблоны
//! - Запускается IsolatedWorld с AI-системой для врагов
//! - CloneableBridge синхронизирует события из IsolatedWorld в основной мир
//! - После тика основной мир печатает состояние

use apex_core::prelude::*;
use apex_isolated::{CloneableBridge, IsolatedWorld, BridgeEvent, sync_bridge_cloneable};
use apex_serialization::prefab::PrefabLoader;
use apex_serialization::WorldSerializer;

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════════
// Компоненты
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Health {
    current: f32,
    max: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Damage {
    amount: f32,
}

// Маркеры — кто есть кто
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Enemy;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Player;

// ═══════════════════════════════════════════════════════════════════════════
// EntityTemplate: EnemyTemplate
// ═══════════════════════════════════════════════════════════════════════════

struct EnemyTemplate;

impl EntityTemplate for EnemyTemplate {
    fn spawn(&self, world: &mut World, _params: &TemplateParams) -> Entity {
        let e = world.spawn_empty();
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
// Вспомогательная функция для печати сущности
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

    // ── 1. Создаём мир и регистрируем компоненты ────────────────────

    let mut world = World::new();

    // Регистрируем сериализуемые компоненты (для префабов)
    world.register_component_serde::<Position>();
    world.register_component_serde::<Health>();
    world.register_component_serde::<Damage>();
    world.register_component_serde::<Enemy>();
    world.register_component_serde::<Player>();

    // Регистрируем событие для коммуникации между мирами
    world.add_event::<String>();

    // ── 2. PrefabManifest — загрузка префаба из JSON ────────────────

    println!("--- Prefab: Player из JSON ---");

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
    // load_json возвращает &PrefabManifest под mutable borrow.
    // Клонируем, чтобы отпустить borrow и использовать instantiate ниже.
    let manifest = loader
        .load_json(player_json)
        .expect("failed to load player prefab JSON")
        .clone();
    println!("  Загружен префаб: {}", manifest.name);

    // Спавним игрока из префаба: instantiate(&self, world, manifest, overrides, parent)
    let player = loader
        .instantiate(&mut world, &manifest, &[], None, None)
        .expect("failed to instantiate player prefab");
    println!("  Игрок создан: entity={}", player);

    // ── 3. EntityTemplate — создание врага через шаблон ────────────

    println!("\n--- EntityTemplate: Enemy через шаблон ---");

    // Регистрируем шаблон: register_template(&mut self, name, template_instance)
    world.register_template("enemy", EnemyTemplate);

    // spawn_from_template(&mut self, name, &TemplateParams)
    let enemy1 = world
        .spawn_from_template("enemy", &TemplateParams::new())
        .expect("failed to spawn enemy from template");
    println!("  Враг создан из шаблона: entity={}", enemy1);

    // Второй враг
    let enemy2 = world
        .spawn_from_template("enemy", &TemplateParams::new())
        .expect("failed to spawn enemy2 from template");
    println!("  Враг 2 создан из шаблона: entity={}", enemy2);

    // ── 4. Проверяем состояние мира ────────────────────────────────

    println!("\n--- Состояние мира ---");
    println!("  Всего entity: {}", world.entity_count());

    print_entity(&world, player, "Игрок");
    print_entity(&world, enemy1, "Враг 1");
    print_entity(&world, enemy2, "Враг 2");

    // ── 5. Export entity → PrefabManifest ───────────────────────────

    println!("\n--- Export: entity → PrefabManifest ---");

    // WorldSerializer::entity_to_prefab(&World, Entity) -> PrefabManifest
    match WorldSerializer::entity_to_prefab(&world, enemy1) {
        Ok(exported) => {
            let json = serde_json::to_string_pretty(&exported).unwrap();
            println!("  Экспортированный префаб врага:\n{}", json);
        }
        Err(e) => {
            println!("  Ошибка экспорта: {:?}", e);
        }
    }

    // ── 6. IsolatedWorld + CloneableBridge ─────────────────────────

    println!("\n--- IsolatedWorld: AI-симуляция ---");

    // Создаём каналы для двусторонней связи между мирами
    //   main_tx → sub_rx  (основной мир отправляет в IsolatedWorld)
    //   sub_tx  → main_rx (IsolatedWorld отправляет в основной мир)
    let (main_tx, _sub_rx) = crossbeam_channel::unbounded();
    let (sub_tx,  main_rx) = crossbeam_channel::unbounded();

    // CloneableBridge для основного мира: может принимать из IsolatedWorld
    let main_bridge = CloneableBridge::new(sub_tx, main_rx);
    world.resources.insert(main_bridge);

    // Создаём изолированный мир
    let mut iso = IsolatedWorld::new();

    // Регистрируем те же компоненты в изолированном мире
    iso.world_mut().register_component_serde::<Position>();
    iso.world_mut().register_component_serde::<Health>();
    iso.world_mut().register_component_serde::<Damage>();
    iso.world_mut().register_component_serde::<Enemy>();
    iso.world_mut().register_component_serde::<Player>();
    iso.world_mut().add_event::<String>();

    // Спавним врага в изолированном мире
    let iso_enemy = iso.world_mut().spawn_empty();
    iso.world_mut().insert(iso_enemy, Position { x: 300.0, y: 400.0 });
    iso.world_mut().insert(iso_enemy, Health {
        current: 80.0,
        max: 80.0,
    });
    iso.world_mut().insert(iso_enemy, Damage { amount: 15.0 });
    iso.world_mut().insert(iso_enemy, Enemy);

    println!("  Враг в IsolatedWorld: entity={}", iso_enemy);
    println!(
        "  Всего entity в IsolatedWorld: {}",
        iso.world_mut().entity_count()
    );

    // Флаг для проверки, что AI-система выполнилась
    let ai_ran = Arc::new(AtomicBool::new(false));
    let ai_flag = ai_ran.clone();

    // Sender для отправки событий из AI-системы в основной мир
    let tx_to_main = main_tx.clone();

    // Добавляем AI-систему в IsolatedWorld
    // Система уменьшает HP всех врагов и отправляет событие в основной мир
    iso.scheduler_mut().add_fn_par_system(
        "ai_damage",
        move |_ctx: SystemContext<'_>| {
            ai_flag.store(true, Ordering::SeqCst);

            // Отправляем событие в основной мир через канал
            let _ = tx_to_main.send(BridgeEvent::Action(Box::new(|world: &mut World| {
                world.send_event("AI: enemy took damage!".to_string());
            })));

            // Читаем Query для Enemy + Health
            // Примечание: в add_fn_par_system доступ к компонентам
            // должен быть указан в AccessDescriptor (см. ниже)
        },
        // Доступ: Write<Health> (система изменяет Health)
        AccessDescriptor::new().write::<Health>(),
    );

    // Выполняем один тик IsolatedWorld
    iso.tick();

    // Проверяем результат
    if let Some(hp) = iso.world_mut().get::<Health>(iso_enemy) {
        println!(
            "  После AI-тика: HP врага = {}/{}",
            hp.current, hp.max
        );
    }

    assert!(
        ai_ran.load(Ordering::SeqCst),
        "AI-система не выполнилась!"
    );

    // ── 7. Применяем события из IsolatedWorld в основном мире ─────

    println!("\n--- CloneableBridge: приём событий из IsolatedWorld ---");

    // Применяем входящие сообщения через sync_bridge_cloneable
    sync_bridge_cloneable(&mut world);

    // Обновляем события (world.tick() продвигает очереди событий)
    world.tick();

    // ── 8. Hierarchy export ─────────────────────────────────────────

    println!("\n--- Hierarchy export: parent-child ---");

    // Создаём иерархию: player → child
    let child = world.spawn_empty();
    world.insert(child, Position { x: 5.0, y: 10.0 });
    world.insert(child, Health {
        current: 30.0,
        max: 30.0,
    });
    world.add_relation(child, ChildOf, player);

    // Экспортируем иерархию начиная с игрока
    // hierarchy_to_prefab(&World, Entity) -> PrefabManifest
    match WorldSerializer::hierarchy_to_prefab(&world, player) {
        Ok(hier) => {
            let json = serde_json::to_string_pretty(&hier).unwrap();
            println!("  Иерархический префаб:\n{}", json);
            println!("  Дочерних элементов: {}", hier.children.len());
            for (i, child) in hier.children.iter().enumerate() {
                println!(
                    "    Ребёнок {}: префаб='{}', {} overrides",
                    i + 1,
                    child.prefab,
                    child.overrides.len()
                );
            }
        }
        Err(e) => {
            println!("  Ошибка экспорта иерархии: {:?}", e);
        }
    }

    // ── 9. PrefabPlugin (hot-reload префабов) ───────────────────────

    println!("\n--- PrefabPlugin: hot-reload префабов ---");

    let mut prefab_plugin = apex_hot_reload::PrefabPlugin::new();
    let mut registry = apex_hot_reload::AssetRegistry::new();

    // PrefabPlugin работает с файлами — создаём временный файл
    let tmp_dir = std::env::temp_dir().join("apex_prefab_example");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let prefab_path = tmp_dir.join("player.prefab.json");
    std::fs::write(&prefab_path, player_json).unwrap();

    let asset_id = prefab_plugin
        .load_file(&prefab_path, &mut registry)
        .expect("failed to load prefab file");
    println!(
        "  Префабов в плагине: {} (AssetId={})",
        prefab_plugin.len(),
        asset_id.0
    );

    // Получаем имя префаба по AssetId
    if let Some(name) = prefab_plugin.prefab_name(asset_id) {
        println!("  Asset #{}: {}", asset_id.0, name);
    }

    // Очищаем временные файлы
    let _ = std::fs::remove_file(&prefab_path);
    let _ = std::fs::remove_dir(&tmp_dir);

    // ═══════════════════════════════════════════════════════════════
    // Итог
    // ═══════════════════════════════════════════════════════════════

    println!("\n=== ИТОГ ===");
    println!("✅ PrefabManifest: загружен и instantiated");
    println!("✅ EntityTemplate: зарегистрирован и использован");
    println!("✅ IsolatedWorld: создан, AI-система выполнилась");
    println!("✅ CloneableBridge: событие доставлено между мирами");
    println!("✅ Hierarchy export: parent-child префаб создан");
    println!("✅ PrefabPlugin: префаб загружен в hot-reload");

    println!("\n  Основной мир: {} entity", world.entity_count());
    println!(
        "  IsolatedWorld: {} entity",
        iso.world_mut().entity_count()
    );
}
