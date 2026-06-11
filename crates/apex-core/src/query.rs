use crate::par_utils::compute_par_chunks;
use crate::{
    access::AccessDescriptor,
    archetype::Archetype,
    component::{Component, ComponentId, Tick},
    entity::Entity,
    sub_world::SubWorld,
    system_param::WorldQuerySystemAccess,
    world::World,
};

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

    /// Заполняет только ОБЯЗАТЕЛЬНЫЕ component IDs — те, без которых архетип
    /// заведомо не матчится (Read/Write/Ref/With/Changed). В отличие от
    /// `fill_positive_ids` НЕ включает optional (`Maybe`/`MaybeWrite`).
    /// Используется `Query::new` для выбора архетипов-кандидатов из
    /// `component_arch_index` (кандидаты = архетипы самого редкого
    /// обязательного компонента).
    fn fill_required_ids(world: &World, ids: &mut Vec<ComponentId>) {
        Self::fill_positive_ids(world, ids);
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool;

    /// Захватывает per-archetype состояние для итерации.
    ///
    /// `last_run` — тик предыдущего запуска (для `Changed<T>`-фильтрации).
    /// `this_run` — текущий тик мира; `Write<T>`/`MaybeWrite<T>` стампят его в
    /// change-tick строки при `DerefMut` через возвращаемый `Mut<T>`.
    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        last_run: Tick,
        this_run: Tick,
    ) -> Self::State;
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>>;

    fn is_filter() -> bool {
        false
    }
    /// Возвращает true для компонентов, которые ДОЛЖНЫ присутствовать.
    /// Для Without<T> возвращает false.
    fn is_positive() -> bool {
        true
    }
    /// Возвращает false, если запрос может работать без всех ComponentId
    /// (например, Maybe<T> для незарегистрированного компонента).
    fn requires_all_ids() -> bool {
        true
    }
}

// ── Mut<T> — smart-pointer для Write<T> (change detection) ──────

/// Мутабельный доступ к компоненту с автоматическим change-detection.
///
/// Возвращается из `Query<Write<T>>`. На `DerefMut` стампит текущий тик мира в
/// change-tick строки → `Changed<T>` достоверно срабатывает на ВСЕХ путях
/// мутации (а не только `World::get_mut`). Семантика как у Bevy `Mut<T>`:
/// любое мутабельное заимствование помечает компонент изменённым, даже если
/// фактически только читали — это приемлемый и стандартный компромисс.
pub struct Mut<'w, T: 'static> {
    pub(crate) value: &'w mut T,
    /// Указатель на change-tick этой строки (внутри `Column::change_ticks`).
    pub(crate) change_tick: *mut Tick,
    /// Текущий тик мира — стампится при `DerefMut`.
    pub(crate) this_run: Tick,
}

impl<T: 'static> Mut<'_, T> {
    /// Явно пометить компонент изменённым без мутации значения.
    #[inline]
    pub fn set_changed(&mut self) {
        unsafe { *self.change_tick = self.this_run };
    }

    /// Получить `&mut T` без пометки изменения (escape-hatch).
    #[inline]
    pub fn bypass_change_detection(&mut self) -> &mut T {
        self.value
    }
}

impl<T: 'static> std::ops::Deref for Mut<'_, T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: 'static> std::ops::DerefMut for Mut<'_, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { *self.change_tick = self.this_run };
        self.value
    }
}

impl<T: std::fmt::Debug + 'static> std::fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

/// Per-archetype состояние `Write<T>`: базовый указатель данных + указатель на
/// массив change-ticks + текущий тик мира.
pub struct WriteState<T> {
    data: *mut T,
    ticks: *mut Tick,
    this_run: Tick,
}

impl<T> Clone for WriteState<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for WriteState<T> {}

// ── Read<T> ────────────────────────────────────────────────────

pub struct Read<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for Read<T> {
    type Item<'w> = &'w T;
    type State = *const T;

