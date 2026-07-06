//! Tier-0 spike proof: is a flattened SoA depth-sorted transform propagation
//! (Unity DOTS / Unreal style) worth it against the current `propagate_transforms` (relation walk +
//! scattered get/set)? We measure HONESTLY — not just the cache kernel, but also gather (pull changed
//! `LocalTransform` into a flat array) and scatter (write `GlobalTransform` back into ECS), since these
//! are exactly what can eat the win if transforms stay ECS components.
//!
//! Profile ~ many_foxes ring-parent @10000: 50 rings → 200 characters → a chain of 27 "bones"
//! ≈ 280k nodes, few roots — the case that exposed the propagation perf bug.
//!
//! Two scenarios:
//!   A. EVERYTHING animates (crowd worst case): the whole graph is dirty every frame.
//!   B. Little moves (1000 leaves): the current propagation only descends into their subtrees, whereas
//!      the naive flat sweep still recomputes the WHOLE array — showing the weakness of flat without
//!      dirty-range.
//!
//! Run: `cargo run --release -p apex-bench --bin propagate_soa`
//! Tuning: `RINGS=50 CHARS=200 BONES=27 cargo run --release -p apex-bench --bin propagate_soa`

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

/// "Animation frame": tick the world and shift `LocalTransform` on a list of nodes (stamps change-tick).
fn animate(world: &mut World, list: &[Entity]) {
    world.tick();
    for &e in list {
        if let Some(mut l) = world.get_mut::<LocalTransform>(e) {
            l.translation.x += 0.001;
        }
    }
}

fn main() {
    let rings = env_usize("RINGS", 50);
    let chars = env_usize("CHARS", 200);
    let bones = env_usize("BONES", 27);
    println!("=== Tier-0 spike: flat SoA propagation vs current (measure-first) ===\n");

    // ── Building the world: ring(root) → character → bone chain ──
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
        "World: {} nodes, {} ring roots × {} characters × {} bones (built in {:.1} ms)\n",
        n,
        rings,
        chars,
        bones,
        ms(t.elapsed())
    );

    // ── Building the flat depth-sorted structure (BFS by levels) ──
    // slot = position in BFS order: parent is always before its child (parent_slot[i] < i).
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
                for c in world.targets_of(ChildOf, e) {
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
    let is_root_level0 = level_ranges[0].1; // slots [0..is_root_level0) — roots
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

    // Gather initial local (Mat4 + Affine3A) and global buffers.
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
        "Flat structure: {} levels (width of level 1 = {}, of last = {})\n",
        level_ranges.len(),
        level_ranges.get(1).map(|r| r.1 - r.0).unwrap_or(0),
        level_ranges.last().map(|r| r.1 - r.0).unwrap_or(0),
    );

    // ── Flat-sweep kernels ──
    let kernel_seq_mat4 = |gm: &mut [Mat4]| {
        for slot in 0..n {
            let ps = parent_slot[slot];
            let pg = if ps < 0 { Mat4::IDENTITY } else { gm[ps as usize] };
            gm[slot] = pg * local_mat[slot];
        }
    };
    // Parallel by level: all nodes of a level are independent; the parent (previous level) is already written.
    let kernel_par_mat4 = |gm: &mut [Mat4]| {
        use rayon::prelude::*;
        let gptr = gm.as_mut_ptr() as usize;
        for &(s, e) in &level_ranges {
            (s..e).into_par_iter().for_each(|i| {
                let g = gptr as *mut Mat4;
                let ps = parent_slot[i];
                // SAFETY: level barrier (par_iter blocks until the level ends): the parent from the PREVIOUS
                // level is already written and is not being written now; nodes of the current level are disjoint.
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

    // ── Sanity: flat kernel == current propagation (same math) ──
    propagate_transforms(&mut world);
    kernel_seq_mat4(&mut global_mat);
    let mut max_err = 0.0f32;
    for (slot, &e) in order.iter().enumerate() {
        let wg = world.get::<GlobalTransform>(e).unwrap().0;
        let err = (wg - global_mat[slot]).to_cols_array().iter().map(|x| x.abs()).fold(0.0, f32::max);
        max_err = max_err.max(err);
    }
    println!("Sanity: max |flat_global − world_global| = {max_err:.2e} (should be ~0)\n");
    assert!(max_err < 1e-3, "flat kernel diverged from propagation");

    // Lists for animation.
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

    // ── Measurements ──
    println!("Measurements (median of {SAMPLES}):\n");

    // Baseline: ONLY propagation (animation is shared by both worlds and upstream, not timed; otherwise
    // the baseline is inflated by the cost of writing 280k local and the comparison is biased toward flat).
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

    // Gather: pull changed local from ECS into a flat array (scattered read).
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

    // Kernels (do not touch the world).
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

    // Scatter: write global back into ECS GlobalTransform (scattered write).
    let (s_all, _) = median_of(|| {
        for &e in &all_nodes {
            let slot = entity_to_slot[e.index() as usize] as usize;
            if let Some(mut g) = world.get_mut::<GlobalTransform>(e) {
                g.0 = global_mat[slot];
            }
        }
        0
    });
    let (s_few, _) = median_of(|| {
        for &e in &leaves {
            let slot = entity_to_slot[e.index() as usize] as usize;
            if let Some(mut g) = world.get_mut::<GlobalTransform>(e) {
                g.0 = global_mat[slot];
            }
        }
        0
    });

    // ── Report ──
    let row = |name: &str, d: Duration| {
        println!("  {:<52} {:>9.3} ms   ({:.1} ns/node)", name, ms(d), ms(d) * 1e6 / n as f64);
    };
    println!("Primitives:");
    row("current propagate — ALL dirty", b_all);
    row("current propagate — 1000 leaves dirty", b_few);
    row("flat gather (all, scattered read local)", g_all);
    row("flat gather (1000, scattered read local)", g_few);
    row("flat kernel seq (Mat4)", k_seq);
    row("flat kernel par-by-level (Mat4)", k_par);
    row("flat kernel par-by-level (Affine3A)", k_aff);
    row("flat scatter (all, scattered write global)", s_all);
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
    println!("\nScenario A — EVERYTHING animates (worst-case crowd, {} dirty):", n);
    combo("flat, consumers read ECS", &[("gather", g_all), ("kernel_aff", k_aff), ("scatter", s_all)], b_all);
    combo("flat, consumers read FLAT", &[("gather", g_all), ("kernel_aff", k_aff)], b_all);
    combo("flat, ceiling (kernel only)", &[("kernel_aff", k_aff)], b_all);

    println!("\nScenario B — little moves (1000 leaves):");
    combo("flat naive (full kernel!)", &[("gather", g_few), ("kernel_aff", k_aff), ("scatter", s_few)], b_few);
    println!("  current only descends into leaf subtrees → cheap; flat without dirty-range runs the WHOLE sweep.");
}
