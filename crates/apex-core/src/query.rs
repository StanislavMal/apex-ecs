use crate::{
    access::{AccessDescriptor, ArchetypeMask},
    archetype::Archetype,
    component::{Component, ComponentId, Tick},
    entity::Entity,
    system_param::WorldQuerySystemAccess,
    world::World,
};

#[cfg(feature = "parallel")]
use crate::par_utils::compute_par_chunks;

// ── WorldQuery ─────────────────────────────────────────────────

pub trait WorldQuery: Sized {
    type Item<'w>;
    type State: Copy;

    fn component_count() -> usize;
    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>);

    /// Заполняет только "positive" (не-Without) component IDs.
    /// По умолчанию — то же что fill_ids.
    fn fill_positive_ids(world: &World, ids: &mut Vec<ComponentId>) {
        Self::fill_ids(world, ids);
    }
    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool;

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick) -> Self::State;
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>>;

    fn is_filter() -> bool { false }
    /// Возвращает true для компонентов, которые ДОЛЖНЫ присутствовать.
    /// Для Without<T> возвращает false.
    fn is_positive() -> bool { true }
}

// ── Read<T> ────────────────────────────────────────────────────

pub struct Read<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for Read<T> {
    type Item<'w> = &'w T;
    type State    = *const T;

    #[inline] fn component_count() -> usize { 1 }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], _: Tick) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        arch.columns[col_idx].get_ptr(0) as *const T
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        Some(&*state.add(row))
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Read<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── Write<T> ───────────────────────────────────────────────────

pub struct Write<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for Write<T> {
    type Item<'w> = &'w mut T;
    type State    = *mut T;

    #[inline] fn component_count() -> usize { 1 }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], _: Tick) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        arch.columns[col_idx].get_ptr(0) as *mut T
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        Some(&mut *state.add(row))
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Write<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().write::<T>()
    }
}

// ── With<T> ────────────────────────────────────────────────────

pub struct With<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for With<T> {
    type Item<'w> = ();
    type State    = ();

    #[inline] fn component_count() -> usize { 1 }
    #[inline] fn is_filter() -> bool { true }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(_: &Archetype, _: &[ComponentId], _: Tick) -> Self::State {}

    #[inline(always)]
    unsafe fn fetch_item<'w>(_: Self::State, _: usize) -> Option<Self::Item<'w>> { Some(()) }
}

impl<T: Component + 'static> WorldQuerySystemAccess for With<T> {
    fn system_access() -> AccessDescriptor {
        // With<T> только проверяет наличие — read semantics
        AccessDescriptor::new().read::<T>()
    }
}

// ── Without<T> ─────────────────────────────────────────────────

pub struct Without<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for Without<T> {
    type Item<'w> = ();
    type State    = ();

    #[inline] fn component_count() -> usize { 1 }
    #[inline] fn is_filter() -> bool { true }
    #[inline] fn is_positive() -> bool { false }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

    fn fill_positive_ids(_: &World, _: &mut Vec<ComponentId>) {}

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        ids.is_empty() || !arch.has_component(ids[0])
    }

    unsafe fn fetch_state(_: &Archetype, _: &[ComponentId], _: Tick) -> Self::State {}

    #[inline(always)]
    unsafe fn fetch_item<'w>(_: Self::State, _: usize) -> Option<Self::Item<'w>> { Some(()) }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Without<T> {
    fn system_access() -> AccessDescriptor {
        // Without не читает данные T — нет доступа к T вообще
        AccessDescriptor::new()
    }
}

// ── Changed<T> ─────────────────────────────────────────────────

pub struct Changed<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct ChangedState {
    data:      *const u8,
    ticks:     *const Tick,
    last_run:  Tick,
    item_size: usize,
}

unsafe impl Send for ChangedState {}
unsafe impl Sync for ChangedState {}

impl<T: Component> WorldQuery for Changed<T> {
    type Item<'w> = &'w T;
    type State    = ChangedState;

    #[inline] fn component_count() -> usize { 1 }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        ChangedState { data: col.data, ticks: col.ticks_ptr(), last_run, item_size: col.item_size }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        let tick = *state.ticks.add(row);
        if tick.is_newer_than(state.last_run) {
            Some(&*(state.data.add(row * state.item_size) as *const T))
        } else {
            None
        }
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Changed<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── Tuple impls ────────────────────────────────────────────────

macro_rules! impl_world_query_tuple {
    ( $( ($Q:ident, $idx:tt) ),+ ) => {
        impl< $($Q: WorldQuery),+ > WorldQuery for ( $($Q,)+ ) {
            type Item<'w> = ( $($Q::Item<'w>,)+ );
            type State    = ( $($Q::State,)+ );

            #[inline]
            fn component_count() -> usize { 0 $( + $Q::component_count() )+ }

            fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
                $( $Q::fill_ids(world, ids); )+
            }

