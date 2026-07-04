//! D8b spike (WAVE6B step 1): validate the deterministic per-system id-block scheme
//! BEFORE it cements the per-system state shape.
//!
//! Claims to validate:
//!   1. DETERMINISM: a spawn-heavy parallel stage assigns identical entity ids across
//!      N runs, independent of thread timing — because each parallel task fills a
//!      sub-block whose base was pre-assigned in deterministic (rank, task-order),
//!      and within a task a private counter is sequential.
//!   2. PERF (§0.9 bonus): private per-task counters have ZERO cross-thread atomic
//!      contention, so mass parallel spawn is FASTER than the current shared-atomic
//!      `EntityReserver::reserve()`.
//!   3. eager `.id()` semantics: the id is known at reserve time (block base + local
//!      counter), before any flush — same as `commands.spawn().id()`.
//!
//! Run: cargo run -p apex-bench --bin d8b_spike --release

use apex_core::entity::EntityAllocator;
use rayon::prelude::*;
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

// ── Prototype: block reserver ───────────────────────────────────────────────
//
// A task-local reserver over a pre-assigned sub-block `[base, base+cap)`. `reserve`
// is a plain Cell increment — no atomics, no sharing. The base is assigned by the
// scheduler (sequentially, in deterministic order) BEFORE the parallel scope, so the
// id of the j-th spawn in a task is `base + j`, a pure function of (rank, task, j).
struct BlockReserver {
    next: Cell<u32>,
    #[allow(dead_code)]
    end: u32, // block ceiling; overflow → rank-ordered claim (not modeled in the hot path)
}
impl BlockReserver {
    #[inline]
    fn reserve(&self) -> u32 {
        let id = self.next.get();
        self.next.set(id + 1);
        id
    }
}
// Sub-blocks are disjoint by construction; a task only touches its own Cell.
unsafe impl Sync for BlockReserver {}

/// One parallel spawn stage under the block scheme. `tasks` = flat list of
/// (sub_base, spawn_count) in deterministic (system-rank, task-order). Returns the
/// full id assignment as a flat vec ordered by (task-order, local index) — the
/// deterministic logical order.
fn run_block_stage(tasks: &[(u32, u32)]) -> Vec<u32> {
    // Parallel: each task fills its own sub-block via a private counter.
    let per_task: Vec<Vec<u32>> = tasks
        .par_iter()
        .map(|&(base, count)| {
            let r = BlockReserver { next: Cell::new(base), end: base + count };
            (0..count).map(|_| r.reserve()).collect()
        })
        .collect();
    per_task.into_iter().flatten().collect()
}

/// Same workload via the CURRENT shared-atomic reserver (nondeterministic order).
fn run_shared_atomic_stage(reserver: &apex_core::entity::EntityReserver, tasks: &[(u32, u32)]) -> Vec<u32> {
    let per_task: Vec<Vec<u32>> = tasks
        .par_iter()
        .map(|&(_base, count)| (0..count).map(|_| reserver.reserve().index()).collect())
        .collect();
    per_task.into_iter().flatten().collect()
}

