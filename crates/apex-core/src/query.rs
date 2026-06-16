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

/// Inline-буфер ComponentId формы запроса: до 8 компонентов БЕЗ heap-аллокации
/// (W2-0 — `fill_ids` на горячем пути `ctx.query` каждый вызов).
pub type IdBuf = smallvec::SmallVec<[ComponentId; 8]>;

/// Роли компонентов в ключе кэша запросов (старшие биты поверх ComponentId).
pub const KEY_ROLE_WITHOUT: u64 = 1 << 32;
pub const KEY_ROLE_OPTIONAL: u64 = 2 << 32;
/// Структурные маркеры `Or<>`-группы в ключе кэша (W2-5). Не несут ComponentId
/// и ПРОПУСКАЮТСЯ при восстановлении ids из ключа (см. [`KEY_MARKER_BIT`]):
/// `(Or<(With<A>,)>, With<B>)` и `Or<(With<A>, With<B>)>` обязаны давать
/// разные записи кэша — у них разная матч-семантика при одинаковых ids.
pub const KEY_OR_OPEN: u64 = 4 << 32;
pub const KEY_OR_CLOSE: u64 = 5 << 32;
/// Бит «запись ключа — структурный маркер, а не компонент».
pub const KEY_MARKER_BIT: u64 = 4 << 32;

pub trait WorldQuery: Sized {
    type Item<'w>;
    type State: Copy;

    fn component_count() -> usize;

    /// Заполняет ComponentId формы. ИНВАРИАНТ (W2): каждая форма кладёт РОВНО
    /// `component_count()` записей — незарегистрированный компонент кодируется
    /// сентинелом [`ComponentId::INVALID`]. Это гарантирует выравнивание
    /// сегментов ids в кортежах/`Or` при любой комбинации регистраций;
    /// «пустой запрос для незарегистрированного обязательного компонента»
    /// получается естественно: `has_component(INVALID)` не матчится нигде.
    fn fill_ids(world: &World, ids: &mut IdBuf);

    /// Заполняет только "positive" (не-Without) component IDs.
    /// По умолчанию — то же что fill_ids.
    fn fill_positive_ids(world: &World, ids: &mut IdBuf) {
        Self::fill_ids(world, ids);
    }

    /// Заполняет только ОБЯЗАТЕЛЬНЫЕ component IDs — те, без которых архетип
    /// заведомо не матчится (Read/Write/Ref/With/Changed). В отличие от
    /// `fill_positive_ids` НЕ включает optional (`Maybe`/`MaybeWrite`).
    /// Используется `Query::new` для выбора архетипов-кандидатов из
    /// `component_arch_index` (кандидаты = архетипы самого редкого
    /// обязательного компонента).
    fn fill_required_ids(world: &World, ids: &mut IdBuf) {
        Self::fill_positive_ids(world, ids);
    }

    /// Ключ кэша запросов (CR-M2b): по одному `u64` на компонент — ComponentId
    /// в нижних 32 битах, роль в верхних ([`KEY_ROLE_WITHOUT`]/[`KEY_ROLE_OPTIONAL`];
    /// обязательные — 0). Однозначно кодирует матч-семантику формы запроса
    /// ((Read<A>, Read<B>) ≠ (Read<A>, Without<B>) ≠ (Read<A>, Maybe<B>)).
    ///
    /// ВАЖНО: последовательность нижних 32 бит обязана совпадать с `fill_ids`
    /// (CachedQuery восстанавливает ids из ключа без второго прохода реестра).
    /// Один проход, без heap-аллокаций до 8 компонентов.
    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>);

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

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // роль REQUIRED = 0
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

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // роль REQUIRED = 0
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
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        <Read<T> as WorldQuery>::fill_ids(world, ids)
    }
    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        <Read<T> as WorldQuery>::fill_cache_key(world, key)
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
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        <Write<T> as WorldQuery>::fill_ids(world, ids)
    }
    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        <Write<T> as WorldQuery>::fill_cache_key(world, key)
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

// ── Entity как форма запроса (П1/TD-8, Bevy-паритет) ───────────

/// `Entity` — обычная форма запроса: `Query<(Entity, &Pos)>` выдаёт id
/// сущности в составе item. После П1 `iter()`/for-цикл выдают ТОЛЬКО item —
/// entity больше не навязана; нужна — запросите явно (как в Bevy).
impl WorldQuery for Entity {
    type Item<'w> = Entity;
    /// Указатель на массив `Archetype::entities` (живёт, пока жив мир и нет
    /// структурных изменений — стандартный инвариант итерации).
    type State = *const Entity;

    #[inline]
    fn component_count() -> usize {
        0
    }

    fn fill_ids(_world: &World, _ids: &mut IdBuf) {}

    fn fill_cache_key(_world: &World, _key: &mut smallvec::SmallVec<[u64; 8]>) {}

