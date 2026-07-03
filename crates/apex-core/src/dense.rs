//! Плотная (chunk) итерация уровня Legion (W2-0.5).
//!
//! Хранение и так archetype-SoA — этот модуль лишь выдаёт колонки СЛАЙСАМИ,
//! убирая per-row `fetch_item`/`Mut`-обвязку с горячего пути. Доступно только
//! НЕ-фильтрующим формам: `Read`/`Write`/`&T`/`&mut T`/`Maybe`/`MaybeWrite`/
//! `With`/`Without` и кортежам из них. `Changed<T>` и `Added<T>` НЕ реализуют
//! [`DenseQuery`] — построчные фильтры несовместимы со слайсовой выдачей,
//! такой запрос не компилируется (используйте `for_each`).
//!
//! Контракт change-detection: write-слайс стампит change-tick ВСЕМУ
//! выданному диапазону в момент выдачи (`Column::stamp_range`,
//! векторизуемый fill) — «взял слайс на запись = весь диапазон changed».
//! Обычный `for_each` с точечным стампингом через `Mut<T>` не тронут.
//!
//! Потребители: `Query::for_each_chunk` / `par_for_each_chunk`,
//! `CachedQuery::for_each_chunk` / `par_for_each_chunk`.

use crate::{
    archetype::Archetype,
    component::{ComponentId, Tick},
    query::{Maybe, MaybeWrite, Read, With, Without, WorldQuery, Write},
};
use crate::component::Component;

/// Слайсовая выдача формы запроса для одного архетипа.
///
/// # Safety-контракт `fetch_slices`
/// - `arch` матчит форму (`matches_archetype` == true);
/// - `ids` — сегмент, выровненный по `component_count` (как в `fetch_state`);
/// - `start + len <= arch.len()`;
/// - эксклюзивность write-доступа гарантирует вызывающий (планировщик /
///   единоличный `&mut World`).
pub trait DenseQuery: WorldQuery {
    /// Что получает колбэк для одного chunk'а: `&[T]` / `&mut [T]` /
    /// `Option<&[T]>` / `()` / кортежи из них.
    type Slices<'w>;

    /// Выдать слайсы колонок диапазона `[start, start+len)` архетипа.
    /// Write-формы стампят change-tick диапазону ЗДЕСЬ.
    ///
    /// # Safety
    /// `arch` must have matched this query and `ids` be its component-id list;
    /// `start + len <= arch` row count. For write-forms the `[start, start+len)`
    /// range must be accessed exclusively (disjoint across parallel chunks).
    unsafe fn fetch_slices<'w>(
        arch: &'w Archetype,
        ids: &[ComponentId],
        start: usize,
        len: usize,
        this_run: Tick,
    ) -> Self::Slices<'w>;
}

// ── Read<T> / &T ───────────────────────────────────────────────

impl<T: Component> DenseQuery for Read<T> {
    type Slices<'w> = &'w [T];

    #[inline]
    unsafe fn fetch_slices<'w>(
        arch: &'w Archetype,
        ids: &[ComponentId],
        start: usize,
        len: usize,
        _: Tick,
    ) -> &'w [T] {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        // get_ptr корректен и для ZST (выровненный ненулевой указатель).
        let ptr = arch.columns[col_idx].get_ptr(start) as *const T;
        std::slice::from_raw_parts(ptr, len)
    }
}

impl<T: Component> DenseQuery for &T {
    type Slices<'w> = &'w [T];

    #[inline]
    unsafe fn fetch_slices<'w>(
        arch: &'w Archetype,
        ids: &[ComponentId],
        start: usize,
        len: usize,
        this_run: Tick,
    ) -> &'w [T] {
        <Read<T> as DenseQuery>::fetch_slices(arch, ids, start, len, this_run)
    }
}

// ── Write<T> / &mut T ──────────────────────────────────────────

impl<T: Component> DenseQuery for Write<T> {
    type Slices<'w> = &'w mut [T];

    #[inline]
    unsafe fn fetch_slices<'w>(
        arch: &'w Archetype,
        ids: &[ComponentId],
        start: usize,
        len: usize,
        this_run: Tick,
    ) -> &'w mut [T] {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        // Контракт dense-write: весь выданный диапазон считается изменённым.
        col.stamp_range(start, start + len, this_run);
        let ptr = col.get_ptr(start) as *mut T;
        std::slice::from_raw_parts_mut(ptr, len)
    }
}

impl<T: Component> DenseQuery for &mut T {
    type Slices<'w> = &'w mut [T];

    #[inline]
    unsafe fn fetch_slices<'w>(
        arch: &'w Archetype,
        ids: &[ComponentId],
        start: usize,
        len: usize,
        this_run: Tick,
    ) -> &'w mut [T] {
        <Write<T> as DenseQuery>::fetch_slices(arch, ids, start, len, this_run)
    }
}

// ── With / Without — фильтры уровня архетипа, слайсов не дают ──

impl<T: Component> DenseQuery for With<T> {
    type Slices<'w> = ();

    // Лайфтайм обязан совпадать с декларацией трейта (E0195), хоть и не нужен.
    #[allow(clippy::needless_lifetimes)]
    #[inline]
    unsafe fn fetch_slices<'w>(_: &'w Archetype, _: &[ComponentId], _: usize, _: usize, _: Tick) {}
}

impl<T: Component> DenseQuery for Without<T> {
    type Slices<'w> = ();

    #[allow(clippy::needless_lifetimes)]
    #[inline]
    unsafe fn fetch_slices<'w>(_: &'w Archetype, _: &[ComponentId], _: usize, _: usize, _: Tick) {}
}

// ── Maybe / MaybeWrite — Option-слайсы ─────────────────────────