    #[inline]
    fn component_count() -> usize {
        1
    }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() {
            ids.push(id);
        }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], _: Tick, _: Tick) -> Self::State {
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
    type Item<'w> = Mut<'w, T>;
    type State = WriteState<T>;

    #[inline]
    fn component_count() -> usize {
        1
    }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() {
            ids.push(id);
        }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        this_run: Tick,
    ) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        WriteState {
            data: col.get_ptr(0) as *mut T,
            ticks: col.ticks_ptr() as *mut Tick,
            this_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        Some(Mut {
            value: &mut *state.data.add(row),
            change_tick: state.ticks.add(row),
            this_run: state.this_run,
        })
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Write<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().write::<T>()
    }
}

// ── Bevy-подобные алиасы и `&T`/`&mut T` синтаксис (C3) ─────────

/// Алиас `Read<T>` — для совместимости стиля (`Ref<T>` ≡ `Read<T>`).
pub type Ref<T> = Read<T>;

/// `&T` как спецификатор запроса (1:1 перенос с Bevy). Делегирует в [`Read<T>`],
/// выдаёт `&T`.
impl<T: Component> WorldQuery for &T {
    type Item<'w> = &'w T;
    type State = <Read<T> as WorldQuery>::State;

    #[inline]
    fn component_count() -> usize {
        <Read<T> as WorldQuery>::component_count()
    }
    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        <Read<T> as WorldQuery>::fill_ids(world, ids)
    }
    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        <Read<T> as WorldQuery>::matches_archetype(arch, ids)
    }
    #[inline]
    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], lr: Tick, tr: Tick) -> Self::State {
        <Read<T> as WorldQuery>::fetch_state(arch, ids, lr, tr)
    }
    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        <Read<T> as WorldQuery>::fetch_item(state, row)
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for &T {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

/// `&mut T` как спецификатор запроса (1:1 перенос с Bevy). Делегирует в
/// [`Write<T>`], выдаёт [`Mut<T>`] (со стампом change-tick на `DerefMut`).
impl<T: Component> WorldQuery for &mut T {
    type Item<'w> = Mut<'w, T>;
    type State = <Write<T> as WorldQuery>::State;

    #[inline]
    fn component_count() -> usize {
        <Write<T> as WorldQuery>::component_count()
    }
    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        <Write<T> as WorldQuery>::fill_ids(world, ids)
    }
    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        <Write<T> as WorldQuery>::matches_archetype(arch, ids)
    }
    #[inline]
    unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], lr: Tick, tr: Tick) -> Self::State {
        <Write<T> as WorldQuery>::fetch_state(arch, ids, lr, tr)
    }
    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        <Write<T> as WorldQuery>::fetch_item(state, row)
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for &mut T {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().write::<T>()
    }
}

// ── With<T> ────────────────────────────────────────────────────

pub struct With<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> WorldQuery for With<T> {
    type Item<'w> = ();
    type State = ();

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() {
            ids.push(id);
        }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(_: &Archetype, _: &[ComponentId], _: Tick, _: Tick) -> Self::State {}