    fn matches_archetype(_arch: &Archetype, _ids: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(arch: &Archetype, _: &[ComponentId], _: Tick, _: Tick) -> Self::State {
        arch.entities().as_ptr()
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        Some(*state.add(row))
    }
}

impl WorldQuerySystemAccess for Entity {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new()
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

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // роль REQUIRED = 0
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

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_positive_ids(_: &World, _: &mut IdBuf) {}

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64 | KEY_ROLE_WITHOUT);
    }

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

    /// Optional ВСЕГДА вносит запись (сентинел [`ComponentId::INVALID`] для
    /// незарегистрированного T) — иначе компоненты ПОСЛЕ него в кортеже
    /// читали бы чужие id (выравнивание по `component_count`).
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    /// Optional-компонент: присутствие НЕ обязательно — кандидатов не сужает.
    fn fill_required_ids(_: &World, _: &mut IdBuf) {}

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64 | KEY_ROLE_OPTIONAL);
    }

    fn matches_archetype(_: &Archetype, _: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        _: Tick,
    ) -> Self::State {
        // INVALID-сентинел не матчится has_component'ом ни в одном архетипе.
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

    /// Optional ВСЕГДА вносит запись (сентинел [`ComponentId::INVALID`] для
    /// незарегистрированного T) — выравнивание ids по `component_count`.
    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    /// Optional-компонент: присутствие НЕ обязательно — кандидатов не сужает.
    fn fill_required_ids(_: &World, _: &mut IdBuf) {}

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64 | KEY_ROLE_OPTIONAL);
    }

    fn matches_archetype(_: &Archetype, _: &[ComponentId]) -> bool {
        true
    }

    unsafe fn fetch_state(
        arch: &Archetype,
        ids: &[ComponentId],
        _: Tick,
        this_run: Tick,
    ) -> Self::State {
        // INVALID-сентинел не матчится has_component'ом ни в одном архетипе.
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

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // роль REQUIRED = 0
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

// ── Added<T> ───────────────────────────────────────────────────

/// Фильтр «компонент `T` ДОБАВЛЕН entity после `last_run`» (W3-1, паритет
/// Bevy `Added<T>`).
///
/// Семантика added-тика: ставится при ПОЯВЛЕНИИ компонента у entity
/// (spawn / insert нового), переживает archetype move (insert/remove соседних
/// компонентов) и НЕ обновляется ни мутацией (`Changed`), ни `insert` поверх
/// существующего компонента (replace = Changed, не Added — как в Bevy).
/// Построчный фильтр: с плотной итерацией ([`DenseQuery`](crate::dense::DenseQuery))
/// не компилируется, как и `Changed<T>`.
pub struct Added<T: Component>(std::marker::PhantomData<T>);

#[derive(Clone, Copy)]
pub struct AddedState {
    added: *const Tick,
    last_run: Tick,
}

unsafe impl Send for AddedState {}
unsafe impl Sync for AddedState {}

impl<T: Component> WorldQuery for Added<T> {
    type Item<'w> = ();
    type State = AddedState;

    #[inline]
    fn component_count() -> usize {
        1
    }
    #[inline]
    fn is_filter() -> bool {
        true
    }

    fn fill_ids(world: &World, ids: &mut IdBuf) {
        ids.push(world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID));
    }

    fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
        let id = world.registry.get_id::<T>().unwrap_or(ComponentId::INVALID);
        key.push(id.0 as u64); // роль REQUIRED = 0
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
        AddedState {
            added: col.added_ticks_ptr(),
            last_run,
        }
    }

    #[inline(always)]
    unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
        let tick = *state.added.add(row);
        if tick.is_newer_than(state.last_run) {
            Some(())
        } else {
            None
        }
    }
}

impl<T: Component + 'static> WorldQuerySystemAccess for Added<T> {
    fn system_access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

// ── Or<> — дизъюнкция фильтров (W2-5) ──────────────────────────

/// Дизъюнкция фильтров уровня Bevy: строка проходит, если проходит ХОТЯ БЫ
/// ОДНА ветка. Главный потребитель — `Or<(Changed<A>, Changed<B>)>` вместо
/// двух запросов + dedup-set (паттерн extract-систем движка).
///
/// ```ignore
/// Query::<(Read<A>, Read<B>, Or<(Changed<A>, Changed<B>)>)>::new(&world)
///     .for_each(|e, (a, b, _)| { /* A ИЛИ B изменился */ });
/// ```
///
/// Семантика:
/// - архетип матчится, если матчится хотя бы одна ветка;
/// - ветка с незарегистрированным компонентом просто не матчится
///   (остальные работают);
/// - ветки — фильтры (`With`/`Without`/`Changed`/вложенный `Or`/кортежи-
///   конъюнкции из них); item ветки игнорируется, поэтому data-формы
///   (`Read` и пр.) внутри `Or` допускаются, но бессмысленны;
/// - `Or` не сужает кандидатов запроса (`fill_required_ids` пуст): строка
///   может пройти по любой ветке.
pub struct Or<T>(std::marker::PhantomData<T>);

