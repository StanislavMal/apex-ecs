//! CR-M0 — бенч «фрагментированный мир» (plans/CORE_REFACTORING.md, apex-engine).
//!
//! Профиль ~ many_foxes @1000: ~28k entity, 1000 деревьев
//! root → n1 → … → n26 → prim (цепочка), т.е. 27 уникальных родителей на дерево
//! → ~27k уникальных родителей → при текущей модели relations (пара в идентичности
//! архетипа) мир фрагментируется на ~27k архетипов.
//!
//! Меряем (порядок прогона: 1, 3, 4, 5 на стабильном мире; 2 — спавн — последним,
//! т.к. он растит мир и добавляет архетипы):
//!   1. `Query::new` ×8 типовых форм (1-3 компонента, Maybe/Without/With/Changed/редкий);
//!   2. spawn поддерева (28 spawn + 27 add_relation);
//!   3. `children_of` по всем родителям; `get_relation_target` по всем subjects;
//!   4. random `get_mut` по всем entity;
//!   5. «extract-подобный» цикл: 18 Query::new + полные итерации (прокси MAIN commit).
//!
//! Числа ДО/ПОСЛЕ фиксируются в CORE_REFACTORING.md и §14.7 руководства.
//! Регресс-страж: падение >20% на этом бенче — блокер мержа.
//!
//! Запуск: `cargo run --release -p apex-bench --bin frag_world`

use apex_bench::{Position, Rotation, Transform, Velocity};
use apex_core::prelude::*;
use apex_core::Query;
use apex_macros::Component;
use cgmath::{Matrix4, Vector3};
use std::hint::black_box;
use std::time::{Duration, Instant};

// ── Дополнительные компоненты профиля ──────────────────────────

/// Редкий компонент (8 шт. на мир) — аналог «8 пустых Query::new по свету»
/// из extract'а движка (находка C-4: «lights 0.46ms»).
#[derive(Component, Clone, Copy)]
struct Light {
    intensity: f32,
}

/// Маркер прим-листа (как mesh-prim у лисы).
#[derive(Component, Clone, Copy)]
struct Prim {
    lod: u32,
}

const TREES: usize = 1000;
const CHAIN: usize = 26; // root → n1..n26 → prim
const LIGHTS: usize = 8;

// ── Тайминг: медиана из SAMPLES прогонов ───────────────────────

const SAMPLES: usize = 9;

fn median_of<F: FnMut() -> u64>(mut f: F) -> (Duration, u64) {
    let mut times: Vec<Duration> = Vec::with_capacity(SAMPLES);
    let mut sink = 0u64;
    for _ in 0..SAMPLES {
        let t = Instant::now();
        sink = black_box(f()); // значение ПОСЛЕДНЕГО сэмпла (для assert'ов)
        times.push(t.elapsed());
    }
    times.sort();
    (times[SAMPLES / 2], sink)
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us >= 1000.0 {
        format!("{:>10.3} ms", us / 1000.0)
    } else {
        format!("{:>10.3} µs", us)
    }
}

struct Report {
    rows: Vec<(String, Duration, String)>,
}

impl Report {
    fn add(&mut self, name: &str, total: Duration, per_op: Option<(u64, &str)>) {
        let note = match per_op {
            Some((n, unit)) if n > 0 => {
                let per = total.as_secs_f64() * 1e9 / n as f64;
                if per >= 1000.0 {
                    format!("{:.3} µs/{unit}", per / 1000.0)
                } else {
                    format!("{per:.1} ns/{unit}")
                }
            }
            _ => String::new(),
        };
        println!("  {:<46} {}   {}", name, fmt_dur(total), note);
        self.rows.push((name.to_string(), total, note));
    }
}

// ── Построение фрагментированного мира ─────────────────────────

struct FragWorld {
    world: World,
    roots: Vec<Entity>,
    /// Все родители (root + 26 узлов на дерево), 27k шт.
    parents: Vec<Entity>,
    /// Все subjects связей (узлы + прим), 27k шт.
    subjects: Vec<Entity>,
    /// Все entity деревьев (28k) в перемешанном порядке — для random get_mut.
    shuffled: Vec<Entity>,
}

