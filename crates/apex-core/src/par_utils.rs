use crate::world::{adaptive_chunk_size, ChunkConfig};
use smallvec::SmallVec;

/// Вычислить список чанков `(arch_idx, start, end)` для параллельной итерации.
///
/// Принимает итератор пар `(arch_idx, len)` — индекс архетипа и количество entity в нём.
/// Возвращает `SmallVec<[(usize, usize, usize); 64]>` где каждый элемент — `(arch_idx, start, end)`.
/// Использует SmallVec с inline-буфером на 64 элемента, чтобы избежать heap-аллокации
/// для типичных сценариев (20–30 архетипов, 1–4 чанка на архетип).
///
/// Использует [`adaptive_chunk_size`] для вычисления оптимального размера чанка.
/// Эта функция является общей для `Query::par_for_each*` и `CachedQuery::par_for_each*`.
pub(crate) fn compute_par_chunks<I>(
    archetype_lens: I,
    num_threads: usize,
    config: &ChunkConfig,
) -> SmallVec<[(usize, usize, usize); 64]>
where
    I: IntoIterator<Item = (usize, usize)>,
{
    archetype_lens
        .into_iter()
        .flat_map(|(arch_idx, len)| {
            let chunk_size = adaptive_chunk_size(len, num_threads, config);
            let num_chunks = len.div_ceil(chunk_size);
            (0..num_chunks).map(move |chunk_i| {
                let start = chunk_i * chunk_size;
                let end = (start + chunk_size).min(len);
                (arch_idx, start, end)
            })
        })
        .collect()
}

/// Recursive divide-and-conquer parallel runner — restores the legion-style
/// *adaptive split* `par_for_each` (wave 5, §7). Unlike [`compute_par_chunks`]
/// (which chunks each archetype INDEPENDENTLY, precomputes a chunk list, and
/// drives it through `par_iter`), this splits the COMBINED row space across
/// archetypes: the recursion halves the total remaining rows each step via
/// `rayon::join`, so work load-balances regardless of per-archetype size and the
/// binary `join` tree work-steals more efficiently than a `par_iter` over a
/// collection. Measured (wave-5 A/B, `par_split_ab`/`par_uniform_ab`): ~7%
/// faster on a uniform heavy workload, ~2-3% on a skewed one, never slower.
///
/// `items` is a list of disjoint `(arch_idx, start, end)` row ranges; `threshold`
/// is the leaf size (pass `adaptive_chunk_size`). `process(arch_idx, start, end)`
/// runs one leaf range — the caller supplies the query-specific fetch/iterate
/// body, so this stays agnostic to `Query` vs `CachedQuery` fetch shapes.
///
/// # Safety contract (upheld by callers)
/// Leaves cover pairwise-disjoint row ranges, so any `&mut` component access
/// inside `process` never aliases across the parallel `join` (same contract as
/// the former fixed-chunk path). `process` itself may run `unsafe` fetches; it
/// must only touch the `(arch, [start,end))` rows handed to it.
pub(crate) fn par_split_run_ranges<P>(
    items: &[(usize, usize, usize)],
    threshold: usize,
    process: &P,
) where
    P: Fn(usize, usize, usize) + Sync,
{
    let total: usize = items.iter().map(|&(_, s, e)| e - s).sum();
    if total == 0 {
        return;
    }
    if total <= threshold {
        for &(a, s, e) in items {
            process(a, s, e);
        }
        return;
    }
    // Halve the combined row space; the range straddling the midpoint is cut.
    let half = total / 2;
    let mut left: SmallVec<[(usize, usize, usize); 16]> = SmallVec::new();
    let mut right: SmallVec<[(usize, usize, usize); 16]> = SmallVec::new();
    let mut acc = 0usize;
    for &(a, s, e) in items {
        let len = e - s;
        if acc >= half {
            right.push((a, s, e));
        } else if acc + len <= half {
            left.push((a, s, e));
            acc += len;
        } else {
            let take = half - acc;
            left.push((a, s, s + take));
            right.push((a, s + take, e));
            acc = half;
        }
    }
    rayon::join(
        || par_split_run_ranges(&left, threshold, process),
        || par_split_run_ranges(&right, threshold, process),
    );
}