macro_rules! impl_or_query {
    ( $( ($F:ident, $idx:tt) ),+ ) => {
        impl< $($F: WorldQuery),+ > WorldQuery for Or<( $($F,)+ )> {
            type Item<'w> = ();
            /// Per-arch состояние ветки: `Some(state)` — ветка матчит этот
            /// архетип, `None` — ветка мертва (state НЕ фетчится, иначе UB
            /// на отсутствующей колонке).
            type State = ( $(Option<$F::State>,)+ );

            #[inline]
            fn component_count() -> usize { 0 $( + $F::component_count() )+ }
            #[inline]
            fn is_filter() -> bool { true }

            /// Ветка с незарегистрированным компонентом несёт INVALID-сентинел
            /// (инвариант `fill_ids`) — мёртвая ветка не опустошает запрос.
            fn fill_ids(world: &World, ids: &mut IdBuf) {
                $( $F::fill_ids(world, ids); )+
            }

            fn fill_positive_ids(world: &World, ids: &mut IdBuf) {
                Self::fill_ids(world, ids);
            }

            /// Дизъюнкция не сужает кандидатов: строка может пройти по любой
            /// ветке, поэтому НИ ОДИН компонент Or не «обязателен».
            fn fill_required_ids(_: &World, _: &mut IdBuf) {}

            fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
                key.push(KEY_OR_OPEN);
                $( $F::fill_cache_key(world, key); )+
                key.push(KEY_OR_CLOSE);
            }

            fn matches_archetype(arch: &Archetype, ids: &[ComponentId]) -> bool {
                let mut offset = 0;
                $(
                    let n = $F::component_count();
                    let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                    if $F::matches_archetype(arch, slice) { return true; }
                    #[allow(unused_assignments)] { offset += n; }
                )+
                false
            }

            unsafe fn fetch_state(arch: &Archetype, ids: &[ComponentId], last_run: Tick, this_run: Tick) -> Self::State {
                let mut offset = 0;
                ($(
                    {
                        let n = $F::component_count();
                        let slice = if offset + n <= ids.len() { &ids[offset..offset + n] } else { &[] };
                        let s = if $F::matches_archetype(arch, slice) {
                            Some($F::fetch_state(arch, slice, last_run, this_run))
                        } else {
                            None
                        };
                        #[allow(unused_assignments)] { offset += n; }
                        s
                    },
                )+)
            }

            #[inline(always)]
            unsafe fn fetch_item<'w>(state: Self::State, row: usize) -> Option<Self::Item<'w>> {
                $(
                    if let Some(s) = state.$idx {
                        if $F::fetch_item(s, row).is_some() {
                            return Some(());
                        }
                    }
                )+
                None
            }
        }

        impl< $($F: WorldQuery + WorldQuerySystemAccess + 'static),+ >
            WorldQuerySystemAccess for Or<( $($F,)+ )>
        {
            fn system_access() -> AccessDescriptor {
                AccessDescriptor::new()
                    $( .merge(&$F::system_access()) )+
            }
        }
    };
}

impl_or_query!((A, 0));
impl_or_query!((A, 0), (B, 1));
impl_or_query!((A, 0), (B, 1), (C, 2));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_or_query!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_or_query!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7)
);

// ── Tuple impls ────────────────────────────────────────────────