fn spawn_tree(world: &mut World, idx: usize) -> (Entity, Vec<Entity>, Vec<Entity>, Vec<Entity>) {
    let fi = idx as f32;
    let root = world.spawn((
        Transform(Matrix4::from_scale(1.0)),
        Position(Vector3::new(fi, 0.0, 0.0)),
        Rotation(Vector3::new(0.0, fi, 0.0)),
        Velocity(Vector3::new(1.0, 0.0, 0.0)),
    ));
    let mut parents = vec![root];
    let mut subjects = Vec::with_capacity(CHAIN + 1);
    let mut all = vec![root];

    let mut parent = root;
    for n in 0..CHAIN {
        let node = world.spawn((
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(fi, n as f32, 0.0)),
            Rotation(Vector3::new(0.0, fi, n as f32)),
        ));
        world.add_relation(node, ChildOf, parent);
        subjects.push(node);
        parents.push(node);
        all.push(node);
        parent = node;
    }
    let prim = world.spawn((
        Transform(Matrix4::from_scale(1.0)),
        Position(Vector3::new(fi, -1.0, 0.0)),
        Prim { lod: 0 },
    ));
    world.add_relation(prim, ChildOf, parent);
    subjects.push(prim);
    all.push(prim);
    // последний узел — родитель прима, но он уже в parents
    (root, parents, subjects, all)
}

fn build() -> FragWorld {
    let mut world = World::new();

    let t = Instant::now();
    let mut roots = Vec::with_capacity(TREES);
    let mut parents = Vec::with_capacity(TREES * (CHAIN + 1));
    let mut subjects = Vec::with_capacity(TREES * (CHAIN + 1));
    let mut all = Vec::with_capacity(TREES * (CHAIN + 2));

    for i in 0..TREES {
        let (root, p, s, a) = spawn_tree(&mut world, i);
        roots.push(root);
        parents.extend(p);
        subjects.extend(s);
        all.extend(a);
    }

    // 8 «светильников» — редкий компонент.
    for i in 0..LIGHTS {
        let e = world.spawn((
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(i as f32, 100.0, 0.0)),
            Light { intensity: 1.0 },
        ));
        all.push(e);
    }
    let build_time = t.elapsed();

    // Детерминированный шафл (LCG) — без зависимости от rand.
    let mut shuffled = all;
    let mut state = 0x9e3779b97f4a7c15u64;
    for i in (1..shuffled.len()).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        shuffled.swap(i, j);
    }

    println!(
        "Мир построен за {}: {} entity, {} архетипов, {} деревьев × (root→{}→prim)",
        fmt_dur(build_time),
        world.entity_count(),
        world.archetype_count(),
        TREES,
        CHAIN,
    );

    FragWorld {
        world,
        roots,
        parents,
        subjects,
        shuffled,
    }
}

// ── 1. Query::new ×8 форм ──────────────────────────────────────

fn bench_query_new(fw: &FragWorld, rep: &mut Report) {
    let w = &fw.world;
    const N: u64 = 20; // конструирований на сэмпл

    println!("\n[1] Query::new — только конструирование, {N}×/сэмпл, медиана из {SAMPLES}:");

    macro_rules! qbench {
        ($name:expr, $q:ty) => {{
            let (t, _) = median_of(|| {
                let mut acc = 0u64;
                for _ in 0..N {
                    let q = Query::<$q>::new(black_box(w));
                    acc = acc.wrapping_add(black_box(&q) as *const _ as u64);
                }
                acc
            });
            rep.add($name, t / N as u32, Some((1, "Query::new")));
        }};
    }

    qbench!("Q1  Read<Transform>            (28k строк)", Read<Transform>);
    qbench!("Q2  (Read<Position>, Read<Velocity>)  (1k)", (Read<Position>, Read<Velocity>));
    qbench!("Q3  (Transform, Position, Rotation) (27k)", (Read<Transform>, Read<Position>, Read<Rotation>));
    qbench!("Q4  (Read<Position>, Maybe<Velocity>)", (Read<Position>, Maybe<Velocity>));
    qbench!("Q5  (Read<Position>, Without<Velocity>)", (Read<Position>, Without<Velocity>));
    qbench!("Q7  (Read<Transform>, With<Prim>)    (1k)", (Read<Transform>, With<Prim>));
    qbench!("Q8  (Read<Light>, Read<Position>)  (8 шт)", (Read<Light>, Read<Position>));

    // Q6: Changed — через new_with_tick (как extract движка).
    let last = w.current_tick();
    let (t, _) = median_of(|| {
        let mut acc = 0u64;
        for _ in 0..N {
            let q = Query::<Changed<Position>>::new_with_tick(black_box(w), last);
            acc = acc.wrapping_add(black_box(&q) as *const _ as u64);
        }
        acc
    });
    rep.add("Q6  Changed<Position> (new_with_tick)", t / N as u32, Some((1, "Query::new")));
}

