//! Tier-0 спайк-доказательство: стоит ли flattened SoA depth-sorted пропагация трансформов
//! (Unity DOTS / Unreal-стиль) против текущего `propagate_transforms` (обход связей + scattered
//! get/set). Меряем ЧЕСТНО — не только кэш-ядро, но и gather (затащить changed `LocalTransform` в
//! плоский массив) и scatter (вернуть `GlobalTransform` в ECS), т.к. именно они могут съесть выигрыш,
//! если трансформы остаются ECS-компонентами.
//!
//! Профиль ~ many_foxes ring-parent @10000: 50 колец → 200 персонажей → цепочка из 27 «костей»
//! ≈ 280k узлов, немного корней — кейс, вскрывший перф-баг пропагации.
//!
//! Два сценария:
//!   A. ВСЁ анимируется (worst case толпы): весь граф dirty каждый кадр.
//!   B. Мало движется (1000 листьев): текущая пропагация спускается лишь по их поддеревьям, а
//!      наивный flat-свип всё равно пересчитывает ВЕСЬ массив — показывает слабость flat без
//!      dirty-range.
//!
//! Запуск: `cargo run --release -p apex-bench --bin propagate_soa`
//! Тюнинг: `RINGS=50 CHARS=200 BONES=27 cargo run --release -p apex-bench --bin propagate_soa`

use apex_core::prelude::*;
use apex_core::transform::{
    propagate_transforms, GlobalTransform, LocalTransform, TransformPlugin,
};
use glam::{Affine3A, Mat4, Quat, Vec3};
use std::hint::black_box;
use std::time::{Duration, Instant};

const SAMPLES: usize = 15;