macro_rules! impl_world_query_tuple {
    ( $( ($Q:ident, $idx:tt) ),+ ) => {
        impl< $($Q: WorldQuery),+ > WorldQuery for ( $($Q,)+ ) {
            type Item<'w> = ( $($Q::Item<'w>,)+ );
            type State    = ( $($Q::State,)+ );

            #[inline]
            fn component_count() -> usize { 0 $( + $Q::component_count() )+ }

            fn fill_ids(world: &World, ids: &mut IdBuf) {
                $( $Q::fill_ids(world, ids); )+
            }

            fn fill_positive_ids(world: &World, ids: &mut IdBuf) {
                $( $Q::fill_positive_ids(world, ids); )+
            }

            fn fill_required_ids(world: &World, ids: &mut IdBuf) {
                $( $Q::fill_required_ids(world, ids); )+
            }

            fn fill_cache_key(world: &World, key: &mut smallvec::SmallVec<[u64; 8]>) {
                $( $Q::fill_cache_key(world, key); )+
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

    fn fill_ids(_world: &World, _ids: &mut IdBuf) {}

    fn fill_cache_key(_world: &World, _key: &mut smallvec::SmallVec<[u64; 8]>) {}

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

// ── ArchetypeFilter (D2-2) ─────────────────────────────────────

/// Маркер «фильтр целиком архетипного уровня» — не смотрит на строки.
/// Реализован для `()`, `With<T>`, `Without<T>` и их кортежей. Требуется
/// плотной итерацией (`for_each_chunk`): построчные фильтры (`Changed`/
/// `Added`/`Or` с ними) со слайсовой выдачей несовместимы.
pub trait ArchetypeFilter: WorldQuery {}

impl ArchetypeFilter for () {}
impl<T: Component> ArchetypeFilter for With<T> {}
impl<T: Component> ArchetypeFilter for Without<T> {}

macro_rules! impl_archetype_filter_tuple {
    ( $($F:ident),+ ) => {
        impl< $($F: ArchetypeFilter),+ > ArchetypeFilter for ( $($F,)+ ) {}
    };
}
impl_archetype_filter_tuple!(A);
impl_archetype_filter_tuple!(A, B);
impl_archetype_filter_tuple!(A, B, C);
impl_archetype_filter_tuple!(A, B, C, D);
impl_archetype_filter_tuple!(A, B, C, D, E);
impl_archetype_filter_tuple!(A, B, C, D, E, F);
impl_archetype_filter_tuple!(A, B, C, D, E, F, G);
impl_archetype_filter_tuple!(A, B, C, D, E, F, G, H);

// ── ArchState ──────────────────────────────────────────────────

pub(crate) struct ArchState<S> {
    pub arch_idx: usize,
    pub state: S,
    pub len: usize,
}

// ── Query<Q, F = ()> ───────────────────────────────────────────

/// Запрос по компонентам. Вторым параметром принимает ФИЛЬТР (D2-2,
/// Bevy-форма): `Query<(&A, &mut B), (With<C>, Changed<A>)>` — данные и
/// фильтрация разнесены, item фильтра не попадает в выдачу. По умолчанию
/// `F = ()` — единый кортеж остаётся как вторая форма
/// (`Query<(Read<A>, With<C>)>` эквивалентен).
pub struct Query<'w, Q: WorldQuery, F: WorldQuery = ()> {
    world: &'w World,
    /// Inline до 8 архетипов (D2-1): типичный системный запрос матчит 1-5
    /// архетипов — конструктор без heap-аллокации (plain-fn `Query`-параметр
    /// строится на КАЖДЫЙ вызов системы).
    ///
    /// Состояние — ПАРЫ `(Q, F)`: data и filter исполняются одним проходом
    /// (matches = AND, fetch_item фильтра отбрасывается на выдаче).
    archetypes: smallvec::SmallVec<[ArchState<<(Q, F) as WorldQuery>::State>; 8]>,
    #[allow(dead_code)]
    last_run: Tick,
    /// Ограничения строк для row-level splits.
    /// Если не пусто — итерация ограничена `(arch_idx, start, end)`.
    row_ranges: &'w [(usize, usize, usize)],
}

impl<'w, Q: WorldQuery, F: WorldQuery> Query<'w, Q, F> {
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
        let mut ids = IdBuf::new();
        <(Q, F)>::fill_ids(world, &mut ids);
        debug_assert_eq!(
            ids.len(),
            <(Q, F)>::component_count(),
            "инвариант fill_ids нарушен"
        );

        // Without-семантика целиком в matches_archetype (Without::matches_archetype
        // проверяет отсутствие сам) — отдельная exclude-маска не нужна (CR-M4).
        let arch_filter = |arch_idx: usize| -> bool {
            let arch = &world.archetypes[arch_idx];
            !arch.is_empty() && <(Q, F)>::matches_archetype(arch, &ids)
        };

        let archetypes: smallvec::SmallVec<[ArchState<<(Q, F) as WorldQuery>::State>; 8]> =
            arch_indices
                .iter()
                .copied()
                .filter(|&arch_idx| arch_filter(arch_idx))
                .map(|arch_idx| {
                    let state = unsafe {
                        <(Q, F)>::fetch_state(
                            &world.archetypes[arch_idx],
                            &ids,
                            last_run,
                            world.current_tick(),
                        )
                    };
                    ArchState {
                        arch_idx,
                        state,
                        len: world.archetypes[arch_idx].len(),
                    }
                })
                .collect();

        Self {
            world,
            archetypes,
            last_run,
            row_ranges: &[],
        }
    }

    pub fn new_with_tick(world: &'w World, last_run: Tick) -> Self {
        let mut ids = IdBuf::new();
        <(Q, F)>::fill_ids(world, &mut ids);
        debug_assert_eq!(
            ids.len(),
            <(Q, F)>::component_count(),
            "инвариант fill_ids нарушен"
        );

        // Without-семантика целиком в matches_archetype (Without::matches_archetype
        // проверяет отсутствие сам) — отдельная exclude-маска не нужна (CR-M4).
        let arch_filter = |arch_idx: usize| -> bool {
            let arch = &world.archetypes[arch_idx];
            !arch.is_empty() && <(Q, F)>::matches_archetype(arch, &ids)
        };

        // Порог линейного обхода: на малых мирах сканировать все архетипы
        // дешевле, чем ходить в component_arch_index (hash-lookup на компонент).
        const LINEAR_SCAN_MAX_ARCHETYPES: usize = 128;

        let archetypes = {
            // Обязательные компоненты (без Maybe/Without) — источник кандидатов.
            // Считаются ТОЛЬКО на большом мире: на малом (типичный случай)
            // линейный обход не требует ни прохода реестра, ни аллокации.
            let mut required_ids = IdBuf::new();
            if world.archetypes.len() > LINEAR_SCAN_MAX_ARCHETYPES {
                <(Q, F)>::fill_required_ids(world, &mut required_ids);
            }

            if required_ids.is_empty() {
                // Линейный обход: мир мал ЛИБО запрос без обязательных
                // компонентов (Without-only / Maybe-only / пустой) — такие
                // матчат почти всё, кандидат-индекс не сузит.
                world
                    .archetypes
                    .iter()
                    .enumerate()
                    .filter(|&(arch_idx, _arch)| arch_filter(arch_idx))
                    .map(|(arch_idx, arch)| {
                        let state =
                            unsafe { <(Q, F)>::fetch_state(arch, &ids, last_run, world.current_tick()) };
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
                        let state =
                            unsafe { <(Q, F)>::fetch_state(arch, &ids, last_run, world.current_tick()) };
                        ArchState {
                            arch_idx,
                            state,
                            len: arch.len(),
                        }
                    })
                    .collect()
            }
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

    /// Итератор по item'ам запроса (П1: entity — через форму
    /// `Query<(Entity, …)>`, как в Bevy).
    pub fn iter(&self) -> QueryIter<'_, Q, F> {
        QueryIter {
            archetypes: &self.archetypes,
            arch_cursor: 0,
            row_cursor: 0,
            row_ranges: self.row_ranges,
        }
    }

    /// Item конкретной entity, если она матчит запрос (П3, Bevy-паритет
    /// `Query::get`). O(1) по location + поиск архетипа среди матчащих
    /// (их единицы); построчные фильтры (`Changed`/`Added` в Q или F)
    /// применяются — не прошедшая фильтр entity даёт `None`.
    pub fn get(&self, entity: Entity) -> Option<Q::Item<'_>> {
        let loc = self.world.entities.get_location(entity)?;
        let arch_idx = loc.archetype_id.as_usize();
        let a = self.archetypes.iter().find(|a| a.arch_idx == arch_idx)?;
        let row = loc.row as usize;
        let (r_start, r_end) = self.row_range(arch_idx);
        if row < r_start || row >= r_end.min(a.len) {
            return None;
        }
        unsafe { <(Q, F)>::fetch_item(a.state, row) }.map(|(item, _)| item)
    }

    /// Алиас [`get`](Self::get) для мутабельных форм (Bevy-паритет;
    /// item `Write<T>` — это `Mut<T>`, `&self` достаточно).
    #[inline]
    pub fn get_mut(&mut self, entity: Entity) -> Option<Q::Item<'_>> {
        self.get(entity)
    }

    /// Ровно одна матчащаяся entity (Bevy-паритет, D2-2).
    ///
    /// `Err(QuerySingleError)` при нуле или нескольких. Выдаёт `Q::Item`
    /// (как Bevy); нужна entity — включите её в запрос:
    /// `Query<(Entity, &Hp)>` (П1). Работает и для мутабельных форм (item
    /// `Write<T>` — это `Mut<T>`), поэтому отдельного `&mut self`-варианта
    /// не требуется; [`single_mut`](Self::single_mut) — алиас для привычки
    /// мигранта.
    pub fn single(&self) -> Result<Q::Item<'_>, QuerySingleError> {
        let mut it = self.iter();
        let first = it.next().ok_or(QuerySingleError::NoEntities)?;
        if it.next().is_some() {
            return Err(QuerySingleError::MultipleEntities);
        }
        Ok(first)
    }

    /// Алиас [`single`](Self::single) (Bevy-паритет).
    #[inline]
    pub fn single_mut(&mut self) -> Result<Q::Item<'_>, QuerySingleError> {
        self.single()
    }

    /// Потребляющая форма [`single`](Self::single): item живёт `'w` (мировой
    /// заём), а не заём `&self` — нужна `Single<Q>`-параметру систем (Э5),
    /// который кладёт извлечённый item в поле и переживает локальный Query.
    pub fn single_inner(self) -> Result<(Entity, Q::Item<'w>), QuerySingleError> {
        let mut found: Option<(Entity, Q::Item<'w>)> = None;
        for a in &self.archetypes {
            let (row_start, row_end) = self.row_range(a.arch_idx);
            let end = row_end.min(a.len);
            if end <= row_start {
                continue;
            }
            let entities = &self.world.archetypes[a.arch_idx].entities[row_start..end];
            for (offset, &entity) in entities.iter().enumerate() {
                let row = row_start + offset;
                // SAFETY: state хранит указатели колонок, действительные весь
                // мировой заём 'w; self потребляется — повторного доступа через
                // этот Query не будет (та же дисциплина алиасинга, что у iter()).
                if let Some((item, _)) = unsafe { <(Q, F)>::fetch_item(a.state, row) } {
                    if found.is_some() {
                        return Err(QuerySingleError::MultipleEntities);
                    }
                    found = Some((entity, item));
                }
            }
        }
        found.ok_or(QuerySingleError::NoEntities)
    }

    #[inline]
    pub fn for_each<Func: FnMut(Entity, Q::Item<'_>)>(&self, mut f: Func) {
        for a in &self.archetypes {
            let (row_start, row_end) = self.row_range(a.arch_idx);
            let end = row_end.min(a.len);
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let entities = &self.world.archetypes[a.arch_idx].entities[row_start..end];
            // Entity грузим ЛЕНИВО — только для прошедших фильтр строк (см. CachedQuery::for_each
            // в world.rs): убирает загрузку entity на непрошедших строках фильтрованных запросов.
            for offset in 0..len {
                let row = row_start + offset;
                if let Some((item, _)) = unsafe { <(Q, F)>::fetch_item(a.state, row) } {
                    f(entities[offset], item);
                }
            }
        }
    }

    /// Параллельная итерация.
    pub fn par_for_each<Func>(&self, f: Func)
    where
        Q: Send,
        F: Send,
        Func: Fn(Entity, Q::Item<'_>) + Send + Sync,
    {
        use rayon::prelude::*;

        let num_threads = rayon::current_num_threads();

        // Предварительно вычисляем ID компонентов (как в new_with_tick)
        let mut ids = IdBuf::new();
        <(Q, F)>::fill_ids(self.world, &mut ids);

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
            let state = unsafe { <(Q, F)>::fetch_state(arch, &ids, last_run, world.current_tick()) };
            let entities = &arch.entities[clamped_start..clamped_end];
            for (offset, &entity) in entities.iter().enumerate() {
                let row = clamped_start + offset;
                if let Some((item, _)) = unsafe { <(Q, F)>::fetch_item(state, row) } {
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

    /// Плотная (chunk) итерация (W2-0.5): колбэк получает entities-слайс и
    /// СЛАЙСЫ колонок архетипа целиком — без per-row `fetch_item`. Доступна
    /// только не-фильтрующим формам ([`DenseQuery`]); `Changed<T>` не
    /// компилируется (построчный фильтр несовместим со слайсами).
    ///
    /// Write-колонки стампятся ДИАПАЗОНОМ при выдаче слайса: контракт
    /// «слайс на запись = весь диапазон changed».
    ///
    /// ```ignore
    /// Query::<(Read<Vel>, Write<Pos>)>::new(&world)
    ///     .for_each_chunk(|_entities, (vel, pos)| {
    ///         for i in 0..pos.len() { pos[i].0 += vel[i].0; } // SIMD-friendly
    ///     });
    /// ```
    pub fn for_each_chunk<Func>(&self, mut f: Func)
    where
        Q: crate::dense::DenseQuery,
        F: ArchetypeFilter,
        Func: FnMut(&[Entity], <Q as crate::dense::DenseQuery>::Slices<'_>),
    {
        let mut ids = IdBuf::new();
        Q::fill_ids(self.world, &mut ids);
        let this_run = self.world.current_tick();

        for a in &self.archetypes {
            let (row_start, row_end) = self.row_range(a.arch_idx);
            let end = row_end.min(a.len);
            let len = end.saturating_sub(row_start);
            if len == 0 {
                continue;
            }
            let arch = &self.world.archetypes[a.arch_idx];
            let slices = unsafe { Q::fetch_slices(arch, &ids, row_start, len, this_run) };
            f(&arch.entities[row_start..end], slices);
        }
    }

    /// Параллельная плотная итерация: те же chunk-диапазоны, что у
    /// [`par_for_each`](Self::par_for_each), но колбэк получает слайсы.
    pub fn par_for_each_chunk<Func>(&self, f: Func)
    where
        Q: crate::dense::DenseQuery + Send,
        F: ArchetypeFilter,
        Func: Fn(&[Entity], <Q as crate::dense::DenseQuery>::Slices<'_>) + Send + Sync,
    {
        use rayon::prelude::*;

        let num_threads = rayon::current_num_threads();
        let mut ids = IdBuf::new();
        Q::fill_ids(self.world, &mut ids);

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

        let world = self.world;
        let this_run = world.current_tick();

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let (r_start, r_end) = rr(arch_idx);
            let clamped_start = r_start + start;
            let clamped_end = (r_start + end).min(r_end);
            if clamped_start >= clamped_end {
                return;
            }
            let arch = unsafe { &*world.archetypes.as_ptr().add(arch_idx) };
            let len = clamped_end - clamped_start;
            let slices = unsafe { Q::fetch_slices(arch, &ids, clamped_start, len, this_run) };
            f(&arch.entities[clamped_start..clamped_end], slices);
        });
    }
}

// ── Итераторы ──────────────────────────────────────────────────

pub struct QueryIter<'q, Q: WorldQuery, F: WorldQuery = ()>
where
    Q::State: 'q,
    F::State: 'q,
{
    archetypes: &'q [ArchState<<(Q, F) as WorldQuery>::State>],
    arch_cursor: usize,
    row_cursor: usize,
    row_ranges: &'q [(usize, usize, usize)],
}

impl<'q, Q: WorldQuery, F: WorldQuery> Iterator for QueryIter<'q, Q, F> {
    /// П1 (TD-8): итерация выдаёт ТОЛЬКО `Q::Item` (Bevy 1:1). Entity больше
    /// не навязана — включайте её в запрос явно: `Query<(Entity, &Pos)>`.
    type Item = Q::Item<'q>;

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
            if let Some((item, _)) = unsafe { <(Q, F)>::fetch_item(a.state, row) } {
                return Some(item);
            }
        }
    }
}

impl<'q, Q: WorldQuery, F: WorldQuery> QueryIter<'q, Q, F> {
    fn row_range(&self, arch_idx: usize) -> (usize, usize) {
        self.row_ranges
            .iter()
            .find_map(|&(a, s, e)| if a == arch_idx { Some((s, e)) } else { None })
            .unwrap_or((0, usize::MAX))
    }
}

/// Ровно один матч запроса (Э5, 1:1 Bevy `Single`): параметр plain-fn системы;
/// система ПРОПУСКАЕТСЯ планировщиком в кадрах, где матчей 0 или >1
/// (skip-семантика, не паника). `Option<Single<Q, F>>` — `None` при нуле
/// матчей, пропуск только при >1.
///
/// ```ignore
/// fn update(camera: Single<(&mut DistanceFog, &mut LocalTransform), With<Camera>>) {
///     let (mut fog, mut tf) = camera.into_inner();
/// }
/// ```
pub struct Single<'w, Q: WorldQuery, F: WorldQuery = ()> {
    pub(crate) entity: Entity,
    pub(crate) item: Q::Item<'w>,
    pub(crate) _filter: std::marker::PhantomData<F>,
}

impl<'w, Q: WorldQuery, F: WorldQuery> Single<'w, Q, F> {
    /// Entity единственного матча.
    pub fn entity(&self) -> Entity {
        self.entity
    }

    /// Забрать item (для деструктуризации кортежа: `let (a, b) = s.into_inner()`).
    pub fn into_inner(self) -> Q::Item<'w> {
        self.item
    }
}

