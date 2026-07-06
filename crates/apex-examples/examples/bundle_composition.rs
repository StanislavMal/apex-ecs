//! Apex ECS — Bundle composition demo
//!
//! Demonstrates nested Bundles, Bundle tuples, and direct component spawn:
//!
//! 1. **Nested #[derive(Bundle)]** — a field of one Bundle inside another
//! 2. **Bundle tuples** — `(BundleA, ComponentB, ComponentC)` in spawn()
//! 3. **Direct component spawn** — `spawn(MyComponent)` without a tuple
//! 4. **Mixed tuples** — Bundle structs + individual components
//!
//! ```bash
//! cargo run -p apex-examples --example bundle_composition
//! ```

use apex_core::prelude::*;

// ── Components ─────────────────────────────────────────────────
//
// Every component is `#[derive(Component)]`, which:
// - implements the `Component` trait (previously a blanket impl)
// - automatically registers the component in the World (via linkme)
// - makes the type usable as a single-element Bundle

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

// ── Bundle: base player set (reusable) ──────────

#[derive(Bundle)]
struct PlayerBase {
    pos:  Pos,
    hp:   Hp,
    team: Team,
}

// ── Bundle: extended set (nested Bundle + components) ─
//
// Note: the `base: PlayerBase` field is a NESTED Bundle.
// The `#[derive(Bundle)]` macro recursively expands it into the set
// of components [Pos, Hp, Team] plus the additional fields.

#[derive(Bundle)]
struct WarriorBundle {
    base:   PlayerBase,   // <— nested Bundle
    weapon: Weapon,
    armor:  Armor,
}

#[derive(Bundle)]
struct ScoutBundle {
    base:  PlayerBase,    // <— nested Bundle
    speed: Speed,
}

// ═══════════════════════════════════════════════════════════════
// Demo functions
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

    println!("═══ 1. Nested Bundle: WarriorBundle ═══\n");
    {
        let warrior = world.spawn(WarriorBundle {
            base: PlayerBase {
                pos:  Pos { x: 10.0, y: 20.0 },
                hp:   Hp { current: 100.0, max: 100.0 },
                team: Team(1),
            },
            weapon: Weapon { name: "Sword", damage: 25.0 },
            armor:  Armor(50.0),
        });
        print_entity(&world, "warrior", warrior);
    }

    println!("\n═══ 2. Bundle tuple + components: (PlayerBase, Speed) ═══\n");
    {
        // Tuple: Bundle struct + single component
        let scout = world.spawn((
            PlayerBase {
                pos:  Pos { x: 30.0, y: 40.0 },
                hp:   Hp { current: 75.0, max: 75.0 },
                team: Team(2),
            },
            Speed(5.0),   // <— single component directly (blanket impl Bundle)
        ));
        print_entity(&world, "scout", scout);
    }

    println!("\n═══ 3. Direct component spawn ═══\n");
    {
        // Component directly in spawn() — works via blanket impl
        let marker = world.spawn(Pos { x: 99.0, y: 99.0 });
        print_entity(&world, "marker", marker);
    }

    println!("\n═══ 4. ScoutBundle (nested Bundle with a Speed field) ═══\n");
    {
        // ScoutBundle — a nested Bundle just like WarriorBundle
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

    println!("\n═══ 5. Mixed tuple: component + Bundle + component ═══\n");
    {
        // Speed directly + PlayerBase + Armor — order does not matter.
        // IMPORTANT: every component type in a bundle must be UNIQUE —
        // a duplicate (e.g. Hp directly AND inside PlayerBase) panics.
        let e = world.spawn((
            Speed(20.0),
            PlayerBase {
                pos:  Pos { x: 70.0, y: 80.0 },
                hp:   Hp { current: 150.0, max: 150.0 },
                team: Team(3),
            },
            Armor(100.0),
        ));
        print_entity(&world, "mixed", e);
    }

    println!("\n═══ 6. Different ways to spawn ═══\n");
    {
        // Bundle struct
        let boss = world.spawn(WarriorBundle {
            base: PlayerBase {
                pos: Pos { x: 0.0, y: 0.0 }, hp: Hp { current: 500.0, max: 500.0 }, team: Team(1),
            },
            weapon: Weapon { name: "Axe", damage: 30.0 },
            armor: Armor(80.0),
        });

        // Tuple of three components
        let minion = world.spawn((
            Pos { x: 5.0, y: 5.0 },
            Hp { current: 10.0, max: 10.0 },
            Team(4),
        ));

        // Empty entity
        let empty = world.spawn(());

        print_entity(&world, "boss", boss);
        print_entity(&world, "minion", minion);
        print_entity(&world, "empty", empty);
    }

    println!("\n═══ 7. Query over components from nested Bundles ═══\n");
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
        println!("  Total entities with Pos+Hp: {}", count);
    }
}