fn main() {
    let threads = rayon::current_num_threads();
    println!("D8b spike — rayon threads = {threads}");

    // Model a spawn-heavy stage: K parallel systems, each row-split into T tasks,
    // each task spawns M entities.
    const K: usize = 6; // systems
    const T: usize = 8; // tasks/system (ASD row-split)
    const M: u32 = 500; // spawns/task
    let total = (K * T) as u32 * M;

    // Pre-assign sub-bases in deterministic (rank, task) order (scheduler setup phase).
    let mut tasks: Vec<(u32, u32)> = Vec::with_capacity(K * T);
    let mut base = 0u32;
    for _s in 0..K {
        for _t in 0..T {
            tasks.push((base, M));
            base += M;
        }
    }

    // ── Claim 1: determinism across N runs ──────────────────────────────────
    const N_RUNS: usize = 40;
    let reference = run_block_stage(&tasks);
    let mut all_identical = true;
    for _ in 0..N_RUNS {
        let r = run_block_stage(&tasks);
        if r != reference {
            all_identical = false;
            break;
        }
    }
    // Sanity: the id set is exactly [0, total) with no gaps/dups.
    let mut sorted = reference.clone();
    sorted.sort_unstable();
    let dense = sorted.iter().copied().eq(0..total);
    println!(
        "[1] determinism: {} across {N_RUNS} runs; id set dense [0,{total}) = {}",
        if all_identical { "IDENTICAL ✓" } else { "DIVERGED ✗" },
        dense
    );

    // Contrast: the shared-atomic scheme is order-nondeterministic. Show that the
    // per-logical-position id assignment VARIES run to run (this is what D8b fixes).
    {
        let alloc = EntityAllocator::new();
        let rv = alloc.reserver();
        let a = run_shared_atomic_stage(&rv, &tasks);
        let alloc2 = EntityAllocator::new();
        let rv2 = alloc2.reserver();
        let b = run_shared_atomic_stage(&rv2, &tasks);
        // Both are permutations of [0,total); but positionally they usually differ.
        let positional_same = a == b;
        println!(
            "[1b] shared-atomic positional id assignment identical across 2 runs = {} (expected false — the bug D8b fixes)",
            positional_same
        );
    }

    // ── Claim 2: perf — block (no atomics) vs shared atomic ──────────────────
    const PERF_ITERS: usize = 200;
    // warm up
    for _ in 0..20 {
        let _ = run_block_stage(&tasks);
    }
    let t0 = Instant::now();
    for _ in 0..PERF_ITERS {
        let r = run_block_stage(&tasks);
        std::hint::black_box(&r);
    }
    let block_ns = t0.elapsed().as_nanos() as f64 / PERF_ITERS as f64;

    let alloc = EntityAllocator::new();
    let rv = alloc.reserver();
    for _ in 0..20 {
        let _ = run_shared_atomic_stage(&rv, &tasks);
    }
    // fresh allocator each iter to avoid unbounded high_water growth skewing cache
    let t1 = Instant::now();
    for _ in 0..PERF_ITERS {
        let alloc = EntityAllocator::new();
        let rv = alloc.reserver();
        let r = run_shared_atomic_stage(&rv, &tasks);
        std::hint::black_box(&r);
    }
    let atomic_ns = t1.elapsed().as_nanos() as f64 / PERF_ITERS as f64;

    // Also measure the block scheme with per-iter fresh state (fair vs fresh-atomic).
    let t2 = Instant::now();
    for _ in 0..PERF_ITERS {
        let mut tks = Vec::with_capacity(K * T);
        let mut b = 0u32;
        for _ in 0..K * T {
            tks.push((b, M));
            b += M;
        }
        let r = run_block_stage(&tks);
        std::hint::black_box(&r);
    }
    let block_fresh_ns = t2.elapsed().as_nanos() as f64 / PERF_ITERS as f64;

    println!(
        "[2] mass parallel spawn ({} entities, {}x{} tasks):",
        total, K, T
    );
    println!("    block (reuse tasks):   {:.1} µs", block_ns / 1000.0);
    println!("    block (fresh setup):   {:.1} µs", block_fresh_ns / 1000.0);
    println!("    shared atomic reserve: {:.1} µs", atomic_ns / 1000.0);
    println!(
        "    → block is {:.2}x vs shared atomic (>1 = block faster)",
        atomic_ns / block_fresh_ns
    );

    // ── Claim 3: eager id known at reserve time ──────────────────────────────
    // Trivially true for the block scheme: reserve() returns base+counter with no
    // deferred remap. Demonstrate: a task can reserve an id and immediately use it.
    {
        let r = BlockReserver { next: Cell::new(1000), end: 2000 };
        let id = r.reserve();
        let eager_ok = id == 1000 && r.reserve() == 1001;
        println!("[3] eager .id() (base+counter, no remap): {}", if eager_ok { "ok ✓" } else { "FAIL ✗" });
    }

    // Cross-check with a shared AtomicU32 to quantify contention specifically.
    {
        let shared = AtomicU32::new(0);
        for _ in 0..20 {
            let _: Vec<u32> = (0..K * T)
                .into_par_iter()
                .flat_map(|_| (0..M).map(|_| shared.fetch_add(1, Ordering::Relaxed)).collect::<Vec<_>>())
                .collect();
        }
        let t3 = Instant::now();
        for _ in 0..PERF_ITERS {
            shared.store(0, Ordering::Relaxed);
            let r: Vec<u32> = (0..K * T)
                .into_par_iter()
                .flat_map(|_| (0..M).map(|_| shared.fetch_add(1, Ordering::Relaxed)).collect::<Vec<_>>())
                .collect();
            std::hint::black_box(&r);
        }
        let raw_atomic_ns = t3.elapsed().as_nanos() as f64 / PERF_ITERS as f64;
        println!(
            "[2b] raw single shared AtomicU32 fetch_add: {:.1} µs (isolates contention; block is {:.2}x)",
            raw_atomic_ns / 1000.0,
            raw_atomic_ns / block_fresh_ns
        );
    }
}