    #[inline(always)]
    unsafe fn fetch_item<'w>(_: Self::State, _: usize) -> Option<Self::Item<'w>> {
        Some(())
    }
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
    type State = ();

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }
    #[inline]
    fn is_positive() -> bool {
        false
    }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() {
            ids.push(id);
        }
    }

    fn fill_positive_ids(_: &World, _: &mut Vec<ComponentId>) {}

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        ids.is_empty() || !arch.has_component(ids[0])
    }

    unsafe fn fetch_state(_: &Archetype, _: &[ComponentId], _: Tick, _: Tick) -> Self::State {}

    #[inline(always)]
    unsafe fn fetch_item<'w>(_: Self::State, _: usize) -> Option<Self::Item<'w>> {
        Some(())
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Without<T> {
    fn system_access() -> AccessDescriptor {
        // Without не читает данные T — нет доступа к T вообще
        AccessDescriptor::new()
    }
}

// ── Maybe<T> — опциональное чтение (Optional <T>) ──────────────

/// Опциональный компонент — аналог `Option<&T>`.
///
/// В отличие от `Read<T>`, не требует обязательного наличия компонента.
/// Всегда итерирует все entity: если компонент отсутствует — возвращает `None`.
///
/// # Пример
///
/// ```ignore
/// Query::<(Read<A>, Maybe<B>)>::new(&world)
///     .for_each(|entity, (a, b)| {
///         // a: &A — всегда есть
///         // b: Option<&B> — может быть None
///     });
/// ```
pub struct Maybe<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct MaybeState {
    data: *const u8,
    item_size: usize,
    present: bool,
}

unsafe impl Send for MaybeState {}
unsafe impl Sync for MaybeState {}

impl MaybeState {
    fn absent() -> Self {
        MaybeState {
            data: std::ptr::null(),
            item_size: 0,
            present: false,
        }
    }
}

impl<T: Component> WorldQuery for Maybe<T> {
    type Item<'w> = Option<&'w T>;
    type State = MaybeState;

    #[inline]
    fn component_count() -> usize {
        1
    }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() {
            ids.push(id);
        }
    }

    /// Optional-компонент: присутствие НЕ обязательно — кандидатов не сужает.
    fn fill_required_ids(_: &World, _: &mut Vec<ComponentId>) {}

    fn matches_archetype(_: &Archetype, _: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        _: Tick,
    ) -> Self::State {
        if ids.is_empty() || !arch.has_component(ids[0]) {
            return MaybeState::absent();
        }
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        MaybeState {
            data: col.data,
            item_size: col.item_size,
            present: true,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        if state.present {
            if state.item_size == 0 {
                // ZST — колонка не аллоцирует память, используем dangling
                Some(Some(&*(std::ptr::NonNull::<T>::dangling().as_ptr())))
            } else {
                Some(Some(&*(state.data.add(row * state.item_size) as *const T)))
            }
        } else {
            Some(None)
        }
    }

    fn requires_all_ids() -> bool {
        false
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Maybe<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── MaybeWrite<T> — опциональная запись (Optional <&mut T>) ────

/// Опциональный мутабельный компонент — аналог `Option<&mut T>`.
///
/// Всегда итерирует все entity: если компонент отсутствует — возвращает `None`.
pub struct MaybeWrite<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct MaybeMutState {
    data: *mut u8,
    ticks: *mut Tick,
    item_size: usize,
    present: bool,
    this_run: Tick,
}

unsafe impl Send for MaybeMutState {}
unsafe impl Sync for MaybeMutState {}

impl MaybeMutState {
    fn absent() -> Self {
        MaybeMutState {
            data: std::ptr::null_mut(),
            ticks: std::ptr::null_mut(),
            item_size: 0,
            present: false,
            this_run: Tick::ZERO,
        }
    }
}

impl<T: Component> WorldQuery for MaybeWrite<T> {
    type Item<'w> = Option<Mut<'w, T>>;
    type State = MaybeMutState;

    #[inline]
    fn component_count() -> usize {
        1
    }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() {
            ids.push(id);
        }
    }

    /// Optional-компонент: присутствие НЕ обязательно — кандидатов не сужает.
    fn fill_required_ids(_: &World, _: &mut Vec<ComponentId>) {}

    fn matches_archetype(_: &Archetype, _: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        this_run: Tick,
    ) -> Self::State {
        if ids.is_empty() || !arch.has_component(ids[0]) {
            return MaybeMutState::absent();
        }
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        MaybeMutState {
            data: col.data,
            ticks: col.ticks_ptr() as *mut Tick,
            item_size: col.item_size,
            present: true,
            this_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        if state.present {
            let value = if state.item_size == 0 {
                &mut *std::ptr::NonNull::<T>::dangling().as_ptr()
            } else {
                &mut *(state.data.add(row * state.item_size) as *mut T)
            };
            Some(Some(Mut {
                value,
                change_tick: state.ticks.add(row),
                this_run: state.this_run,
            }))
        } else {
            Some(None)
        }
    }

    fn requires_all_ids() -> bool {
        false
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for MaybeWrite<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().write::<T>()
    }
}

// ── Changed<T> ─────────────────────────────────────────────────

pub struct Changed<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct ChangedState {
    ticks: *const Tick,
    last_run: Tick,
}

unsafe impl Send for ChangedState {}
unsafe impl Sync for ChangedState {}

impl<T: Component> WorldQuery for Changed<T> {
    type Item<'w> = ();
    type State = ChangedState;

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }

    fn fill_ids(world: &World, ids: &mut Vec<ComponentId>) {
        if let Some(id) = world.registry.get_id::<T>() {
            ids.push(id);
        }
    }

    fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
        !ids.is_empty() && arch.has_component(ids[0])
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        last_run: Tick,
        _: Tick,
    ) -> Self::State {
        let col_idx = arch.column_index(ids[0]).unwrap_unchecked();
        let col = &arch.columns[col_idx];
        ChangedState {
            ticks: col.ticks_ptr(),
            last_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        let tick = *state.ticks.add(row);
        if tick.is_newer_than(state.last_run) {
            Some(())
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

            fn fill_required_ids(world: &World, ids: &mut Vec<ComponentId>) {
                $( $Q::fill_required_ids(world, ids); )+
            }

            fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
                let mut offset = 0;
                $(
                    let n = $Q::component_count();
                    let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                    if !$Q::matches_archetype(arch, slice) { return false; }
                    #[allow(unused_assignments)] { offset += n; }
                )+
                true
            }

            fn requires_all_ids() -> bool {
                let mut all = true;
                $( all = all && $Q::requires_all_ids(); )+
                all
            }

            unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick, this_run: Tick) -> Self::State {
                let mut offset = 0;
                ($(
                    {
                        let n = $Q::component_count();
                        let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                        let s = $Q::fetch_state(arch, slice, last_run, this_run);
                        #[allow(unused_assignments)] { offset += n; }
                        s
                    },
                )+)
            }

            #[inline(always)]
            unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
                Some(( $( {
                    let item = $Q::fetch_item(state.$idx, row);
                    if item.is_none() { return None; }
                    item.unwrap_unchecked()
                }, )+ ))
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

// ── () — пустой запрос (для AutoSystem без компонентного доступа) ─

impl WorldQuery for () {
    type Item<'w> = ();
    type State = ();

    fn component_count() -> usize {
        0
    }

    fn fill_ids(_world: &World, _ids: &mut Vec<ComponentId>) {}

    fn matches_archetype(_arch: &Archetype, _ids: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        _arch: &Archetype,
        _ids: &[ComponentId],
        _last_run: Tick,
        _this_run: Tick,
    ) -> Self::State {
    }

    unsafe fn fetch_item<'w>(_state: Self::State, _row: usize) -> Option<Self::Item<'w>> {
        Some(())
    }
}

impl WorldQuerySystemAccess for () {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new()
    }
}

impl_world_query_tuple!((A, 0));
impl_world_query_tuple!((A, 0), (B, 1));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_world_query_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_world_query_tuple!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7)
);

