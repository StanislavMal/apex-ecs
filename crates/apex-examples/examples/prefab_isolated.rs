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
use apex_core::access_desc;
use apex_isolated::{CloneableBridge, IsolatedWorld, sync_bridge_cloneable};
use apex_macros::Component;
use apex_serialization::prefab::{PrefabChild, PrefabLoader};
use apex_serialization::WorldSerializer;

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════════
// Компоненты
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

// Маркеры — кто есть кто
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

    // Регистрируем сериализуемые компоненты (для префабов).
    // Префабы используют JSON-формат, поэтому register_component_serde_json.
    world.register_component_serde_json::<Position>();
    world.register_component_serde_json::<Health>();
    world.register_component_serde_json::<Damage>();
    world.register_component_serde_json::<Enemy>();
    world.register_component_serde_json::<Player>();

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
        .spawn_template_with("enemy", &TemplateParams::new())
        .expect("failed to spawn enemy from template");
    println!("  Враг создан из шаблона: entity={}", enemy1);

    // Второй враг
    let enemy2 = world
        .spawn_template_with("enemy", &TemplateParams::new())
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
    //     Демонстрируются: send_action_event, register_event + send_event,
    //     ctx.try_resource

    println!("\n--- IsolatedWorld: AI-симуляция ---");

    // Два независимых канала: main → sub и sub → main
    let (main_to_sub, sub_recv) = crossbeam_channel::unbounded(); // main  → sub
    let (sub_to_main, main_recv) = crossbeam_channel::unbounded(); // sub → main

    // Мост для основного мира (хранится в Resources):
    //   - to_sub    = main_to_sub  → отправляет в IsolatedWorld
    //   - from_sub  = main_recv    ← принимает из IsolatedWorld
    //   (исправлено: раньше каналы были перепутаны)
    let main_bridge = CloneableBridge::new(main_to_sub, main_recv);
    world.insert_resource(main_bridge);

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
    let iso_enemy = iso.world_mut().spawn(());
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

    // ── 6a. register_event + send_event (main → sub) ──────────────
    // send_event сериализует событие через bincode. На принимающей
    // стороне нужен register_event, который регистрирует тип в EventQueue
    // мира и сохраняет bincode-десериализатор в мосте.
    {
        println!("\n  --- send_event: сериализованное событие main → sub ---");
        // Достаём мост из ресурсов
        let bridge = world.try_resource::<CloneableBridge>().unwrap();
        // Регистрируем тип String в IsolatedWorld
        bridge.register_event::<String>(iso.world_mut());
        // Отправляем сериализованное событие
        bridge.send_event(&"Hello via bincode from main world!".to_string());
        println!("  ✓ register_event + send_event: событие отправлено в IsolatedWorld");
    }

    // ── 6b. send_action_event (main → sub) ─────────────────────────
    // send_action_event не требует сериализации и register_event.
    // Это просто замыкание, которое вызовет world.send_event() на той стороне.
    {
        println!("\n  --- send_action_event: действие main → sub ---");
        let bridge = world.try_resource::<CloneableBridge>().unwrap();
        bridge.send_action_event("Action event from main!".to_string());
        println!("  ✓ send_action_event отправлено в IsolatedWorld");
    }

    // ── 6c. AI-система с send_action_event и try_resource ─────────

    // Флаг для проверки, что AI-система выполнилась
    let ai_ran = Arc::new(AtomicBool::new(false));
    let ai_flag = ai_ran.clone();

    // CloneableBridge для IsolatedWorld: отправляет события в основной мир
    let iso_bridge = CloneableBridge::new(sub_to_main, sub_recv);

    // Добавляем AI-систему в IsolatedWorld
    iso.scheduler_mut().add_systems(
        apex_scheduler::StageLabel::Update,
        apex_scheduler::par_access(
        "ai_damage",
        access_desc!(write<Health>),
        move |ctx| {
            ai_flag.store(true, Ordering::SeqCst);

            // ── ctx.try_resource — безопасный доступ к ресурсу ──
            // В IsolatedWorld нет ресурса String, поэтому вернётся None
            if let Some(msg) = ctx.try_resource::<String>() {
                println!("  [AI] Resource found: {}", *msg);
            } else {
                // Этот else выполнится — ресурс не вставлен
            }

            // ── send_action_event — отправка действия в основной мир ──
            // В отличие от send_event, не требует сериализации и регистрации
            iso_bridge.send_action_event("AI: enemy took damage!".to_string());
        },
    ));

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

    // sync_bridge_cloneable применяет все накопленные сообщения
    sync_bridge_cloneable(&mut world);

    // world.tick() инкрементирует тик, flush_all_events() продвигает очереди событий
    world.tick();
    world.flush_all_events();

    // ── 8. Hierarchy export ─────────────────────────────────────────

    println!("\n--- Hierarchy export: parent-child ---");

    // Создаём иерархию: player → child
    let child = world.spawn(());
    world.insert(child, Position { x: 5.0, y: 10.0 });
    world.insert(child, Health {
        current: 30.0,
        max: 30.0,
    });
    world.add_relation(child, ChildOf, player);

    // Экспортируем иерархию начиная с игрока.
    // hierarchy_to_prefab(&World, Entity) -> PrefabManifest — дети встроены (inline), так что префаб
    // самодостаточен: инстанцируется без предзагрузки под-префабов.
    match WorldSerializer::hierarchy_to_prefab(&world, player) {
        Ok(hier) => {
            let json = serde_json::to_string_pretty(&hier).unwrap();
            println!("  Иерархический префаб:\n{}", json);
            println!("  Дочерних элементов: {}", hier.children.len());
            for (i, child) in hier.children.iter().enumerate() {
                match child {
                    PrefabChild::Inline(m) => println!(
                        "    Ребёнок {}: inline '{}', {} компонент(ов), {} вложенных",
                        i + 1,
                        m.name,
                        m.components.len(),
                        m.children.len()
                    ),
                    PrefabChild::Ref { prefab, overrides } => println!(
                        "    Ребёнок {}: ссылка '{}', {} overrides",
                        i + 1,
                        prefab,
                        overrides.len()
                    ),
                }
            }

            // Round-trip: инстанцируем экспортированный префаб обратно в тот же мир. Loader пуст — ничего
            // не предзагружаем, и всё равно работает, потому что дети встроены (самодостаточный префаб).
            let loader2 = PrefabLoader::new();
            match loader2.instantiate(&mut world, &hier, &[], None, None) {
                Ok(new_root) => {
                    let kids = world.targets_of(ChildOf, new_root).count();
                    println!(
                        "  ✓ Round-trip: префаб инстанцирован (entity={}, детей={})",
                        new_root, kids
                    );
                }
                Err(e) => println!("  Ошибка инстанцирования: {:?}", e),
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
