use crate::{
    archetype::Archetype,
    component::{Component, ComponentId},
    entity::Entity,
    world::World,
};

// ── WorldQuery ─────────────────────────────────────────────────
//
// Единый trait для всего что можно запросить.
// Компилятор полностью инлайнит fetch_item — никаких виртуальных вызовов.

pub trait WorldQuery: Sized {
    type Item<'w>;
    type State: Copy;

    /// Сколько ComponentId этот запрос занимает в плоском массиве
    fn component_count() -> usize;
    /// Заполнить ids начиная с offset
    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>);
    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool;

    /// Safety: архетип содержит все нужные компоненты, ids корректны
    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId]) -> Self::State;

    /// Safety: row < arch.len()
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Self::Item<'w>;
}

// ── Read<T> ────────────────────────────────────────────────────

pub struct Read<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for Read<T> {
    type Item<'w> = &'w T;
    type State = *const T;

    #[inline] fn component_count() -> usize { 1 }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId]) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        arch.columns[col_idx].data as *const T
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        &*state.add(row)
    }
}

// ── Write<T> ───────────────────────────────────────────────────

pub struct Write<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for Write<T> {
    type Item<'w> = &'w mut T;
    type State = *mut T;

    #[inline] fn component_count() -> usize { 1 }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() { ids.push(id); }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId]) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        arch.columns[col_idx].data as *mut T
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
        &mut *state.add(row)
    }
}

// ── Tuple impls ────────────────────────────────────────────────

macro_rules! impl_world_query_tuple {
    ( $( ($Q:ident, $idx:tt) ),+ ) => {
        impl< $($Q: WorldQuery),+ > WorldQuery for ( $($Q,)+ ) {
            type Item<'w> = ( $($Q::Item<'w>,)+ );
            type State    = ( $($Q::State,)+ );

            #[inline]
            fn component_count() -> usize {
                0 $( + $Q::component_count() )+
            }

            fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
                $( $Q::fill_ids(world, ids); )+
            }

            fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
                let mut offset = 0;
                #[allow(unused_assignments)]
                $(
                    let n = $Q::component_count();
                    if !$Q::matches_archetype(arch, &ids[offset..offset + n]) { return false; }
                    offset += n;
                )+
                let _ = offset;
                true
            }

            unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId]) -> Self::State {
                let mut offset = 0;
                #[allow(unused_assignments)]
                ($(
                    {
                        let n = $Q::component_count();
                        let s = $Q::fetch_state(arch, &ids[offset..offset + n]);
                        offset += n;
                        s
                    },
                )+)
            }

            #[inline(always)]
            unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Self::Item<'w> {
                ( $( $Q::fetch_item(state.$idx, row), )+ )
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

// ── Query<Q> ──────────────────────────────────────────────────

/// Zero-cost typed query. Итерация компилируется в прямой доступ к указателям.
///
/// ```ignore
/// for (entity, (pos, vel)) in Query::<(Read<Position>, Write<Velocity>)>::new(&world).iter() {
///     vel.x += pos.x;
/// }
/// ```
pub struct Query<'w, Q: WorldQuery> {
    world: &'w World,
    archetypes: Vec<ArchState<Q::State>>,
}

struct ArchState<S> {
    arch_idx: usize,
    state: S,
    len: usize,
}

impl<'w, Q: WorldQuery> Query<'w, Q> {
    pub fn new(world: &'w World) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        // ids могут быть не найдены если компонент не зарегистрирован
        let all_found = ids.len() == Q::component_count();

        let archetypes = if all_found {
            world.archetypes
                .iter()
                .enumerate()
                .filter(|(_, arch)| !arch.is_empty() && Q::matches_archetype(arch, &ids))
                .map(|(arch_idx, arch)| {
                    let state = unsafe { Q::fetch_state(arch, &ids) };
                    ArchState { arch_idx, state, len: arch.len() }
                })
                .collect()
        } else {
            Vec::new()
        };

        Self { world, archetypes }
    }