// ── ArchState ──────────────────────────────────────────────────

pub(crate) struct ArchState<S> {
    pub arch_idx: usize,
    pub state: S,
    pub len: usize,
}

// ── Query<Q> ───────────────────────────────────────────────────

pub struct Query<'w, Q: WorldQuery> {
    world: &'w World,
    archetypes: Vec<ArchState<Q::State>>,
    #[allow(dead_code)]
    last_run: Tick,
    /// Ограничения строк для row-level splits.
    /// Если не пусто — итерация ограничена `(arch_idx, start, end)`.
    row_ranges: &'w [(usize, usize, usize)],
}

impl<'w, Q: WorldQuery> Query<'w, Q> {
    pub fn new(world: &'w World) -> Self {
        Self::new_with_tick(world, Tick::ZERO)
    }

    /// Создать Query с ограничением на архетипы и строки из SubWorld.
    ///
    /// Использует `sub.archetype_indices` для фильтрации архетипов
    /// и `sub.row_ranges` для ограничения строк (row-level splits).
    pub fn from_sub_world(sub: &'w SubWorld<'w>, last_run: Tick) -> Self {
        let mut q = Self::new_within_archetypes(sub.world(), sub.archetype_indices(), last_run);
        q.row_ranges = sub.row_ranges();
        q
    }