fn median_of<F: FnMut() -> u64>(mut f: F) -> (Duration, u64) {
    let mut times = Vec::with_capacity(SAMPLES);
    let mut sink = 0u64;
    for _ in 0..SAMPLES {
        let t = Instant::now();
        sink = black_box(f());
        times.push(t.elapsed());
    }
    times.sort();
    (times[SAMPLES / 2], sink)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// «Кадр анимации»: тикнуть мир и сдвинуть `LocalTransform` у списка узлов (стампит change-tick).
fn animate(world: &mut World, list: &[Entity]) {
    world.tick();
    for &e in list {
        if let Some(l) = world.get_mut::<LocalTransform>(e) {
            l.translation.x += 0.001;
        }
    }
}

fn main() {
    let rings = env_usize("RINGS", 50);
    let chars = env_usize("CHARS", 200);
    let bones = env_usize("BONES", 27);
    println!("=== Tier-0 спайк: flat SoA пропагация vs текущая (measure-first) ===\n");

    // ── Построение мира: кольцо(root) → персонаж → цепочка костей ──
    let mut world = World::new();
    TransformPlugin::register_components(&mut world);
    let t = Instant::now();
    let mut roots = Vec::new();
    let lt = |x: f32, y: f32, z: f32| LocalTransform {
        translation: Vec3::new(x, y, z),
        rotation: Quat::from_rotation_y(0.01 * (x + y + z)),
        scale: Vec3::ONE,
    };
    for r in 0..rings {
        let ring = world.spawn((lt(r as f32, 0.0, 0.0), GlobalTransform(Mat4::IDENTITY)));
        roots.push(ring);
        for c in 0..chars {
            let ch = world.spawn((lt(0.0, c as f32, 0.0), GlobalTransform(Mat4::IDENTITY)));
            world.add_relation(ch, ChildOf, ring);
            let mut parent = ch;
            for b in 0..bones {
                let bone = world.spawn((lt(0.0, 0.0, b as f32 + 1.0), GlobalTransform(Mat4::IDENTITY)));
                world.add_relation(bone, ChildOf, parent);
                parent = bone;
            }
        }
    }
    let n = world.entity_count();
    println!(
        "Мир: {} узлов, {} корней-колец × {} персонажей × {} костей (построен за {:.1} ms)\n",
        n,
        rings,
        chars,
        bones,
        ms(t.elapsed())
    );

    // ── Построение плоской depth-sorted структуры (BFS по уровням) ──
    // slot = позиция в BFS-порядке: родитель всегда раньше ребёнка (parent_slot[i] < i).
    let mut order: Vec<Entity> = Vec::with_capacity(n);
    let mut level_ranges: Vec<(usize, usize)> = Vec::new();
    let mut parent_of: Vec<Entity> = Vec::with_capacity(n);
    {
        let mut frontier = roots.clone();
        let mut parents_frontier: Vec<Option<Entity>> = vec![None; frontier.len()];
        while !frontier.is_empty() {
            let start = order.len();
            for (i, &e) in frontier.iter().enumerate() {
                order.push(e);
                parent_of.push(parents_frontier[i].unwrap_or(e)); // root: parent = self (sentinel)
            }
            level_ranges.push((start, order.len()));
            let mut next = Vec::new();
            let mut next_parents = Vec::new();
            for &e in &frontier {
                for c in world.children_of(ChildOf, e) {
                    next.push(c);
                    next_parents.push(Some(e));
                }
            }
            frontier = next;
            parents_frontier = next_parents;
        }
    }
    // entity.index() → slot
    let max_index = order.iter().map(|e| e.index() as usize).max().unwrap_or(0);
    let mut entity_to_slot = vec![u32::MAX; max_index + 1];
    for (slot, &e) in order.iter().enumerate() {
        entity_to_slot[e.index() as usize] = slot as u32;
    }
    let is_root_level0 = level_ranges[0].1; // slots [0..is_root_level0) — корни
    let parent_slot: Vec<i32> = order
        .iter()
        .enumerate()
        .map(|(slot, _)| {
            if slot < is_root_level0 {
                -1
            } else {
                entity_to_slot[parent_of[slot].index() as usize] as i32
            }
        })
        .collect();

    // Gather начальных local (Mat4 + Affine3A) и буферы global.
    let local_mat: Vec<Mat4> = order
        .iter()
        .map(|&e| world.get::<LocalTransform>(e).unwrap().to_matrix())
        .collect();
    let local_aff: Vec<Affine3A> = order
        .iter()
        .map(|&e| {
            let l = world.get::<LocalTransform>(e).unwrap();
            Affine3A::from_scale_rotation_translation(l.scale, l.rotation, l.translation)
        })
        .collect();
    let mut global_mat = vec![Mat4::IDENTITY; n];
    let mut global_aff = vec![Affine3A::IDENTITY; n];
    println!(
        "Плоская структура: {} уровней (ширина уровня 1 = {}, последнего = {})\n",
        level_ranges.len(),
        level_ranges.get(1).map(|r| r.1 - r.0).unwrap_or(0),
        level_ranges.last().map(|r| r.1 - r.0).unwrap_or(0),
    );

    // ── Ядра flat-свипа ──
    let kernel_seq_mat4 = |gm: &mut [Mat4]| {
        for slot in 0..n {
            let ps = parent_slot[slot];
            let pg = if ps < 0 { Mat4::IDENTITY } else { gm[ps as usize] };
            gm[slot] = pg * local_mat[slot];
        }
    };
    // Параллельно по уровням: все узлы уровня независимы; родитель (предыдущий уровень) уже записан.
    let kernel_par_mat4 = |gm: &mut [Mat4]| {
        use rayon::prelude::*;
        let gptr = gm.as_mut_ptr() as usize;
        for &(s, e) in &level_ranges {
            (s..e).into_par_iter().for_each(|i| {
                let g = gptr as *mut Mat4;
                let ps = parent_slot[i];
                // SAFETY: уровень-барьер (par_iter блокирует до конца уровня): родитель из ПРЕДЫДУЩЕГО
                // уровня уже записан и не пишется сейчас; узлы текущего уровня дизъюнктны.
                let pg = if ps < 0 { Mat4::IDENTITY } else { unsafe { *g.add(ps as usize) } };
                unsafe { *g.add(i) = pg * local_mat[i] };
            });
        }
    };
    let kernel_par_aff = |ga: &mut [Affine3A]| {
        use rayon::prelude::*;
        let gptr = ga.as_mut_ptr() as usize;
        for &(s, e) in &level_ranges {
            (s..e).into_par_iter().for_each(|i| {
                let g = gptr as *mut Affine3A;
                let ps = parent_slot[i];
                let pg = if ps < 0 { Affine3A::IDENTITY } else { unsafe { *g.add(ps as usize) } };
                unsafe { *g.add(i) = pg * local_aff[i] };
            });
        }
    };

    // ── Sanity: flat-ядро == текущая пропагация (та же математика) ──
    propagate_transforms(&mut world);
    kernel_seq_mat4(&mut global_mat);
    let mut max_err = 0.0f32;
    for (slot, &e) in order.iter().enumerate() {
        let wg = world.get::<GlobalTransform>(e).unwrap().0;
        let err = (wg - global_mat[slot]).to_cols_array().iter().map(|x| x.abs()).fold(0.0, f32::max);
        max_err = max_err.max(err);
    }
    println!("Sanity: max |flat_global − world_global| = {max_err:.2e} (должно быть ~0)\n");
    assert!(max_err < 1e-3, "flat-ядро разошлось с пропагацией");

    // Списки для анимации.
    let all_nodes: Vec<Entity> = order[is_root_level0..].to_vec();
    let leaves: Vec<Entity> = {
        let (s, e) = *level_ranges.last().unwrap();
        let mut v = Vec::new();
        let mut st = 0x9e3779b9u64;
        for _ in 0..1000.min(e - s) {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1);
            v.push(order[s + (st >> 33) as usize % (e - s)]);
        }
        v
    };

    // ── Замеры ──
    println!("Замеры (медиана из {SAMPLES}):\n");

    // Baseline: ТОЛЬКО пропагация (анимация — общая для обоих миров и upstream, не таймится; иначе
    // baseline раздут на стоимость записи 280k local и сравнение смещено в пользу flat).
    let bench_propagate = |world: &mut World, list: &[Entity]| -> Duration {
        let mut times = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            animate(world, list); // untimed
            let t = Instant::now();
            propagate_transforms(world);
            times.push(t.elapsed());
        }
        times.sort();
        times[SAMPLES / 2]
    };
    let b_all = bench_propagate(&mut world, &all_nodes);
    let b_few = bench_propagate(&mut world, &leaves);

    // Gather: затащить changed local из ECS в плоский массив (scattered read).
    let mut local_scratch = local_mat.clone();
    let (g_all, _) = median_of(|| {
        for &e in &all_nodes {
            let slot = entity_to_slot[e.index() as usize] as usize;
            local_scratch[slot] = world.get::<LocalTransform>(e).unwrap().to_matrix();
        }
        local_scratch[0].col(0).x.to_bits() as u64
    });
    let (g_few, _) = median_of(|| {
        for &e in &leaves {
            let slot = entity_to_slot[e.index() as usize] as usize;
            local_scratch[slot] = world.get::<LocalTransform>(e).unwrap().to_matrix();
        }
        local_scratch[0].col(0).x.to_bits() as u64
    });

    // Kernels (мир не трогают).
    let (k_seq, _) = median_of(|| {
        kernel_seq_mat4(&mut global_mat);
        global_mat[n - 1].col(3).x.to_bits() as u64
    });
    let (k_par, _) = median_of(|| {
        kernel_par_mat4(&mut global_mat);
        global_mat[n - 1].col(3).x.to_bits() as u64
    });
    let (k_aff, _) = median_of(|| {
        kernel_par_aff(&mut global_aff);
        global_aff[n - 1].translation.x.to_bits() as u64
    });

    // Scatter: вернуть global в ECS GlobalTransform (scattered write).
    let (s_all, _) = median_of(|| {
        for &e in &all_nodes {
            let slot = entity_to_slot[e.index() as usize] as usize;
            if let Some(g) = world.get_mut::<GlobalTransform>(e) {
                g.0 = global_mat[slot];
            }
        }
        0
    });
    let (s_few, _) = median_of(|| {
        for &e in &leaves {
            let slot = entity_to_slot[e.index() as usize] as usize;
            if let Some(g) = world.get_mut::<GlobalTransform>(e) {
                g.0 = global_mat[slot];
            }
        }
        0
    });

    // ── Отчёт ──
    let row = |name: &str, d: Duration| {
        println!("  {:<52} {:>9.3} ms   ({:.1} ns/узел)", name, ms(d), ms(d) * 1e6 / n as f64);
    };
    println!("Примитивы:");
    row("current propagate — ВСЁ dirty", b_all);
    row("current propagate — 1000 листьев dirty", b_few);
    row("flat gather (всё, scattered read local)", g_all);
    row("flat gather (1000, scattered read local)", g_few);
    row("flat kernel seq (Mat4)", k_seq);
    row("flat kernel par-by-level (Mat4)", k_par);
    row("flat kernel par-by-level (Affine3A)", k_aff);
    row("flat scatter (всё, scattered write global)", s_all);
    row("flat scatter (1000, scattered write global)", s_few);

    let combo = |label: &str, parts: &[(&str, Duration)], base: Duration| {
        let sum: f64 = parts.iter().map(|(_, d)| ms(*d)).sum();
        let speedup = ms(base) / sum;
        let names = parts.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" + ");
        println!(
            "  {:<40} {:>9.3} ms   vs current {:>7.3} ms  → {:.2}×  [{}]",
            label, sum, ms(base), speedup, names
        );
    };
    println!("\nСценарий A — ВСЁ анимируется (worst-case толпа, {} dirty):", n);
    combo("flat, потребители читают ECS", &[("gather", g_all), ("kernel_aff", k_aff), ("scatter", s_all)], b_all);
    combo("flat, потребители читают FLAT", &[("gather", g_all), ("kernel_aff", k_aff)], b_all);
    combo("flat, потолок (только ядро)", &[("kernel_aff", k_aff)], b_all);

    println!("\nСценарий B — мало движется (1000 листьев):");
    combo("flat наивный (полное ядро!)", &[("gather", g_few), ("kernel_aff", k_aff), ("scatter", s_few)], b_few);
    println!("  current спускается лишь по поддеревьям листьев → дёшево; flat без dirty-range гонит ВЕСЬ свип.");
}
