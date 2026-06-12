//! Apex ECS — Bundle composition demo
//!
//! Демонстрирует вложенные Bundle, кортежи Bundle и прямой spawn компонентов:
//!
//! 1. **Вложенные #[derive(Bundle)]** — поле одного Bundle внутри другого
//! 2. **Кортежи Bundle** — `(BundleA, ComponentB, ComponentC)` в spawn()
//! 3. **Прямой spawn компонента** — `spawn(MyComponent)` без кортежа
//! 4. **Смешанные кортежи** — Bundle-структуры + отдельные компоненты
//!
//! ```bash
//! cargo run -p apex-examples --example bundle_composition
//! ```

use apex_core::prelude::*;

// ── Компоненты ─────────────────────────────────────────────────
//
// Каждый компонент — `#[derive(Component)]`, что:
// - реализует трейт `Component` (раньше был blanket impl)
// - автоматически регистрирует компонент в World (через linkme)
// - делает тип доступным как Bundle из одного элемента

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Pos { x: f32, y: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Hp { current: f32, max: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Armor(f32);

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Weapon { name: &'static str, damage: f32 }

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Team(u8);

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Speed(f32);

// ── Bundle: базовый набор игрока (переиспользуемый) ──────────

#[derive(Bundle)]
struct PlayerBase {
    pos:  Pos,
    hp:   Hp,
    team: Team,
}

// ── Bundle: расширенный набор (вложенный Bundle + компоненты) ─
//
// Обратите внимание: поле `base: PlayerBase` — это ВЛОЖЕННЫЙ Bundle.
// Макрос `#[derive(Bundle)]` рекурсивно развернёт его в набор
// компонентов [Pos, Hp, Team] плюс дополнительные поля.

#[derive(Bundle)]
struct WarriorBundle {
    base:   PlayerBase,   // <— вложенный Bundle
    weapon: Weapon,
    armor:  Armor,
}

#[derive(Bundle)]
struct ScoutBundle {
    base:  PlayerBase,    // <— вложенный Bundle
    speed: Speed,
}

// ═══════════════════════════════════════════════════════════════
// Демо-функции
// ═══════════════════════════════════════════════════════════════

fn print_entity(world: &World, label: &str, entity: Entity) {
    println!("  [{label}] entity {:?}:", entity);
    if let Some(pos) = world.get::<Pos>(entity) {
        println!("    Pos({:.0}, {:.0})", pos.x, pos.y);
    }
    if let Some(hp) = world.get::<Hp>(entity) {
        println!("    Hp({}/{})", hp.current, hp.max);
    }
    if let Some(armor) = world.get::<Armor>(entity) {
        println!("    Armor({})", armor.0);
    }
    if let Some(w) = world.get::<Weapon>(entity) {
        println!("    Weapon({}, dmg={})", w.name, w.damage);
    }
    if let Some(t) = world.get::<Team>(entity) {
        println!("    Team({})", t.0);
    }
    if let Some(s) = world.get::<Speed>(entity) {
        println!("    Speed({})", s.0);
    }
}

fn main() {
    let mut world = World::new();

    println!("═══ 1. Вложенный Bundle: WarriorBundle ═══\n");
    {
        let warrior = world.spawn(WarriorBundle {
            base: PlayerBase {
                pos:  Pos { x: 10.0, y: 20.0 },
                hp:   Hp { current: 100.0, max: 100.0 },
                team: Team(1),
            },
            weapon: Weapon { name: "Меч", damage: 25.0 },
            armor:  Armor(50.0),
        });
        print_entity(&world, "warrior", warrior);
    }

    println!("\n═══ 2. Кортеж Bundle + компоненты: (PlayerBase, Speed) ═══\n");
    {
        // Кортеж: Bundle-структура + одиночный компонент
        let scout = world.spawn((
            PlayerBase {
                pos:  Pos { x: 30.0, y: 40.0 },
                hp:   Hp { current: 75.0, max: 75.0 },
                team: Team(2),
            },
            Speed(5.0),   // <— одиночный компонент напрямую (blanket impl Bundle)
        ));
        print_entity(&world, "scout", scout);
    }

    println!("\n═══ 3. Прямой spawn компонента ═══\n");
    {
        // Компонент напрямую в spawn() — работает через blanket impl
        let marker = world.spawn(Pos { x: 99.0, y: 99.0 });
        print_entity(&world, "marker", marker);
    }

    println!("\n═══ 4. ScoutBundle (вложенный Bundle с полем Speed) ═══\n");
    {
        // ScoutBundle — такой же вложенный Bundle как WarriorBundle
        let scout2 = world.spawn(ScoutBundle {
            base: PlayerBase {
                pos:  Pos { x: 50.0, y: 60.0 },
                hp:   Hp { current: 60.0, max: 60.0 },
                team: Team(2),
            },
            speed: Speed(8.0),
        });
        print_entity(&world, "scout2", scout2);
    }

    println!("\n═══ 5. Смешанный кортеж: компонент + Bundle + компонент ═══\n");
    {
        // Hp напрямую + PlayerBase + Armor — порядок не важен
        let e = world.spawn((
            Hp { current: 200.0, max: 200.0 },
            PlayerBase {
                pos:  Pos { x: 70.0, y: 80.0 },
                hp:   Hp { current: 150.0, max: 150.0 },
                team: Team(3),
            },
            Armor(100.0),
        ));
        print_entity(&world, "mixed", e);
        // Примечание: Hp в кортеже дублируется с Hp внутри PlayerBase.
        // В одной сущности компонент Hp — один. Побеждает последний записанный.
        println!("      (Hp записан дважды — см. код)");
    }

    println!("\n═══ 6. Разные способы spawn ═══\n");
    {
        // Bundle-структура
        let boss = world.spawn(WarriorBundle {
            base: PlayerBase {
                pos: Pos { x: 0.0, y: 0.0 }, hp: Hp { current: 500.0, max: 500.0 }, team: Team(1),
            },
            weapon: Weapon { name: "Топор", damage: 30.0 },
            armor: Armor(80.0),
        });

        // Кортеж из трёх компонентов
        let minion = world.spawn((
            Pos { x: 5.0, y: 5.0 },
            Hp { current: 10.0, max: 10.0 },
            Team(4),
        ));

        // Пустая сущность
        let empty = world.spawn(());

        print_entity(&world, "boss", boss);
        print_entity(&world, "minion", minion);
        print_entity(&world, "empty", empty);
    }

    println!("\n═══ 7. Query по компонентам из вложенных Bundle ═══\n");
    {
        let query = world.query::<(Read<Pos>, Read<Hp>)>();
        let mut count = 0;
        query.for_each(|entity, (pos, hp)| {
            count += 1;
            println!(
                "  {:?}: Pos({:.0},{:.0}) HP({}/{})",
                entity, pos.x, pos.y, hp.current, hp.max
            );
        });
        println!("  Всего entity с Pos+Hp: {}", count);
    }
}