    /// Создать Query, перебирающий только указанные архетипы.
    /// Используется из from_sub_world для сканирования archetype_indices SubWorld.
    fn new_within_archetypes(world: &'w World, arch_indices: &[usize], last_run: Tick) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        // Without-семантика целиком в matches_archetype (Without::matches_archetype
        // проверяет отсутствие сам) — отдельная exclude-маска не нужна (CR-M4).
        let arch_filter = |arch_idx: usize| -> bool {
            let arch = &world.archetypes[arch_idx];
            !arch.is_empty() && Q::matches_archetype(arch, &ids)
        };

        let archetypes: Vec<ArchState<Q::State>> =
            if ids.len() == Q::component_count() || !Q::requires_all_ids() {
                arch_indices
                    .iter()
                    .copied()
                    .filter(|&arch_idx| arch_filter(arch_idx))
                    .map(|arch_idx| {
                        let state =
                            unsafe { Q::fetch_state(&world.archetypes[arch_idx], &ids, last_run, world.current_tick()) };
                        ArchState {
                            arch_idx,
                            state,
                            len: world.archetypes[arch_idx].len(),
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };

        Self {
            world,
            archetypes,
            last_run,
            row_ranges: &[],
        }
    }

    pub fn new_with_tick(world: &'w World, last_run: Tick) -> Self {
        let mut ids = Vec::with_capacity(Q::component_count());
        Q::fill_ids(world, &mut ids);

        // Without-семантика целиком в matches_archetype (Without::matches_archetype
        // проверяет отсутствие сам) — отдельная exclude-маска не нужна (CR-M4).
        let arch_filter = |arch_idx: usize| -> bool {
            let arch = &world.archetypes[arch_idx];
            !arch.is_empty() && Q::matches_archetype(arch, &ids)
        };

        // Порог линейного обхода: на малых мирах сканировать все архетипы
        // дешевле, чем ходить в component_arch_index (hash-lookup на компонент).
        const LINEAR_SCAN_MAX_ARCHETYPES: usize = 128;

        let archetypes = if ids.len() == Q::component_count() || !Q::requires_all_ids() {
            // Обязательные компоненты (без Maybe/Without) — источник кандидатов.
            let mut required_ids = Vec::with_capacity(Q::component_count());
            Q::fill_required_ids(world, &mut required_ids);

            if required_ids.is_empty() || world.archetypes.len() <= LINEAR_SCAN_MAX_ARCHETYPES {
                // Линейный обход: мир мал ЛИБО запрос без обязательных
                // компонентов (Without-only / Maybe-only / пустой) — такие
                // матчат почти всё, кандидат-индекс не сузит.
                world
                    .archetypes
                    .iter()
                    .enumerate()
                    .filter(|&(arch_idx, _arch)| arch_filter(arch_idx))
                    .map(|(arch_idx, arch)| {
                        let state = unsafe { Q::fetch_state(arch, &ids, last_run, world.current_tick()) };
                        ArchState {
                            arch_idx,
                            state,
                            len: arch.len(),
                        }
                    })
                    .collect()
            } else {
                // Кандидаты = архетипы САМОГО РЕДКОГО обязательного компонента
                // из component_arch_index: O(кандидатов), не O(всех архетипов).
                // Отсутствие записи в индексе = компонент не встречается ни в
                // одном архетипе → запрос заведомо пуст (кандидатов нет).
                let mut candidates: &[crate::archetype::ArchetypeId] = &[];
                let mut best = usize::MAX;
                for id in &required_ids {
                    let list = world
                        .component_arch_index
                        .get(id)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    if list.len() < best {
                        best = list.len();
                        candidates = list;
                    }
                }

                candidates
                    .iter()
                    .map(|id| id.0 as usize)
                    .filter(|&arch_idx| arch_filter(arch_idx))
                    .map(|arch_idx| {
                        let arch = &world.archetypes[arch_idx];
                        let state = unsafe { Q::fetch_state(arch, &ids, last_run, world.current_tick()) };
                        ArchState {
                            arch_idx,
                            state,
                            len: arch.len(),
                        }
                    })
                    .collect()
            }
        } else {
            Vec::new()
        };

        Self {
            world,
            archetypes,
            last_run,
            row_ranges: &[],
        }
    }

    /// Получить диапазон строк для архетипа, если есть row_ranges.
    fn row_range(&self, arch_idx: usize) -> (usize, usize) {
        self.row_ranges
            .iter()
            .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
            .unwrap_or((0, usize::MAX))
    }

    /// Итератор по entity и компонентам.
    pub fn iter(&self) -> QueryIter<'_, Q> {
        QueryIter {
            archetypes: &self.archetypes,
            world: self.world,
            arch_cursor: 0,
            row_cursor: 0,
            row_ranges: self.row_ranges,
        }
    }

    /// Consuming итератор — для использования в ParamQuery.
    #[allow(dead_code)]
    pub(crate) fn into_iter_owned(self) -> QueryIterOwned<'w, Q> {
        QueryIterOwned {
            query: self,
            arch_cursor: 0,
            row_cursor: 0,
        }
    }

    #[inline]
    pub fn for_each<F: FnMut(Entity, Q::Item<'_>)>(&self, mut f: F) {
        for a in &self.archetypes {
            let (row_start, row_end) = self.row_range(a.arch_idx);
            let end = row_end.min(a.len);
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let entities = &self.world.archetypes[a.arch_idx].entities[row_start..end];
            for (offset, &entity) in entities.iter().enumerate() {
                let row = row_start + offset;
                if let Some(item) = unsafe { Q::fetch_item(a.state, row) } {
                    f(entity, item);
                }
            }
        }
    }

    /// Параллельная итерация.
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

        // Учитываем row_ranges при вычислении длины архетипов для chunk'ирования
        let row_ranges = self.row_ranges;
        let rr = |arch_idx: usize| -> (usize, usize) {
            row_ranges
                .iter()
                .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
                .unwrap_or((0, usize::MAX))
        };
        let chunks = compute_par_chunks(
            self.archetypes.iter().map(|a| {
                let s = rr(a.arch_idx);
                let effective_len = s.1.min(a.len).saturating_sub(s.0);
                (a.arch_idx, effective_len)
            }),
            num_threads,
            self.world.chunk_config(),
        );

        let last_run = self.last_run;
        let world = self.world;

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let (r_start, r_end) = rr(arch_idx);
            let clamped_start = r_start + start;
            let clamped_end = (r_start + end).min(r_end);
            if clamped_start >= clamped_end {
                return;
            }
            let arch = unsafe { &*world.archetypes.as_ptr().add(arch_idx) };
            let state = unsafe { Q::fetch_state(arch, &ids, last_run, world.current_tick()) };
            let entities = &arch.entities[clamped_start..clamped_end];
            for (offset, &entity) in entities.iter().enumerate() {
                let row = clamped_start + offset;
                if let Some(item) = unsafe { Q::fetch_item(state, row) } {
                    f(entity, item);
                }
            }
        });
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
    row_ranges: &'q [(usize, usize, usize)],
}