            fn fill_positive_ids(world: &World, ids: &mut Vec<ComponentId>) {
                $( $Q::fill_positive_ids(world, ids); )+
            }

            fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
                let mut offset = 0;
                $(
                    let n = $Q::component_count();
                    if !$Q::matches_archetype(arch, &ids[offset..offset + n]) { return false; }
                    #[allow(unused_assignments)] { offset += n; }
                )+
                true
            }

            unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick) -> Self::State {
                let mut offset = 0;
                ($(
                    {
                        let n = $Q::component_count();
                        let s = $Q::fetch_state(arch, &ids[offset..offset + n], last_run);
                        #[allow(unused_assignments)] { offset += n; }
                        s
                    },
                )+)
            }

            #[inline(always)]
            unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
                Some(( $( $Q::fetch_item(state.$idx, row)?, )+ ))
            }
        }

        // WorldQuerySystemAccess для кортежей
        impl< $($Q: WorldQuery + WorldQuerySystemAccess + 'static),+ >
            WorldQuerySystemAccess for ( $($Q,)+ )
        {
            fn system_access() -> AccessDescriptor {
                AccessDescriptor::new()
                    $( .merge(&$Q::system_access()) )+
            }
        }
    };
}

impl_world_query_tuple!((A, 0), (B, 1));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7));

// ── ArchState ──────────────────────────────────────────────────

pub(crate) struct ArchState<S> {
    pub arch_idx: usize,
    pub state:    S,
    pub len:      usize,
}

// ── Query<Q> ───────────────────────────────────────────────────

pub struct Query<'w, Q: WorldQuery> {
    world:      &'w World,
    archetypes: Vec<ArchState<Q::State>>,
    last_run:   Tick,
}

impl<'w, Q: WorldQuery> Query<'w, Q> {
    pub fn new(world: &'w World) -> Self {
        Self::new_with_tick(world, Tick::ZERO)
    }

    pub fn new_with_tick(world: &'w World, last_run: Tick) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        // Собираем positive IDs (не-Without) для candidate selection
        let mut positive_ids = Vec::with_capacity(Q::component_count());
        Q::fill_positive_ids(world, &mut positive_ids);

        // Строим exclude_mask из negative (Without) компонентов
        let exclude_mask = {
            let mut mask = ArchetypeMask::EMPTY;
            for &id in ids.iter().filter(|id| !positive_ids.contains(id)) {
                if let Some(arch_ids) = world.component_arch_index.get(&id) {
                    for arch_id in arch_ids {
                        mask.set(arch_id.0 as usize);
                    }
                }
            }
            mask
        };

        // Predicate для фильтрации архетипов: проверка exclude_mask + matches_archetype
        let arch_filter = |arch_idx: usize| -> bool {
            if exclude_mask.get(arch_idx) {
                return false;
            }
            let arch = &world.archetypes[arch_idx];
            !arch.is_empty() && Q::matches_archetype(arch, &ids)
        };

        let archetypes = if ids.len() == Q::component_count() {
            // Линейный обход архетипов — быстрее для малых запросов (≤3 компонента)
            // и малых миров (≤128 архетипов). ComponentArchIndex даёт выигрыш только
            // для больших миров (500+ архетипов) и запросов с 4+ компонентами.
            if !positive_ids.is_empty() && (positive_ids.len() <= 3 || world.archetypes.len() <= 128) {
                // Линейный обход: O(N) по числу архетипов, без HashMap lookup'ов
                world.archetypes.iter().enumerate()
                    .filter(|&(arch_idx, _arch)| arch_filter(arch_idx))
                    .map(|(arch_idx, arch)| {
                        let state = unsafe { Q::fetch_state(arch, &ids, last_run) };
                        ArchState { arch_idx, state, len: arch.len() }
                    })
                    .collect()
            } else if positive_ids.is_empty() && !ids.is_empty() {
                // Только Without-компоненты (нет positive) — используем exclude_mask
                // для отсеивания архетипов, которые содержат excluding-компонент
                world.archetypes.iter().enumerate()
                    .filter(|&(arch_idx, _arch)| arch_filter(arch_idx))
                    .map(|(arch_idx, arch)| {
                        let state = unsafe { Q::fetch_state(arch, &ids, last_run) };
                        ArchState { arch_idx, state, len: arch.len() }
                    })
                    .collect()
            } else if positive_ids.is_empty() {
                // Запрос без компонентов — все архетипы
                world.archetypes.iter().enumerate()
                    .filter(|(_, arch)| !arch.is_empty())
                    .map(|(arch_idx, arch)| {
                        let state = unsafe { Q::fetch_state(arch, &ids, last_run) };
                        ArchState { arch_idx, state, len: arch.len() }
                    })
                    .collect()
            } else {
                // component_arch_index — O(K) поиск кандидатов через наименее
                // распространённый компонент. Для больших миров (500+ архетипов)
                // и сложных запросов (4+ компонентов) это K << N.
                let candidate_archetypes = {
                    let smallest = positive_ids.iter()
                        .filter_map(|id| world.component_arch_index.get(id))
                        .min_by_key(|v| v.len());

                    match smallest {
                        Some(arch_ids) => arch_ids.iter()
                            .map(|id| id.0 as usize)
                            .collect(),
                        None => {
                            (0..world.archetypes.len()).collect::<Vec<_>>()
                        }
                    }
                };

                candidate_archetypes.into_iter()
                    .filter(|&arch_idx| arch_filter(arch_idx))
                    .map(|arch_idx| {
                        let arch = &world.archetypes[arch_idx];
                        let state = unsafe { Q::fetch_state(arch, &ids, last_run) };
                        ArchState { arch_idx, state, len: arch.len() }
                    })
                    .collect()
            }
        } else {
            Vec::new()
        };

        Self { world, archetypes, last_run }
    }