// ── 3. children_of / get_relation_target ──────────────────────

fn bench_relations_read(fw: &FragWorld, rep: &mut Report) {
    let w = &fw.world;
    println!("\n[3] Обход связей (медиана из {SAMPLES}):");

    let n_parents = fw.parents.len() as u64;
    let (t, sink) = median_of(|| {
        let mut acc = 0u64;
        for &p in &fw.parents {
            acc += w.children_of(ChildOf, p).count() as u64;
        }
        acc
    });
    rep.add(
        &format!("children_of по {} родителям", n_parents),
        t,
        Some((n_parents, "вызов")),
    );
    assert_eq!(sink, fw.subjects.len() as u64, "children_of насчитал не всех детей");

    let n_subj = fw.subjects.len() as u64;
    let (t, sink) = median_of(|| {
        let mut acc = 0u64;
        for &s in &fw.subjects {
            if let Some(target) = w.get_relation_target(s, ChildOf) {
                acc = acc.wrapping_add(target.index() as u64);
            }
        }
        acc
    });
    black_box(sink);
    rep.add(
        &format!("get_relation_target ×{}", n_subj),
        t,
        Some((n_subj, "вызов")),
    );
}

// ── 4. random get_mut ──────────────────────────────────────────

fn bench_random_access(fw: &mut FragWorld, rep: &mut Report) {
    println!("\n[4] Random-access (медиана из {SAMPLES}):");
    let n = fw.shuffled.len() as u64;
    let entities = std::mem::take(&mut fw.shuffled);
    let w = &mut fw.world;

    let (t, sink) = median_of(|| {
        let mut acc = 0u64;
        for &e in &entities {
            if let Some(p) = w.get_mut::<Position>(e) {
                p.0.x += 0.001;
                acc = acc.wrapping_add(p.0.x as u64);
            }
        }
        acc
    });
    black_box(sink);
    rep.add(&format!("get_mut::<Position> ×{} (shuffle)", n), t, Some((n, "вызов")));

    // CR-M3: ComponentId берётся один раз на проход — без TypeId-hash на вызов.
    let pos_id = w.component_id::<Position>().expect("Position зарегистрирован");
    let (t, sink) = median_of(|| {
        let mut acc = 0u64;
        for &e in &entities {
            if let Some(p) = w.get_mut_by_id::<Position>(e, pos_id) {
                p.0.y += 0.001;
                acc = acc.wrapping_add(p.0.y as u64);
            }
        }
        acc
    });
    black_box(sink);
    rep.add(&format!("get_mut_by_id::<Position> ×{} (CR-M3)", n), t, Some((n, "вызов")));

    let (t, sink) = median_of(|| {
        let mut acc = 0u64;
        for &e in &entities {
            acc = acc.wrapping_add(w.has_component::<Velocity>(e) as u64);
        }
        acc
    });
    black_box(sink);
    rep.add(&format!("has_component::<Velocity> ×{}", n), t, Some((n, "вызов")));

    fw.shuffled = entities;
}

// ── 5. extract-подобный цикл ───────────────────────────────────