impl<'q, Q: WorldQuery> Iterator for QueryIter<'q, Q> {
    type Item = (Entity, Q::Item<'q>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let a = self.archetypes.get(self.arch_cursor)?;
            let (r_start, r_end) = self.row_range(a.arch_idx);
            let effective_end = r_end.min(a.len);
            if self.row_cursor < r_start {
                self.row_cursor = r_start;
            }
            if self.row_cursor >= effective_end {
                self.arch_cursor += 1;
                self.row_cursor = 0;
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

impl<'q, Q: WorldQuery> QueryIter<'q, Q> {
    fn row_range(&self, arch_idx: usize) -> (usize, usize) {
        self.row_ranges
            .iter()
            .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
            .unwrap_or((0, usize::MAX))
    }
}

pub struct QueryIterOwned<'w, Q: WorldQuery> {
    query: Query<'w, Q>,
    arch_cursor: usize,
    row_cursor: usize,
}

impl<'w, Q: WorldQuery> Iterator for QueryIterOwned<'w, Q> {
    type Item = (Entity, Q::Item<'w>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let a = self.query.archetypes.get(self.arch_cursor)?;
            let (r_start, r_end) = self.query.row_range(a.arch_idx);
            let effective_end = r_end.min(a.len);
            if self.row_cursor < r_start {
                self.row_cursor = r_start;
            }
            if self.row_cursor >= effective_end {
                self.arch_cursor += 1;
                self.row_cursor = 0;
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
    world: &'w World,
    reads: Vec<ComponentId>,
    writes: Vec<ComponentId>,
    excludes: Vec<ComponentId>,
}

impl<'w> QueryBuilder<'w> {
    pub fn new(world: &'w World) -> Self {
        Self {
            world,
            reads: Vec::new(),
            writes: Vec::new(),
            excludes: Vec::new(),
        }
    }

    pub fn read<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() {
            self.reads.push(id);
        }
        self
    }

    pub fn write<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() {
            self.writes.push(id);
        }
        self
    }

    pub fn exclude<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() {
            self.excludes.push(id);
        }
        self
    }

    pub fn matching_archetype_ids(&self) -> Vec<usize> {
        self.world
            .archetypes
            .iter()
            .enumerate()
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

    use crate::component::Component;

    struct A;
    impl Component for A {}
    struct B;
    impl Component for B {}

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
        assert_eq!(
            results,
            vec![e1],
            "Without<B> должен исключить сущности с B"
        );

        // Query<Read<B>, Without<A>> должен вернуть только e3
        let query: Query<'_, (Read<B>, Without<A>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _)| e).collect();
        assert_eq!(
            results,
            vec![e3],
            "Without<A> должен исключить сущности с A"
        );

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

    #[derive(Debug)]
    struct Pos {
        x: f32,
    }
    impl Component for Pos {}

    /// C1: мутация через `Query<Write<T>>` должна делать `Changed<T>`
    /// достоверным (раньше change-tick ставился только в `World::get_mut`).
    #[test]
    fn write_query_marks_changed() {
        let mut world = World::new();
        let target = world.spawn((Pos { x: 0.0 },));
        let _other = world.spawn((Pos { x: 100.0 },));

        // Базовая линия: тик, относительно которого считаем "изменения".
        world.tick();
        let last_run = world.current_tick();
        world.tick();

        // Мутируем ТОЛЬКО target через Query<Write<Pos>>.
        {
            let q: Query<'_, Write<Pos>> = Query::new(&world);
            q.for_each(|e, mut p| {
                if e == target {
                    p.x += 1.0;
                }
            });
        }

        // Changed<Pos> относительно last_run должен вернуть ровно target.
        let changed: Vec<_> =
            Query::<(crate::query::Changed<Pos>, Read<Pos>)>::new_with_tick(&world, last_run)
                .iter()
                .map(|(e, _)| e)
                .collect();
        assert_eq!(
            changed,
            vec![target],
            "мутация через Query<Write<T>> должна помечать Changed<T>"
        );
    }

    /// C3: Bevy-подобный синтаксис `&T` / `&mut T` в запросах.
    #[test]
    fn bevy_ref_syntax_query() {
        let mut world = World::new();
        let e = world.spawn((Pos { x: 1.0 },));

        // Чтение через &Pos, мутация через &mut Pos — как в Bevy.
        Query::<(&Pos,)>::new(&world).for_each(|_, (p,)| {
            assert_eq!(p.x, 1.0);
        });
        Query::<&mut Pos>::new(&world).for_each(|_, mut p| {
            p.x += 10.0;
        });
        assert_eq!(world.get::<Pos>(e).unwrap().x, 11.0);

        // &mut стампит change-tick (как Write) — Changed достоверен.
        world.tick();
        let lr = world.current_tick();
        world.tick();
        Query::<&mut Pos>::new(&world).for_each(|_, mut p| {
            p.x += 1.0;
        });
        let changed = Query::<crate::query::Changed<Pos>>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(changed, 1, "&mut T должен помечать Changed как Write<T>");
    }

    /// Чистое чтение через `Write<T>` без `DerefMut` НЕ должно помечать изменённым
    /// (стамп происходит только при мутабельном разыменовании).
    #[test]
    fn write_query_read_only_no_change() {
        let mut world = World::new();
        let _e = world.spawn((Pos { x: 5.0 },));

        world.tick();
        let last_run = world.current_tick();
        world.tick();

        {
            let q: Query<'_, Write<Pos>> = Query::new(&world);
            let mut sink = 0.0;
            q.for_each(|_, p| {
                // Только Deref (чтение) — без DerefMut.
                sink += p.x;
            });
            std::hint::black_box(sink);
        }

        let changed_count = Query::<crate::query::Changed<Pos>>::new_with_tick(&world, last_run)
            .iter()
            .count();
        assert_eq!(changed_count, 0, "чтение через Write<T> не должно помечать Changed");
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
        assert_eq!(
            results,
            vec![e2],
            "Without<A> должен вернуть сущности без A"
        );
    }

    #[test]
    fn maybe_optional_component_simple() {
        let mut world = World::new();
        // Все entity имеют и A и B — один архетип
        world.spawn((A, B));
        world.spawn((A, B));

        let query: Query<'_, (Read<A>, Maybe<B>)> = Query::new(&world);
        let count = query.iter().count();
        assert_eq!(count, 2, "Должно быть 2 entity с A");
    }

    #[test]
    fn maybe_optional_component_mixed_archetypes() {
        let mut world = World::new();
        // Два архетипа: [A,B] и [A]
        world.spawn((A, B));
        world.spawn((A,));
        world.spawn((B,));

        let query: Query<'_, (Read<A>, Maybe<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(_, (_, b))| b.is_some()).collect();

        assert_eq!(results.len(), 2, "Должно быть 2 сущности с A");
        // e1 имеет A+B → b.is_some() == true
        // e2 имеет A → b.is_some() == false
        assert!(results[0] != results[1], "Одна должна иметь B, другая нет");
    }

    #[test]
    fn maybe_write_optional_mut() {
        let mut world = World::new();

        world.spawn((A, B));
        world.spawn((A,));

        // Опциональная запись: у кого есть B — удваиваем
        let query: Query<'_, (Read<A>, MaybeWrite<B>)> = Query::new(&world);
        let results: Vec<_> = query
            .iter()
            .map(|(_, (_, b_opt))| b_opt.is_some())
            .collect();

        assert_eq!(results.len(), 2);
        let has_b_count = results.iter().filter(|b| **b).count();
        assert_eq!(has_b_count, 1, "Только одна сущность имеет B");
    }

    #[test]
    fn maybe_with_unregistered_component() {
        let mut world = World::new();
        // Регистрируем только A — B не используется
        world.spawn((A,));

        // Query<Maybe<B>> — B никогда не регистрировался
        let query: Query<'_, Maybe<B>> = Query::new(&world);
        // Должен вернуть entity, но B будет None
        let results: Vec<_> = query.iter().collect();
        assert_eq!(results.len(), 1, "Должна вернуться entity без B");
        assert!(results[0].1.is_none(), "B должен быть None");
    }

    #[test]
    fn maybe_tuple_integration() {
        let mut world = World::new();

        #[derive(Debug, PartialEq)]
        struct C(u32);
        impl Component for C {}
        #[derive(Debug, PartialEq)]
        struct D(u32);
        impl Component for D {}

        let _e1 = world.spawn((A, C(1)));
        let _e2 = world.spawn((B, D(2)));

        // (Maybe<A>, Maybe<C>) — все entity
        let query: Query<'_, (Maybe<A>, Maybe<C>)> = Query::new(&world);
        let results: Vec<_> = query
            .iter()
            .map(|(_, (a, c))| (a.is_some(), c.is_some()))
            .collect();

        // Должно быть 2 entity
        assert_eq!(results.len(), 2);
        // e1: A=Some, C=Some
        assert!(results[0].0 && results[0].1);
        // e2: A=None, C=None
        assert!(!results[1].0 && !results[1].1);
    }
}