impl<'w, Q: WorldQuery, F: WorldQuery> std::ops::Deref for Single<'w, Q, F> {
    type Target = Q::Item<'w>;
    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl<'w, Q: WorldQuery, F: WorldQuery> std::ops::DerefMut for Single<'w, Q, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}


/// Ошибка [`Query::single`] (D2-2, Bevy-паритет).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuerySingleError {
    NoEntities,
    MultipleEntities,
}

impl std::fmt::Display for QuerySingleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEntities => write!(f, "Query::single: ни одной матчащейся entity"),
            Self::MultipleEntities => write!(f, "Query::single: больше одной матчащейся entity"),
        }
    }
}

impl std::error::Error for QuerySingleError {}

/// `for item in &query` (D2-2/П1) — Bevy 1:1: выдаёт `Q::Item`; entity —
/// через форму запроса (`for (e, hp) in &Query::<(Entity, &Hp)>::new(&w)`).
impl<'q, 'w, Q: WorldQuery, F: WorldQuery> IntoIterator for &'q Query<'w, Q, F> {
    type Item = Q::Item<'q>;
    type IntoIter = QueryIter<'q, Q, F>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// `for mut item in &mut query` — привычная Bevy-форма для мутабельных
/// запросов (наш `iter()` и так выдаёт `Mut<T>` через `&self`, но
/// `&mut`-форма оставлена для построчного переноса кода).
impl<'q, 'w, Q: WorldQuery, F: WorldQuery> IntoIterator for &'q mut Query<'w, Q, F> {
    type Item = Q::Item<'q>;
    type IntoIter = QueryIter<'q, Q, F>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
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
mod query_filter_tests {
    use super::*;
    use crate::component::Component;
    use crate::world::World;