    #[inline]
    pub fn iter(&self) -> QueryIter<'_, Q> {
        QueryIter {
            archetypes:   &self.archetypes,
            world:        self.world,
            arch_cursor:  0,
            row_cursor:   0,
        }
    }

    /// Consuming итератор — для использования в ParamQuery.
    pub(crate) fn into_iter_owned(self) -> QueryIterOwned<'w, Q> {
        QueryIterOwned { query: self, arch_cursor: 0, row_cursor: 0 }
    }

    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        for a in &self.archetypes {
            let entities = &self.world.archetypes[a.arch_idx].entities[..a.len];
            for (row, &entity) in entities.iter().enumerate() {
                if let Some(item) = unsafe { Q::fetch_item(a.state, row) } {
                    f(entity, item);
                }
            }
        }
    }

    /// Параллельная итерация.
    #[cfg(feature = "parallel")]
    pub fn par_for_each<F>(&self, f: F)
    where
        Q: Send,
        F: Fn(Entity, Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;

        let num_threads = rayon::current_num_threads();

        // Предварительно вычисляем ID компонентов (как в new_with_tick)
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(self.world, &mut ids);
        if ids.len() != Q::component_count() {
            return;
        }

        let chunks = compute_par_chunks(
            self.archetypes.iter().map(|a| (a.arch_idx, a.len)),
            num_threads,
        );

        let last_run = self.last_run;

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch = unsafe { &*self.world.archetypes.as_ptr().add(arch_idx) };
            let state = unsafe { Q::fetch_state(arch, &ids, last_run) };
            let entities = &arch.entities[start..end];
            for (row, &entity) in entities.iter().enumerate() {
                if let Some(item) = unsafe { Q::fetch_item(state, start + row) } {
                    f(entity, item);
                }
            }
        });
    }

    #[cfg(not(feature = "parallel"))]
    pub fn par_for_each<F>(&self, f: F)
    where
        Q: WorldQuery,
        F: FnMut(Entity, Q::Item<'_>),
    {
        self.for_each(f);
    }

    pub fn len(&self) -> usize {
        self.archetypes.iter().map(|a| a.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.archetypes.iter().all(|a| a.len == 0)
    }
}

// ── Итераторы ──────────────────────────────────────────────────

pub struct QueryIter<'q, Q: WorldQuery> {
    archetypes:  &'q [ArchState<Q::State>],
    world:       &'q World,
    arch_cursor: usize,
    row_cursor:  usize,
}

impl<'q, Q: WorldQuery> Iterator for QueryIter<'q, Q> {
    type Item = (Entity, Q::Item<'q>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let a = self.archetypes.get(self.arch_cursor)?;
            if self.row_cursor >= a.len {
                self.arch_cursor += 1;
                self.row_cursor  = 0;
                continue;
            }
            let row = self.row_cursor;
            self.row_cursor += 1;
            if let Some(item) = unsafe { Q::fetch_item(a.state, row) } {
                let entity = self.world.archetypes[a.arch_idx].entities[row];
                return Some((entity, item));
            }
        }
    }
}