/// Прокси extract'а движка: ~18 Query::new за «кадр» + полные итерации.
/// Состав запросов имитирует реальный extract: меши (3 комп.), трансформы,
/// Changed-детекция, 8 запросов по редким (свет), маркеры.
fn bench_extract_cycle(fw: &mut FragWorld, rep: &mut Report) {
    println!("\n[5] Extract-подобный цикл, 18 Query::new + итерации (медиана из {SAMPLES}):");

    // «Кадр» движка: между extract'ами что-то меняется — двигаем корни.
    let roots = fw.roots.clone();
    let w = &mut fw.world;

    let (t, sink) = median_of(|| {
        let mut acc = 0u64;

        // симуляция: двигаем 1000 корней (как муверы many_foxes)
        let prev = w.current_tick();
        w.tick();
        for &r in &roots {
            if let Some(p) = w.get_mut::<Position>(r) {
                p.0.x += 0.01;
            }
        }

        // extract: 3 «мешовых» запроса
        for _ in 0..3 {
            let q = Query::<(Read<Transform>, Read<Position>, Read<Rotation>)>::new(w);
            q.for_each(|_, (t, p, _)| {
                acc = acc.wrapping_add((t.0.x.x + p.0.x) as u64);
            });
        }
        // 2 запроса трансформов целиком
        for _ in 0..2 {
            let q = Query::<Read<Transform>>::new(w);
            q.for_each(|_, t| {
                acc = acc.wrapping_add(t.0.w.w as u64);
            });
        }
        // 2 Changed-запроса (дельта кадра)
        for _ in 0..2 {
            let q = Query::<Changed<Position>>::new_with_tick(w, prev);
            q.for_each(|_, _| {
                acc = acc.wrapping_add(1);
            });
        }
        // 8 запросов по редким компонентам (свет: 8 entity)
        for _ in 0..8 {
            let q = Query::<(Read<Light>, Read<Position>)>::new(w);
            q.for_each(|_, (l, p)| {
                acc = acc.wrapping_add((l.intensity + p.0.x) as u64);
            });
        }
        // 2 маркерных (Prim — 1k строк; Velocity — 1k корней)
        let q = Query::<(Read<Position>, Read<Prim>)>::new(w);
        q.for_each(|_, (p, prim)| {
            acc = acc.wrapping_add(p.0.z as u64 + prim.lod as u64);
        });
        let q = Query::<(Read<Position>, Read<Velocity>)>::new(w);
        q.for_each(|_, (p, v)| {
            acc = acc.wrapping_add((p.0.x + v.0.x) as u64);
        });

        acc
    });
    black_box(sink);
    rep.add("extract-цикл (18 Query::new + iter)", t, Some((18, "Query::new")));
}

// ── 2. spawn поддерева (растит мир — гоняем последним) ─────────

fn bench_spawn_subtree(fw: &mut FragWorld, rep: &mut Report) {
    println!("\n[2] Spawn поддерева (28 spawn + 27 add_relation), мир растёт, среднее по 100:");
    let w = &mut fw.world;
    let archetypes_before = w.archetype_count();

    const NEW_TREES: usize = 100;
    let t = Instant::now();
    for i in 0..NEW_TREES {
        let (_, _, _, a) = spawn_tree(w, TREES + i);
        black_box(a.len());
    }
    let total = t.elapsed();

    rep.add(
        &format!("spawn поддерева (среднее по {NEW_TREES})"),
        total / NEW_TREES as u32,
        Some((CHAIN as u64 + 1, "add_relation")),
    );
    println!(
        "    архетипов: {} → {} (+{} за {} деревьев)",
        archetypes_before,
        w.archetype_count(),
        w.archetype_count() - archetypes_before,
        NEW_TREES,
    );
}

// ── main ───────────────────────────────────────────────────────

fn main() {
    println!("=== CR-M0: фрагментированный мир (профиль many_foxes @1000) ===\n");
    let mut fw = build();
    let mut rep = Report { rows: Vec::new() };

    bench_query_new(&fw, &mut rep);
    bench_relations_read(&fw, &mut rep);
    bench_random_access(&mut fw, &mut rep);
    bench_extract_cycle(&mut fw, &mut rep);
    bench_spawn_subtree(&mut fw, &mut rep);

    println!("\n=== Сводка (markdown для CORE_REFACTORING.md / §14.7) ===\n");
    println!("| Операция | Время | На операцию |");
    println!("|----------|-------|-------------|");
    for (name, d, note) in &rep.rows {
        println!("| {} | {} | {} |", name.trim(), fmt_dur(*d).trim(), note);
    }
    println!(
        "\nМир: {} entity, {} архетипов (после spawn-бенча).",
        fw.world.entity_count(),
        fw.world.archetype_count()
    );
}