    #[derive(Debug, PartialEq)]
    struct Hp(u32);
    impl Component for Hp {}
    #[derive(Debug, PartialEq)]
    struct Mana(u32);
    impl Component for Mana {}
    struct Boss;
    impl Component for Boss {}

    /// Bevy-форма `Query<Data, Filter>`: фильтр не попадает в выдачу.
    #[test]
    fn query_data_filter_form() {
        let mut world = World::new();
        let boss = world.spawn((Hp(100), Mana(50), Boss));
        let _mob = world.spawn((Hp(10), Mana(5)));

        // С фильтром (With<Boss>,): item — только данные; entity — явной
        // формой запроса (П1).
        let q = Query::<(Entity, Read<Hp>, Read<Mana>), (With<Boss>,)>::new(&world);
        let got: Vec<(Entity, &Hp, &Mana)> = q.iter().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, boss);
        assert_eq!(*got[0].1, Hp(100));
        drop(q);

        // Одиночный фильтр без кортежа тоже работает.
        let n = Query::<Read<Hp>, With<Boss>>::new(&world).iter().count();
        assert_eq!(n, 1);

        // Changed в фильтре: после advance — пусто, после мутации — снова 1.
        world.advance_change_tick();
        let lr = world.last_run_tick();
        let n = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 0);
        world.get_mut::<Hp>(boss).unwrap().0 += 1;
        let n = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr)
            .iter()
            .count();
        assert_eq!(n, 1);
    }

    /// `for item in &q` / `&mut q` — IntoIterator поверх iter() (D2-2/П1):
    /// item без навязанной entity (Bevy 1:1); entity — формой запроса.
    #[test]
    fn query_for_loop_iteration() {
        let mut world = World::new();
        world.spawn((Hp(1),));
        world.spawn((Hp(2),));

        let q = Query::<Read<Hp>>::new(&world);
        let mut sum = 0;
        for hp in &q {
            sum += hp.0;
        }
        assert_eq!(sum, 3);

        let mut q = Query::<Write<Hp>>::new(&world);
        for mut hp in &mut q {
            hp.0 *= 10;
        }
        let total: u32 = Query::<Read<Hp>>::new(&world).iter().map(|hp| hp.0).sum();
        assert_eq!(total, 30);

        // Entity — явной формой, как в Bevy:
        let q = Query::<(Entity, Read<Hp>)>::new(&world);
        let pairs: Vec<(Entity, &Hp)> = q.iter().collect();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|(e, _)| world.is_alive(*e)));
    }

    /// `single()` — Bevy-паритет: 0 → NoEntities, 1 → Ok, 2+ → MultipleEntities.
    #[test]
    fn query_single() {
        let mut world = World::new();
        assert_eq!(
            Query::<Read<Hp>>::new(&world).single().unwrap_err(),
            QuerySingleError::NoEntities
        );

        let e = world.spawn((Hp(42),));
        // Entity при необходимости — формой запроса (П1).
        let q = Query::<(Entity, Read<Hp>)>::new(&world);
        let (got_e, hp) = q.single().unwrap();
        assert_eq!((got_e, hp.0), (e, 42));
        drop(q);

        // single_mut: мутация через Mut<T>.
        let mut q = Query::<Write<Hp>>::new(&world);
        let mut hp = q.single_mut().unwrap();
        hp.0 = 7;
        drop(q);
        assert_eq!(world.get::<Hp>(e), Some(&Hp(7)));

        world.spawn((Hp(1),));
        assert_eq!(
            Query::<Read<Hp>>::new(&world).single().unwrap_err(),
            QuerySingleError::MultipleEntities
        );
    }

    /// `get(entity)` — random-access внутри запроса (П3, Bevy-паритет):
    /// O(1) по location, фильтры (арх- и построчные) применяются.
    #[test]
    fn query_get_by_entity() {
        let mut world = World::new();
        let boss = world.spawn((Hp(100), Boss));
        let mob = world.spawn((Hp(10),));

        let q = Query::<Read<Hp>, With<Boss>>::new(&world);
        assert_eq!(q.get(boss), Some(&Hp(100)));
        assert_eq!(q.get(mob), None, "не матчит фильтр With<Boss>");
        drop(q);

        // Построчный фильтр: Changed применяется и в get().
        world.advance_change_tick();
        let lr = world.last_run_tick();
        world.get_mut::<Hp>(mob).unwrap().0 += 1;
        let q = Query::<Read<Hp>, Changed<Hp>>::new_with_tick(&world, lr);
        assert_eq!(q.get(mob), Some(&Hp(11)));
        assert_eq!(q.get(boss), None, "boss не менялся");
        drop(q);

        // get_mut: мутация через Mut<T>.
        let mut q = Query::<Write<Hp>>::new(&world);
        q.get_mut(boss).unwrap().0 = 1;
        drop(q);
        assert_eq!(world.get::<Hp>(boss), Some(&Hp(1)));
    }

    /// Архетипный фильтр совместим с плотной итерацией; данные приходят
    /// только из отфильтрованных архетипов.
    #[test]
    fn query_filter_with_chunks() {
        let mut world = World::new();
        world.spawn((Hp(1), Boss));
        world.spawn((Hp(2),));

        let mut sum = 0;
        Query::<Read<Hp>, With<Boss>>::new(&world).for_each_chunk(|_, hp| {
            sum += hp.iter().map(|h| h.0).sum::<u32>();
        });
        assert_eq!(sum, 1, "for_each_chunk уважает архетипный фильтр");
    }
}

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
        let query: Query<'_, (Entity, Read<A>, Without<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _, _)| e).collect();
        assert_eq!(
            results,
            vec![e1],
            "Without<B> должен исключить сущности с B"
        );

        // Query<Read<B>, Without<A>> должен вернуть только e3
        let query: Query<'_, (Entity, Read<B>, Without<A>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _, _)| e).collect();
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
        let query: Query<'_, (Entity, Read<A>, Without<B>)> = Query::new(&world);
        let results: Vec<_> = query.iter().map(|(e, _, _)| e).collect();

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
            Query::<(Entity, crate::query::Changed<Pos>, Read<Pos>)>::new_with_tick(&world, last_run)
                .iter()
                .map(|(e, _, _)| e)
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
        let query: Query<'_, (Entity, Without<A>)> = Query::new(&world);
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
        let results: Vec<_> = query.iter().map(|(_, b)| b.is_some()).collect();

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
            .map(|(_, b_opt)| b_opt.is_some())
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
        assert!(results[0].is_none(), "B должен быть None");
    }

    /// Регрессия W2: (Maybe<X>, Read<A>) с НЕзарегистрированным X раньше
    /// смещал ids — Read<A> читал чужой сегмент и запрос был ложно пуст.
    /// INVALID-сентинел сохраняет выравнивание.
    #[test]
    fn maybe_unregistered_does_not_misalign_following_ids() {
        struct NeverRegistered;
        impl Component for NeverRegistered {}

        let mut world = World::new();
        world.spawn((A,));
        world.spawn((A, B));

        let query: Query<'_, (Maybe<NeverRegistered>, Read<A>)> = Query::new(&world);
        assert_eq!(
            query.iter().count(),
            2,
            "незарегистрированный Maybe не должен опустошать запрос"
        );
    }

    // ── Or<> (W2-5) ────────────────────────────────────────────

    #[test]
    fn or_changed_matches_either_branch() {
        let mut world = World::new();
        let ea = world.spawn((Pos { x: 0.0 }, A));
        let eb = world.spawn((Pos { x: 0.0 }, B));
        let _ec = world.spawn((Pos { x: 0.0 },));

        #[derive(Debug)]
        struct Marker2(f32);
        impl Component for Marker2 {}
        let em = world.spawn((Pos { x: 0.0 }, Marker2(0.0)));

        world.tick();
        let last_run = world.current_tick();
        world.tick();

        // Мутируем Pos у ea и Marker2 у em — ловим Or<(Changed<Pos>, Changed<Marker2>)>.
        if let Some(p) = world.get_mut::<Pos>(ea) {
            p.x = 1.0;
        }
        if let Some(m) = world.get_mut::<Marker2>(em) {
            m.0 = 1.0;
        }

        let hits: Vec<_> = Query::<(
            Entity,
            Read<Pos>,
            Or<(Changed<Pos>, Changed<Marker2>)>,
        )>::new_with_tick(&world, last_run)
        .iter()
        .map(|(e, _, _)| e)
        .collect();

        assert!(hits.contains(&ea), "ветка Changed<Pos>");
        assert!(hits.contains(&em), "ветка Changed<Marker2>");
        assert!(!hits.contains(&eb), "B не менялся");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn or_with_matches_archetype_union() {
        let mut world = World::new();
        let ea = world.spawn((Pos { x: 0.0 }, A));
        let eb = world.spawn((Pos { x: 0.0 }, B));
        let none = world.spawn((Pos { x: 0.0 },));

        let hits: Vec<_> = Query::<(Entity, Read<Pos>, Or<(With<A>, With<B>)>)>::new(&world)
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert!(hits.contains(&ea) && hits.contains(&eb));
        assert!(!hits.contains(&none));
    }

    /// Ветка Or с незарегистрированным компонентом мертва, но НЕ опустошает
    /// запрос — другая ветка работает (и не падает на fetch_state).
    #[test]
    fn or_with_unregistered_branch_is_dead_not_fatal() {
        struct NeverRegistered;
        impl Component for NeverRegistered {}

        let mut world = World::new();
        let ea = world.spawn((Pos { x: 0.0 }, A));
        let _e = world.spawn((Pos { x: 0.0 },));

        let hits: Vec<_> =
            Query::<(Entity, Read<Pos>, Or<(With<NeverRegistered>, With<A>)>)>::new(&world)
                .iter()
                .map(|(e, _, _)| e)
                .collect();
        assert_eq!(hits, vec![ea]);
    }

    /// Or в CachedQuery: `(Or<(With<A>,)>, With<B>)` и `Or<(With<A>, With<B>)>`
    /// имеют одинаковые ids, но разную семантику — маркеры группы в ключе
    /// кэша обязаны разводить их по разным записям.
    #[test]
    fn or_cache_key_distinguishes_grouping() {
        let mut world = World::new();
        let _only_a = world.spawn((A,));
        let both = world.spawn((A, B));

        // (Or<(With<A>,)>, With<B>) ≡ With<A> AND With<B> → только both
        let strict: Vec<_> = world
            .query::<(Entity, Or<(With<A>,)>, With<B>)>()
            .iter()
            .map(|(e, _, _)| e)
            .collect();
        assert_eq!(strict, vec![both]);

        // Or<(With<A>, With<B>)> → обе entity
        let union_count = world.query::<Or<(With<A>, With<B>)>>().iter().count();
        assert_eq!(union_count, 2);
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
            .map(|(a, c)| (a.is_some(), c.is_some()))
            .collect();

        // Должно быть 2 entity
        assert_eq!(results.len(), 2);
        // e1: A=Some, C=Some
        assert!(results[0].0 && results[0].1);
        // e2: A=None, C=None
        assert!(!results[1].0 && !results[1].1);
    }
}