impl<T: Component> DenseQuery for Maybe<T> {
    type Slices<'w> = Option<&'w [T]>;

    #[inline]
    unsafe fn fetch_slices<'w>(
        arch: &'w Archetype,
        ids: &[ComponentId],
        start: usize,
        len: usize,
        _: Tick,
    ) -> Option<&'w [T]> {
        if ids.is_empty() || !arch.has_component(ids[0]) {
            return None;
        }
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let ptr = arch.columns[col_idx].get_ptr(start) as *const T;
        Some(std::slice::from_raw_parts(ptr, len))
    }
}

impl<T: Component> DenseQuery for MaybeWrite<T> {
    type Slices<'w> = Option<&'w mut [T]>;

    #[inline]
    unsafe fn fetch_slices<'w>(
        arch: &'w Archetype,
        ids: &[ComponentId],
        start: usize,
        len: usize,
        this_run: Tick,
    ) -> Option<&'w mut [T]> {
        if ids.is_empty() || !arch.has_component(ids[0]) {
            return None;
        }
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        col.stamp_range(start, start + len, this_run);
        let ptr = col.get_ptr(start) as *mut T;
        Some(std::slice::from_raw_parts_mut(ptr, len))
    }
}

// ── Кортежи ────────────────────────────────────────────────────

macro_rules! impl_dense_query_tuple {
    ( $( $Q:ident ),+ ) => {
        impl< $($Q: DenseQuery),+ > DenseQuery for ( $($Q,)+ ) {
            type Slices<'w> = ( $($Q::Slices<'w>,)+ );

            #[inline]
            unsafe fn fetch_slices<'w>(
                arch: &'w Archetype,
                ids: &[ComponentId],
                start: usize,
                len: usize,
                this_run: Tick,
            ) -> Self::Slices<'w> {
                let mut offset = 0;
                ($(
                    {
                        let n = $Q::component_count();
                        let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                        let s = $Q::fetch_slices(arch, slice, start, len, this_run);
                        #[allow(unused_assignments)] { offset += n; }
                        s
                    },
                )+)
            }
        }
    };
}

impl_dense_query_tuple!(A);
impl_dense_query_tuple!(A, B);
impl_dense_query_tuple!(A, B, C);
impl_dense_query_tuple!(A, B, C, D);
impl_dense_query_tuple!(A, B, C, D, E);
impl_dense_query_tuple!(A, B, C, D, E, F);
impl_dense_query_tuple!(A, B, C, D, E, F, G);
impl_dense_query_tuple!(A, B, C, D, E, F, G, H);

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::component::Component;
    use crate::query::{Changed, Maybe, Query, Read, Write};
    use crate::world::World;

    #[derive(Debug, PartialEq)]
    struct P(f32);
    impl Component for P {}
    #[derive(Debug, PartialEq)]
    struct V(f32);
    impl Component for V {}
    struct Tag;
    impl Component for Tag {}

    #[test]
    fn dense_chunk_iterates_all_rows() {
        let mut world = World::new();
        for i in 0..100 {
            world.spawn((P(i as f32), V(1.0)));
        }
        for i in 0..50 {
            world.spawn((P(i as f32), V(2.0), Tag));
        }

        let mut total = 0usize;
        Query::<(Read<V>, Write<P>)>::new_mut(&mut world).for_each_chunk(|entities, (v, p)| {
            assert_eq!(entities.len(), v.len());
            assert_eq!(v.len(), p.len());
            for i in 0..p.len() {
                p[i].0 += v[i].0;
            }
            total += p.len();
        });
        assert_eq!(total, 150);

        // Проверяем фактическую запись: P увеличился на V.
        let mut sum = 0.0;
        Query::<Read<P>>::new(&world).for_each(|_, p| sum += p.0);
        let expected: f32 = (0..100).map(|i| i as f32 + 1.0).sum::<f32>()
            + (0..50).map(|i| i as f32 + 2.0).sum::<f32>();
        assert_eq!(sum, expected);
    }

    #[test]
    fn dense_write_stamps_whole_range_changed() {
        let mut world = World::new();
        for _ in 0..10 {
            world.spawn((P(0.0),));
        }
        world.tick();
        let last_run = world.current_tick();
        world.tick();

        // Берём слайс на запись, но НИЧЕГО не пишем — по контракту dense-write
        // весь диапазон всё равно становится changed.
        Query::<Write<P>>::new_mut(&mut world).for_each_chunk(|_, _p| {});

        let changed = Query::<Changed<P>>::new_with_tick(&world, last_run)
            .iter()
            .count();
        assert_eq!(changed, 10, "dense-write стампит весь диапазон");
    }

    #[test]
    fn dense_maybe_slices_per_archetype() {
        let mut world = World::new();
        world.spawn((P(1.0), V(10.0)));
        world.spawn((P(2.0),));

        let mut seen = Vec::new();
        Query::<(Read<P>, Maybe<V>)>::new(&world).for_each_chunk(|_, (p, v)| {
            seen.push((p.len(), v.is_some()));
        });
        seen.sort();
        assert_eq!(seen, vec![(1, false), (1, true)]);
    }

    #[test]
    fn dense_par_chunk_matches_sequential() {
        let mut world = World::new();
        for i in 0..10_000 {
            world.spawn((P(i as f32), V(1.0)));
        }
        Query::<(Read<V>, Write<P>)>::new_mut(&mut world).par_for_each_chunk(|_, (v, p)| {
            for i in 0..p.len() {
                p[i].0 += v[i].0;
            }
        });
        let mut sum = 0.0f64;
        Query::<Read<P>>::new(&world).for_each(|_, p| sum += p.0 as f64);
        let expected: f64 = (0..10_000).map(|i| i as f64 + 1.0).sum();
        assert_eq!(sum, expected);
    }
}
