use crate::world::adaptive_chunk_size;

/// Вычислить список чанков `(arch_idx, start, end)` для параллельной итерации.
///
/// Принимает итератор пар `(arch_idx, len)` — индекс архетипа и количество entity в нём.
/// Возвращает `Vec<(usize, usize, usize)>` где каждый элемент — `(arch_idx, start, end)`.
///
/// Использует [`adaptive_chunk_size`] для вычисления оптимального размера чанка.
/// Эта функция является общей для `Query::par_for_each*` и `CachedQuery::par_for_each*`.
#[cfg(feature = "parallel")]
pub(crate) fn compute_par_chunks<I>(archetype_lens: I, num_threads: usize) -> Vec<(usize, usize, usize)>
where
    I: IntoIterator<Item = (usize, usize)>,
{
    archetype_lens
        .into_iter()
        .flat_map(|(arch_idx, len)| {
            let chunk_size = adaptive_chunk_size(len, num_threads);
            let num_chunks = (len + chunk_size - 1) / chunk_size;
            (0..num_chunks).map(move |chunk_i| {
                let start = chunk_i * chunk_size;
                let end = (start + chunk_size).min(len);
                (arch_idx, start, end)
            })
        })
        .collect()
}
