use crate::{
    access::AccessDescriptor,
    archetype::Archetype,
    component::{Component, ComponentId, Tick},
    entity::Entity,
    system_param::WorldQuerySystemAccess,
    world::{adaptive_chunk_size, World},
};

// ── WorldQuery ─────────────────────────────────────────────────

pub trait WorldQuery: Sized {
    type Item<'w>;
    type State: Copy;

    fn component_count() -> usize;
    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>);
    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool;

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick) -> Self::State;
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>>;

    fn is_filter() -> bool { false }
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
        arch.columns[col_idx].data as *const T
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
        arch.columns[col_idx].data as *mut T
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

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

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

        let archetypes = if ids.len() == Q::component_count() {
            world.archetypes
                .iter()
                .enumerate()
                .filter(|(_, arch)| !arch.is_empty() && Q::matches_archetype(arch, &ids))
                .map(|(arch_idx, arch)| {
                    let state = unsafe { Q::fetch_state(arch, &ids, last_run) };
                    ArchState { arch_idx, state, len: arch.len() }
                })
                .collect()
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
    pub fn iter_components(&self) -> QueryComponentIter<'_, Q> {
        QueryComponentIter { archetypes: &self.archetypes, arch_cursor: 0, row_cursor: 0 }
    }

    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        for a in &self.archetypes {
            let entities = &self.world.archetypes[a.arch_idx].entities;
            for row in 0..a.len {
                if let Some(item) = unsafe { Q::fetch_item(a.state, row) } {
                    f(entities[row], item);
                }
            }
        }
    }

    #[inline]
    pub fn for_each_component<F: FnMut(Q::Item<'_>)>(&self, mut f: F) {
        for a in &self.archetypes {
            for row in 0..a.len {
                if let Some(item) = unsafe { Q::fetch_item(a.state, row) } {
                    f(item);
                }
            }
        }
    }

    /// Параллельная итерация по компонентам (chunk-level parallelism).
    /// Автоматически использует `adaptive_chunk_size` для каждого архетипа.
    #[cfg(feature = "parallel")]
    pub fn par_for_each_component<F>(&self, f: F)
    where
        Q: Send,
        F: Fn(Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;

        let num_threads = rayon::current_num_threads();

        // Предварительно вычисляем ID компонентов (как в new_with_tick)
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(self.world, &mut ids);
        if ids.len() != Q::component_count() {
            return;
        }

        let chunks: Vec<(usize, usize, usize)> = self
            .archetypes
            .iter()
            .flat_map(|a| {
                let chunk_size = adaptive_chunk_size(a.len, num_threads);
                (0..(a.len + chunk_size - 1) / chunk_size).map(move |chunk_i| {
                    let start = chunk_i * chunk_size;
                    let end = (start + chunk_size).min(a.len);
                    (a.arch_idx, start, end)
                })
            })
            .collect();

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch = unsafe { &*self.world.archetypes.as_ptr().add(arch_idx) };
            let state = unsafe { Q::fetch_state(arch, &ids, Tick::ZERO) };
            for row in start..end {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(item);
                }
            }
        });
    }

    #[cfg(not(feature = "parallel"))]
    pub fn par_for_each_component<F>(&self, f: F)
    where
        Q: WorldQuery,
        F: FnMut(Q::Item<'_>),
    {
        self.for_each_component(f);
    }

    /// Параллельная итерация с Entity.
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

        let chunks: Vec<(usize, usize, usize)> = self
            .archetypes
            .iter()
            .flat_map(|a| {
                let chunk_size = adaptive_chunk_size(a.len, num_threads);
                (0..(a.len + chunk_size - 1) / chunk_size).map(move |chunk_i| {
                    let start = chunk_i * chunk_size;
                    let end = (start + chunk_size).min(a.len);
                    (a.arch_idx, start, end)
                })
            })
            .collect();

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch = unsafe { &*self.world.archetypes.as_ptr().add(arch_idx) };
            let state = unsafe { Q::fetch_state(arch, &ids, Tick::ZERO) };
            let entities = &arch.entities;
            for row in start..end {
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entities[row], item);
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

pub struct QueryComponentIter<'q, Q: WorldQuery> {
    archetypes:  &'q [ArchState<Q::State>],
    arch_cursor: usize,
    row_cursor:  usize,
}

impl<'q, Q: WorldQuery> Iterator for QueryComponentIter<'q, Q> {
    type Item = Q::Item<'q>;

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
                return Some(item);
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