pub struct QueryIterOwned<'w, Q: WorldQuery> {
    query:       Query<'w, Q>,
    arch_cursor: usize,
    row_cursor:  usize,
}

impl<'w, Q: WorldQuery> Iterator for QueryIterOwned<'w, Q> {
    type Item = (Entity, Q::Item<'w>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let a = self.query.archetypes.get(self.arch_cursor)?;
            if self.row_cursor >= a.len {
                self.arch_cursor += 1;
                self.row_cursor  = 0;
                continue;
            }
            let row = self.row_cursor;
            self.row_cursor += 1;
            if let Some(item) = unsafe { Q::fetch_item(a.state, row) } {
                let entity = self.query.world.archetypes[a.arch_idx].entities[row];
                return Some((entity, item));
            }
        }
    }
}

// ── QueryBuilder ───────────────────────────────────────────────

pub struct QueryBuilder<'w> {
    world:    &'w World,
    reads:    Vec<ComponentId>,
    writes:   Vec<ComponentId>,
    excludes: Vec<ComponentId>,
}

impl<'w> QueryBuilder<'w> {
    pub fn new(world: &'w World) -> Self {
        Self { world, reads: Vec::new(), writes: Vec::new(), excludes: Vec::new() }
    }

    pub fn read<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() { self.reads.push(id); }
        self
    }

    pub fn write<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() { self.writes.push(id); }
        self
    }

    pub fn exclude<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() { self.excludes.push(id); }
        self
    }

    pub fn matching_archetype_ids(&self) -> Vec<usize> {
        self.world.archetypes.iter().enumerate()
            .filter(|(_, arch)| self.matches_arch(arch))
            .map(|(i, _)| i)
            .collect()
    }

    #[inline]
    fn matches_arch(&self, arch: &Archetype) -> bool {
        self.reads.iter().all(|id| arch.has_component(*id))
            && self.writes.iter().all(|id| arch.has_component(*id))
            && self.excludes.iter().all(|id| !arch.has_component(*id))
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::World;

    struct A;
    struct B;

    #[test]
    fn without_exclude_mask_works() {
        let mut world = World::new();

        // Создаём сущность только с A
        let e1 = world.spawn((A,));
        // Создаём сущность с A и B
        let _e2 = world.spawn((A, B));
        // Создаём сущность только с B
        let e3 = world.spawn((B,));

        // Query<Read<A>, Without<B>> должен вернуть только e1
        let query: Query<'_, (Read<A>, Without<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _)| e).collect();
        assert_eq!(results, vec![e1], "Without<B> должен исключить сущности с B");

        // Query<Read<B>, Without<A>> должен вернуть только e3
        let query: Query<'_, (Read<B>, Without<A>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _)| e).collect();
        assert_eq!(results, vec![e3], "Without<A> должен исключить сущности с A");

        // Query<Without<A>, Without<B>> — пустой результат (все имеют хотя бы один компонент)
        let query: Query<'_, (Without<A>, Without<B>)> = Query::new(&world);
        assert!(query.is_empty(), "Без A и B ничего не должно остаться");
    }

    #[test]
    fn without_with_large_world() {
        let mut world = World::new();

        // Создаём много сущностей с (A) и много с (A, B)
        let mut only_a = Vec::new();
        let mut with_b = Vec::new();

        for _ in 0..50 {
            only_a.push(world.spawn((A,)));
        }
        for _ in 0..50 {
            with_b.push(world.spawn((A, B)));
        }

        // Query<Read<A>, Without<B>> — должны получить только entities с A без B
        let query: Query<'_, (Read<A>, Without<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _)| e).collect();

        assert_eq!(results.len(), 50, "Должно быть 50 сущностей с A без B");
        for e in &results {
            assert!(only_a.contains(e), "Сущность должна быть из only_a");
            assert!(!with_b.contains(e), "Сущность не должна быть из with_b");
        }
    }

    #[test]
    fn without_alone_query() {
        let mut world = World::new();

        let _e1 = world.spawn((A,));
        let e2 = world.spawn((B,));
        let _e3 = world.spawn((A, B));

        // Чистый Without<A> — все сущности без A
        let query: Query<'_, Without<A>> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _)| e).collect();
        assert_eq!(results, vec![e2], "Without<A> должен вернуть сущности без A");
    }
}