    /// Итерация с entity
    #[inline]
    pub fn iter(&self) -> QueryIter<'_, Q> {
        QueryIter {
            archetypes: &self.archetypes,
            world: self.world,
            arch_cursor: 0,
            row_cursor: 0,
        }
    }

    /// Итерация без entity — чуть быстрее
    #[inline]
    pub fn iter_components(&self) -> QueryComponentIter<'_, Q> {
        QueryComponentIter {
            archetypes: &self.archetypes,
            arch_cursor: 0,
            row_cursor: 0,
        }
    }

    /// for_each — лучший вариант для горячих путей, компилятор инлайнит замыкание
    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        for a in &self.archetypes {
            let entities = &self.world.archetypes[a.arch_idx].entities;
            for row in 0..a.len {
                let item = unsafe { Q::fetch_item(a.state, row) };
                f(entities[row], item);
            }
        }
    }

    /// for_each без entity
    #[inline]
    pub fn for_each_component<F: FnMut(Q::Item<'_>)>(&self, mut f: F) {
        for a in &self.archetypes {
            for row in 0..a.len {
                f(unsafe { Q::fetch_item(a.state, row) });
            }
        }
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
    archetypes: &'q [ArchState<Q::State>],
    world: &'q World,
    arch_cursor: usize,
    row_cursor: usize,
}

impl<'q, Q: WorldQuery> Iterator for QueryIter<'q, Q> {
    type Item = (Entity, Q::Item<'q>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let a = self.archetypes.get(self.arch_cursor)?;
            if self.row_cursor >= a.len {
                self.arch_cursor += 1;
                self.row_cursor = 0;
                continue;
            }
            let row = self.row_cursor;
            self.row_cursor += 1;
            let entity = self.world.archetypes[a.arch_idx].entities[row];
            let item = unsafe { Q::fetch_item(a.state, row) };
            return Some((entity, item));
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n: usize = self.archetypes.get(self.arch_cursor..)
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .map(|(i, a)| if i == 0 { a.len.saturating_sub(self.row_cursor) } else { a.len })
            .sum();
        (n, Some(n))
    }
}

pub struct QueryComponentIter<'q, Q: WorldQuery> {
    archetypes: &'q [ArchState<Q::State>],
    arch_cursor: usize,
    row_cursor: usize,
}

impl<'q, Q: WorldQuery> Iterator for QueryComponentIter<'q, Q> {
    type Item = Q::Item<'q>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let a = self.archetypes.get(self.arch_cursor)?;
            if self.row_cursor >= a.len {
                self.arch_cursor += 1;
                self.row_cursor = 0;
                continue;
            }
            let row = self.row_cursor;
            self.row_cursor += 1;
            return Some(unsafe { Q::fetch_item(a.state, row) });
        }
    }
}

// ── QueryBuilder (совместимость) ───────────────────────────────

pub struct QueryBuilder<'w> {
    world: &'w World,
    reads: Vec<ComponentId>,
    writes: Vec<ComponentId>,
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
    fn matches_arch(&self, arch: &crate::archetype::Archetype) -> bool {
        self.reads.iter().all(|id| arch.has_component(*id))
            && self.writes.iter().all(|id| arch.has_component(*id))
            && self.excludes.iter().all(|id| !arch.has_component(*id))
    }

    pub fn iter_one<T: Component>(&'w self) -> Box<dyn Iterator<Item = (Entity, &'w T)> + 'w> {
        let comp_id = match self.world.registry.get_id::<T>() {
            Some(id) => id,
            None => return Box::new(std::iter::empty()),
        };
        let arch_indices: Vec<usize> = self.world.archetypes.iter().enumerate()
            .filter(|(_, arch)| arch.has_component(comp_id) && self.matches_arch(arch))
            .map(|(i, _)| i)
            .collect();
        Box::new(LegacyIter {
            world: self.world,
            arch_indices,
            comp_id,
            arch_cursor: 0,
            row_cursor: 0,
            _phantom: std::marker::PhantomData,
        })
    }
}

struct LegacyIter<'w, T> {
    world: &'w World,
    arch_indices: Vec<usize>,
    comp_id: ComponentId,
    arch_cursor: usize,
    row_cursor: usize,
    _phantom: std::marker::PhantomData<&'w T>,
}

impl<'w, T: Component> Iterator for LegacyIter<'w, T> {
    type Item = (Entity, &'w T);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let arch_idx = *self.arch_indices.get(self.arch_cursor)?;
            let arch = &self.world.archetypes[arch_idx];
            if self.row_cursor >= arch.len() {
                self.arch_cursor += 1;
                self.row_cursor = 0;
                continue;
            }
            let entity = arch.entities[self.row_cursor];
            let row = self.row_cursor;
            self.row_cursor += 1;
            let component = unsafe { arch.get_component::<T>(row, self.comp_id)? };
            return Some((entity, component));
        }
    }
}
