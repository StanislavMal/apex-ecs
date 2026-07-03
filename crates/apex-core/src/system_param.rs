//! SystemParam — типобезопасные обёртки для параметров систем.
//!
//! # Иерархия API (от простого к гибкому)
//!
//! ## 1. AutoSystem — рекомендуемый способ (автовывод access)
//!
//! Access выводится статически из `type Query`, `type Resources` и `type Events`.
//! Невозможно случайно забыть компонент, ресурс или событие.
//!
//! ```ignore
//! struct MovementSystem;
//! impl AutoSystem for MovementSystem {
//!     type Query = (Read<Velocity>, Write<Position>);
//!     type Resources = ();
//!     type Events = ();
//!     fn run(&mut self, ctx: SystemContext<'_>) {
//!         ctx.query::<Self::Query>().for_each(|_, (vel, pos)| {
//!             pos.x += vel.x * 0.016;
//!         });
//!     }
//! }
//! sched.add_auto_system("movement", MovementSystem);
//! ```
//!
//! ```ignore
//! struct PhysicsSystem;
//! impl AutoSystem for PhysicsSystem {
//!     type Query     = (Read<Mass>, Write<Velocity>, Write<Position>);
//!     type Resources = (ResRead<PhysicsConfig>, ResRead<DeltaTime>);
//!     type Events    = Emit<CollisionEvent>;
//!     fn run(&mut self, ctx: SystemContext<'_>) {
//!         let cfg = ctx.resource::<PhysicsConfig>();
//!         let mut writer = ctx.event_writer::<CollisionEvent>();
//!         ctx.query::<Self::Query>().for_each(|entity, (mass, vel, pos)| {
//!             vel.y -= cfg.gravity * mass.0 * cfg.dt;
//!             pos.x += vel.x * cfg.dt;
//!             if pos.y < 0.0 { writer.send(CollisionEvent { entity }); }
//!         });
//!     }
//! }
//! ```
//!
//! ## 2. FnParSystem — замыкание с явным access
//!
//! ```ignore
//! sched.add_fn_par_system("ai", |ctx| { ... },
//!     AccessDescriptor::new().read::<Enemy>().write::<Velocity>()
//! );
//! ```
//!
//! ## 3. Sequential — полный &mut World
//!
//! ```ignore
//! sched.add_system("commands", |world: &mut World| { ... });
//! ```

use crate::{
    access::AccessDescriptor,
    events::{EventCursor, EventReadGuard, Events},
    query::WorldQuery,
};
use std::marker::PhantomData;

// ── Res / ResMut ───────────────────────────────────────────────

/// Иммутабельный доступ к ресурсу.
///
/// Поле скрыто (D2-1): публичный `.0` ПЕРЕКРЫВАЛ Deref — `res.0` на ресурсе-
/// кортеже выдавал `&T` вместо поля ресурса (футган для Bevy-мигранта).
/// Доступ — через Deref (`*res`, `res.field`) или [`into_inner`](Self::into_inner).
#[derive(Clone, Copy)]
pub struct Res<'w, T: Send + Sync + 'static>(pub(crate) &'w T);

impl<'w, T: Send + Sync + 'static> Res<'w, T> {
    /// Сконструировать из ссылки (низкоуровневый API планировщика/мостов).
    #[inline]
    pub fn new(value: &'w T) -> Self {
        Self(value)
    }

    /// Извлечь `&T` с полным лайфтаймом мира.
    #[inline]
    pub fn into_inner(self) -> &'w T {
        self.0
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for Res<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        self.0
    }
}

impl<T: Send + Sync + 'static + std::fmt::Debug> std::fmt::Debug for Res<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Res({:?})", self.0)
    }
}

/// Мутабельный доступ к ресурсу.
pub struct ResMut<'w, T: Send + Sync + 'static> {
    ptr: *mut T,
    _marker: PhantomData<&'w mut T>,
}

impl<'w, T: Send + Sync + 'static> ResMut<'w, T> {
    /// # Safety
    /// `ptr` is valid for `'w`; the scheduler guarantees exclusive access.
    pub unsafe fn from_ptr(ptr: *mut T) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for ResMut<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.ptr }
    }
}

impl<T: Send + Sync + 'static> std::ops::DerefMut for ResMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.ptr }
    }
}

unsafe impl<T: Send + Sync + 'static> Send for ResMut<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for ResMut<'_, T> {}

// ── EventReader / EventWriter ──────────────────────────────────

/// Читатель событий — использует per-reader курсор.
pub struct EventReader<'w, T: Send + Sync + 'static> {
    /// Сырой указатель для возможности мутабельного доступа через `read()`.
    ptr: *const Events<T>,
    cursor: EventCursor,
    _marker: PhantomData<&'w Events<T>>,
}

impl<'w, T: Send + Sync + 'static> EventReader<'w, T> {
    /// Создать читателя с новым курсором.
    /// # Panics
    /// Паникует если события типа T не зарегистрированы.
    pub fn new(events: &'w mut Events<T>) -> Self {
        let cursor = events.add_reader();
        Self {
            ptr: events as *const Events<T>,
            cursor,
            _marker: PhantomData,
        }
    }

    /// Итерация по непрочитанным событиям.
    #[inline]
    pub fn iter(&self) -> &[T] {
        unsafe { (*self.ptr).iter(&self.cursor) }
    }

    /// Прочитать и автоматически продвинуть курсор (RAII).
    #[inline]
    pub fn read(&mut self) -> EventReadGuard<'_, T> {
        unsafe {
            (self.ptr as *mut Events<T>)
                .as_mut()
                .unwrap()
                .read(&self.cursor)
        }
    }

    /// Количество непрочитанных событий.
    #[inline]
    pub fn len(&self) -> usize {
        self.iter().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.iter().is_empty()
    }
}

impl<T: Send + Sync + 'static> Drop for EventReader<'_, T> {
    fn drop(&mut self) {
        unsafe {
            let events = self.ptr as *mut Events<T>;
            (*events).remove_reader(self.cursor);
        }
    }
}

/// Отправитель событий — мутабельный доступ к Events.
pub struct EventWriter<'w, T: Send + Sync + 'static> {
    ptr: *mut Events<T>,
    _marker: PhantomData<&'w mut Events<T>>,
}

impl<'w, T: Send + Sync + 'static> EventWriter<'w, T> {
    /// # Safety
    /// `ptr` is valid for `'w`; the scheduler guarantees exclusive access.
    pub unsafe fn from_ptr(ptr: *mut Events<T>) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn send(&mut self, event: T) {
        unsafe {
            (*self.ptr).send(event);
        }
    }

    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        unsafe {
            (*self.ptr).send_batch(events);
        }
    }

    /// Предварительно выделить capacity для отправляемых событий.
    /// Позволяет избежать реаллокаций при массовой отправке.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        unsafe {
            (*self.ptr).reserve(additional);
        }
    }
}

unsafe impl<T: Send + Sync + 'static> Send for EventWriter<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for EventWriter<'_, T> {}

/// Bevy-совместимое имя для чтения удалений компонента `T` (D2-3):
/// обычный [`EventReader`] событий [`Removed<T>`](crate::events::Removed).
///
/// Требует включённого трекинга — `world.track_removals::<T>()` при setup'е
/// (в отличие от Bevy, где удаления пишутся для всех компонентов всегда,
/// у нас это opt-in с нулевой стоимостью по умолчанию).
///
/// ```ignore
/// fn cleanup(mut removed: RemovedComponents<PhysicsBody>, mut phys: ResMut<Physics>) {
///     for r in removed.read() { phys.remove_body(r.entity); }
/// }
/// ```
pub type RemovedComponents<'w, T> = EventReader<'w, crate::events::Removed<T>>;

// ── Маркеры для ResourceAccessList ────────────────────────────

/// Маркер: read-доступ к ресурсу T в `AutoSystem::Resources`.
///
/// Не путать с runtime-обёрткой `Res<'w, T>` — это только статическое
/// описание доступа для планировщика.
pub struct ResRead<T: Send + Sync + 'static>(PhantomData<T>);

/// Маркер: write-доступ к ресурсу T в `AutoSystem::Resources`.
pub struct ResWrite<T: Send + Sync + 'static>(PhantomData<T>);

// ── Маркеры для EventAccessList ────────────────────────────────

/// Маркер: подписка на события типа E в `AutoSystem::Events`.
///
/// Соответствует `ctx.event_reader::<E>()` внутри `run()`.
pub struct Listen<E: Send + Sync + 'static>(PhantomData<E>);

/// Маркер: публикация событий типа E в `AutoSystem::Events`.
///
/// Соответствует `ctx.event_writer::<E>()` внутри `run()`.
pub struct Emit<E: Send + Sync + 'static>(PhantomData<E>);

// ── ResourceAccessList ─────────────────────────────────────────

/// Статическое описание доступа к ресурсам — используется в `AutoSystem::Resources`.
///
/// Реализован для:
/// - `()` — нет доступа к ресурсам (дефолт)
/// - `ResRead<T>` — read-доступ к ресурсу T
/// - `ResWrite<T>` — write-доступ к ресурсу T
/// - кортежи из вышеперечисленных (до 8 элементов)
pub trait ResourceAccessList {
    fn resource_accesses() -> crate::access::AccessDescriptor;
}

impl ResourceAccessList for () {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new()
    }
}

impl<T: Send + Sync + 'static> ResourceAccessList for ResRead<T> {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().read::<T>()
    }
}

// NB: write-доступ к ресурсу ставит `resource_write()` (гейт ASD, TD-37).
impl<T: Send + Sync + 'static> ResourceAccessList for ResWrite<T> {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().write::<T>().resource_write()
    }
}

macro_rules! impl_resource_access_list_tuple {
    ( $($R:ident),+ ) => {
        impl< $($R: ResourceAccessList),+ > ResourceAccessList for ( $($R,)+ ) {
            fn resource_accesses() -> crate::access::AccessDescriptor {
                crate::access::AccessDescriptor::new()
                    $( .merge(&$R::resource_accesses()) )+
            }
        }
    };
}

impl_resource_access_list_tuple!(A);
impl_resource_access_list_tuple!(A, B);
impl_resource_access_list_tuple!(A, B, C);
impl_resource_access_list_tuple!(A, B, C, D);
impl_resource_access_list_tuple!(A, B, C, D, E);
impl_resource_access_list_tuple!(A, B, C, D, E, F);
impl_resource_access_list_tuple!(A, B, C, D, E, F, G);
impl_resource_access_list_tuple!(A, B, C, D, E, F, G, H);

// ── EventAccessList ────────────────────────────────────────────

/// Статическое описание доступа к событиям — используется в `AutoSystem::Events`.
///
/// Реализован для:
/// - `()` — нет доступа к событиям (дефолт)
/// - `Listen<E>` — подписка на события E (read_event)
/// - `Emit<E>`   — публикация событий E  (write_event)
/// - кортежи из вышеперечисленных (до 8 элементов)
pub trait EventAccessList {
    fn event_accesses() -> crate::access::AccessDescriptor;
}

impl EventAccessList for () {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new()
    }
}

impl<E: Send + Sync + 'static> EventAccessList for Listen<E> {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().read_event::<E>()
    }
}

impl<E: Send + Sync + 'static> EventAccessList for Emit<E> {
    #[inline]
    fn event_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().write_event::<E>()
    }
}

macro_rules! impl_event_access_list_tuple {
    ( $($E:ident),+ ) => {
        impl< $($E: EventAccessList),+ > EventAccessList for ( $($E,)+ ) {
            fn event_accesses() -> crate::access::AccessDescriptor {
                crate::access::AccessDescriptor::new()
                    $( .merge(&$E::event_accesses()) )+
            }
        }
    };
}

impl_event_access_list_tuple!(A);
impl_event_access_list_tuple!(A, B);
impl_event_access_list_tuple!(A, B, C);
impl_event_access_list_tuple!(A, B, C, D);
impl_event_access_list_tuple!(A, B, C, D, E);
impl_event_access_list_tuple!(A, B, C, D, E, F);
impl_event_access_list_tuple!(A, B, C, D, E, F, G);
impl_event_access_list_tuple!(A, B, C, D, E, F, G, H);

// ── SystemParam ────────────────────────────────────────────────

/// Типобезопасное извлечение параметров системы из `SystemContext`.
///
/// Аналог Bevy `SystemParam`, но БЕЗ разделения State/Fetch,
/// БЕЗ proc-макросов, БЕЗ изменения существующих макросов `system!`/`sequential_system!`.
///
/// # Примеры
///
/// ```ignore
/// // Один ресурс
/// type MyParam = ResRead<DeltaTime>;
/// let dt = MyParam::fetch(&ctx);
///
/// // Кортеж: ресурс + запрос + события
/// type MyParam = (ResRead<DeltaTime>, QueryParam<(Read<Vel>, Write<Pos>)>, Listen<CollisionEvent>);
/// let (dt, q, events) = MyParam::fetch(&ctx);
/// ```
///
/// # Зачем
///
/// Упрощает портирование Bevy-рендера (где `RenderCommand::Param: SystemParam`)
/// и устраняет бойлерплейт в sequential системах.
pub trait SystemParam {
    /// Что возвращается при [`fetch`](SystemParam::fetch) (с лайфтаймом `'w`).
    type Item<'w>;

    /// Статическая декларация доступа для планировщика.
    fn access() -> AccessDescriptor;

    /// Извлечь значение из контекста системы.
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w>;

    /// Параметр использует отложенные команды (`Commands`) — планировщик
    /// вставляет auto-apply sync-точку после системы (D2-1).
    fn has_deferred() -> bool {
        false
    }

    /// Валидация перед запуском системы (Э5): `false` ⇒ система ПРОПУСКАЕТСЯ
    /// в этом кадре (skip-семантика `Single<Q>`; Bevy `validate_param`).
    /// Вызывается планировщиком непосредственно перед [`fetch`](Self::fetch).
    fn validate(_ctx: &crate::world::SystemContext<'_>) -> bool {
        true
    }
}

// ── impl SystemParam для маркеров ресурсов ────────────────────

impl<T: Send + Sync + 'static> SystemParam for ResRead<T> {
    type Item<'w> = Res<'w, T>;
    fn access() -> AccessDescriptor {
        <Self as ResourceAccessList>::resource_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Res<'w, T> {
        ctx.resource::<T>()
    }
}

impl<T: Send + Sync + 'static> SystemParam for ResWrite<T> {
    type Item<'w> = ResMut<'w, T>;
    fn access() -> AccessDescriptor {
        <Self as ResourceAccessList>::resource_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> ResMut<'w, T> {
        ctx.resource_mut::<T>()
    }
}

// ── impl SystemParam для маркеров событий ─────────────────────

impl<E: Send + Sync + 'static> SystemParam for Listen<E> {
    type Item<'w> = EventReader<'w, E>;
    fn access() -> AccessDescriptor {
        <Self as EventAccessList>::event_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventReader<'w, E> {
        ctx.event_reader::<E>()
    }
}

impl<E: Send + Sync + 'static> SystemParam for Emit<E> {
    type Item<'w> = EventWriter<'w, E>;
    fn access() -> AccessDescriptor {
        <Self as EventAccessList>::event_accesses()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventWriter<'w, E> {
        ctx.event_writer::<E>()
    }
}

// ── Маркер QueryParam ──────────────────────────────────────────

/// Маркер: параметр-запрос компонентов через [`SystemParam`].
///
/// ```ignore
/// type MyParams = QueryParam<(Read<Position>, Write<Velocity>)>;
/// let q = MyParams::fetch(&ctx);
/// q.for_each(|entity, (pos, vel)| { ... });
/// ```
pub struct QueryParam<Q: WorldQuery>(PhantomData<Q>);

impl<Q: WorldQuery + WorldQuerySystemAccess> SystemParam for QueryParam<Q> {
    type Item<'w> = crate::world::CachedQuery<'w, Q>;
    fn access() -> AccessDescriptor {
        Q::system_access()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> crate::world::CachedQuery<'w, Q> {
        ctx.query::<Q>()
    }
}

// ── Маркер CommandsParam ───────────────────────────────────────

/// Маркер: параметр-доступ к [`Commands`](crate::commands::Commands).
///
/// ```ignore
/// type MyParams = CommandsParam;
/// let cmds = MyParams::fetch(&ctx);
/// cmds.spawn((Position { x: 0.0 }, Velocity { x: 1.0 }));
/// ```
pub struct CommandsParam;

impl SystemParam for CommandsParam {
    type Item<'w> = &'w mut crate::commands::Commands;
    fn access() -> AccessDescriptor {
        // `commands_used` гейтит ASD: не-entity-локальные команды дублировались бы по чанкам.
        AccessDescriptor::new().commands_used()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> &'w mut crate::commands::Commands {
        ctx.commands()
    }
    fn has_deferred() -> bool {
        true
    }
}

// ── Bevy-style параметры plain-fn систем (D2-1) ────────────────
//
// В plain-fn пути (см. [`SystemParamFunction`](crate::fn_system::SystemParamFunction))
// параметром служит САМ пользовательский тип: `Res<T>`, `ResMut<T>`,
// `Query<Q>`, `EventReader<E>`, `EventWriter<E>`, `&mut Commands` — семантика
// Bevy 1:1 (в отличие от `system!`, где `&T` означает ресурс).
// Item обязан быть тем же конструктором типа, что и параметр (двойной
// Fn-баунд SystemParamFunction связывает обе формы).

impl<'a, T: Send + Sync + 'static> SystemParam for Res<'a, T> {
    type Item<'w> = Res<'w, T>;
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Res<'w, T> {
        ctx.resource::<T>()
    }
}

impl<'a, T: Send + Sync + 'static> SystemParam for ResMut<'a, T> {
    type Item<'w> = ResMut<'w, T>;
    fn access() -> AccessDescriptor {
        // `write::<T>()` — для конфликт-анализа (две `ResMut<T>` не параллелятся);
        // `resource_write()` — гейт ASD: систему с мутацией ресурса нельзя дробить на чанки
        // (тело выполнялось бы раз на чанк ⇒ мутация × число чанков). См. TD-37.
        AccessDescriptor::new().write::<T>().resource_write()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> ResMut<'w, T> {
        ctx.resource_mut::<T>()
    }
}

impl<'a, Q, F> SystemParam for crate::query::Query<'a, Q, F>
where
    Q: WorldQuery + WorldQuerySystemAccess,
    F: WorldQuery + WorldQuerySystemAccess,
{
    type Item<'w> = crate::query::Query<'w, Q, F>;
    fn access() -> AccessDescriptor {
        Q::system_access().merge(&F::system_access())
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> crate::query::Query<'w, Q, F> {
        // База Changed/Added — last_run_tick мира (граница прошлого кадра),
        // как у ctx.query() (TD-9).
        let sub = &ctx.sub_worlds[0];
        let last_run = sub.world().last_run_tick();
        // SAFETY: `ctx` (and its SubWorlds) is vended by the scheduler for a
        // system whose declared access (`Self::access()`) covers exactly this
        // shape; conflicting systems never run concurrently.
        unsafe { crate::query::Query::from_sub_world(sub, last_run) }
    }
}

/// `Single<Q, F>` — ровно один матч; система пропускается при 0 или >1 (Э5).
impl<'a, Q, F> SystemParam for crate::query::Single<'a, Q, F>
where
    Q: WorldQuery + WorldQuerySystemAccess,
    F: WorldQuery + WorldQuerySystemAccess,
{
    type Item<'w> = crate::query::Single<'w, Q, F>;
    fn access() -> AccessDescriptor {
        Q::system_access().merge(&F::system_access())
    }
    fn validate(ctx: &crate::world::SystemContext<'_>) -> bool {
        let q = <crate::query::Query<'_, Q, F> as SystemParam>::fetch(ctx);
        q.iter().take(2).count() == 1
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w> {
        let q = <crate::query::Query<'w, Q, F> as SystemParam>::fetch(ctx);
        match q.single_inner() {
            Ok((entity, item)) => crate::query::Single {
                entity,
                item,
                _filter: std::marker::PhantomData,
            },
            // Недостижимо после validate (планировщик зовёт validate→fetch
            // атомарно в рамках слота системы); паника — для ручных вызовов
            // fetch без validate.
            Err(e) => panic!(
                "Single<…>: запрос дал не ровно один матч ({e:?});                  системы с Single пропускаются планировщиком"
            ),
        }
    }
}

/// `Option<Single<Q, F>>` — `None` при нуле матчей; пропуск только при >1 (Э5).
impl<'a, Q, F> SystemParam for Option<crate::query::Single<'a, Q, F>>
where
    Q: WorldQuery + WorldQuerySystemAccess,
    F: WorldQuery + WorldQuerySystemAccess,
{
    type Item<'w> = Option<crate::query::Single<'w, Q, F>>;
    fn access() -> AccessDescriptor {
        Q::system_access().merge(&F::system_access())
    }
    fn validate(ctx: &crate::world::SystemContext<'_>) -> bool {
        let q = <crate::query::Query<'_, Q, F> as SystemParam>::fetch(ctx);
        q.iter().take(2).count() <= 1
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w> {
        let q = <crate::query::Query<'w, Q, F> as SystemParam>::fetch(ctx);
        match q.single_inner() {
            Ok((entity, item)) => Some(crate::query::Single {
                entity,
                item,
                _filter: std::marker::PhantomData,
            }),
            Err(crate::query::QuerySingleError::NoEntities) => None,
            Err(e) => panic!(
                "Option<Single<…>>: больше одного матча ({e:?});                  системы пропускаются планировщиком"
            ),
        }
    }
}

impl<'a, Q: WorldQuery + WorldQuerySystemAccess> SystemParam for crate::world::CachedQuery<'a, Q> {
    type Item<'w> = crate::world::CachedQuery<'w, Q>;
    fn access() -> AccessDescriptor {
        Q::system_access()
    }
    fn fetch<'w>(
        ctx: &'w crate::world::SystemContext<'w>,
    ) -> crate::world::CachedQuery<'w, Q> {
        ctx.query::<Q>()
    }
}

impl<'a, E: Send + Sync + 'static> SystemParam for EventReader<'a, E> {
    type Item<'w> = EventReader<'w, E>;
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().read_event::<E>()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventReader<'w, E> {
        ctx.event_reader::<E>()
    }
}

impl<'a, E: Send + Sync + 'static> SystemParam for EventWriter<'a, E> {
    type Item<'w> = EventWriter<'w, E>;
    fn access() -> AccessDescriptor {
        AccessDescriptor::new().write_event::<E>()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventWriter<'w, E> {
        ctx.event_writer::<E>()
    }
}

impl SystemParam for &mut crate::commands::Commands {
    type Item<'w> = &'w mut crate::commands::Commands;
    fn access() -> AccessDescriptor {
        // `commands_used` гейтит ASD: не-entity-локальные команды дублировались бы по чанкам.
        AccessDescriptor::new().commands_used()
    }
    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> &'w mut crate::commands::Commands {
        ctx.commands()
    }
    fn has_deferred() -> bool {
        true
    }
}

// ── impl SystemParam для () (нет параметров) ──────────────────

impl SystemParam for () {
    type Item<'w> = ();
    fn access() -> AccessDescriptor {
        AccessDescriptor::new()
    }
    fn fetch<'w>(_ctx: &crate::world::SystemContext<'w>) {}
}

// ── Кортежи SystemParam (1..12) ───────────────────────────────

macro_rules! impl_system_param_tuple {
    ( $($P:ident),+ ) => {
        impl< $($P: SystemParam),+ > SystemParam for ( $($P,)+ ) {
            type Item<'w> = ( $($P::Item<'w>,)+ );
            fn access() -> AccessDescriptor {
                AccessDescriptor::new()
                    $( .merge(&$P::access()) )+
            }
            fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Self::Item<'w> {
                ( $($P::fetch(ctx),)+ )
            }
            fn has_deferred() -> bool {
                false $( || $P::has_deferred() )+
            }
            fn validate(ctx: &crate::world::SystemContext<'_>) -> bool {
                true $( && $P::validate(ctx) )+
            }
        }
    };
}

impl_system_param_tuple!(A);
impl_system_param_tuple!(A, B);
impl_system_param_tuple!(A, B, C);
impl_system_param_tuple!(A, B, C, D);
impl_system_param_tuple!(A, B, C, D, E);
impl_system_param_tuple!(A, B, C, D, E, F);
impl_system_param_tuple!(A, B, C, D, E, F, G);
impl_system_param_tuple!(A, B, C, D, E, F, G, H);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_system_param_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

// ── WorldQuerySystemAccess ─────────────────────────────────────

/// Расширение WorldQuery — статическое описание R/W доступа для планировщика.
///
/// Реализовано для Read<T>, Write<T>, With<T>, Without<T>, Changed<T>
/// и кортежей из них в query.rs.
///
/// Является основой для `AutoSystem::access()` — позволяет планировщику
/// получить `AccessDescriptor` без ручного перечисления компонентов.
pub trait WorldQuerySystemAccess: WorldQuery {
    fn system_access() -> AccessDescriptor;
}

// ── AutoSystem ─────────────────────────────────────────────────

/// Параллельная система с автоматическим выводом AccessDescriptor.
///
/// # Мотивация
///
/// При использовании `ParSystem` с явным `AccessDescriptor` есть риск
/// забыть задекларировать компонент:
///
/// ```ignore
/// // БАГИ: Write<Position> не указан — планировщик не видит конфликт
/// fn access() -> AccessDescriptor {
///     AccessDescriptor::new().read::<Velocity>() // забыли write::<Position>()
/// }
/// fn run(&mut self, ctx: SystemContext<'_>) {
///     ctx.for_each::<(Read<Velocity>, Write<Position>), _>(...)
///     //                                        ^^^^^^^^^^^^^^^ пишем, но не декларировали
/// }
/// ```
///
/// `AutoSystem` устраняет этот класс багов: access выводится из `type Query`
/// статически во время компиляции.
///
/// # Ресурсы и события
///
/// Если системе нужен доступ к ресурсам или событиям, укажи их в
/// ассоциированных типах `Resources` и `Events`:
///
/// # Примеры
///
/// ```ignore
/// // Только компоненты
/// struct MovementSystem;
/// impl AutoSystem for MovementSystem {
///     type Query = (Read<Velocity>, Write<Position>);
///     fn run(&mut self, ctx: SystemContext<'_>) {
///         ctx.query::<Self::Query>().for_each(|_, (vel, pos)| {
///             pos.x += vel.x * 0.016;
///         });
///     }
/// }
///
/// // Компоненты + ресурсы + события
/// struct PhysicsSystem;
/// impl AutoSystem for PhysicsSystem {
///     type Query     = (Read<Mass>, Write<Velocity>, Write<Position>);
///     type Resources = ResRead<DeltaTime>;
///     type Events    = Emit<CollisionEvent>;
///     fn run(&mut self, ctx: SystemContext<'_>) {
///         let dt = ctx.resource::<DeltaTime>().0;
///         let mut writer = ctx.event_writer::<CollisionEvent>();
///         ctx.query::<Self::Query>().for_each(|entity, (mass, vel, pos)| {  });
///     }
/// }
///
/// ```
pub trait AutoSystem: Send + Sync {
    /// Компонентный запрос — из него выводится часть `AccessDescriptor`.
    type Query: WorldQuery + WorldQuerySystemAccess;

    /// Ресурсы, которые нужны системе.
    type Resources: ResourceAccessList;

    /// События, которые система читает или пишет.
    type Events: EventAccessList;

    /// Системе нужны ВСЕ entity (глобальный доступ).
    /// ASD-чанкование запрещено, система всегда получает полный SubWorld.
    /// По умолчанию `false`.
    const NEEDS_WHOLE_WORLD: bool = false;

    /// Система использует Commands (отложенные операции).
    /// Устанавливается `system!` макросом автоматически при обнаружении `cmd: Cmd`.
    /// Позволяет `compile()` вставлять auto-apply sync points.
    const HAS_DEFERRED: bool = false;

    fn run(&mut self, ctx: crate::world::SystemContext<'_>);

    fn name() -> &'static str
    where
        Self: Sized,
    {
        std::any::type_name::<Self>()
    }
}

/// Эксклюзивная система — получает полный `&mut World`.
///
/// Генерируется тем же макросом [`system!`](crate::system), когда среди
/// параметров есть `world: &mut World`. Такая система объявляет **FULL access**
/// (конфликтует со всем) и исполняется планировщиком в одиночку (sync-точка),
/// между параллельными батчами. `world: &mut World` нельзя комбинировать с
/// другими data-параметрами — макрос проверяет это и даёт compile-ошибку.
///
/// Это замена удалённого `sequential_system!`: один макрос `system!` для
/// параллельных и эксклюзивных систем, режим выбирается по наличию `&mut World`.
pub trait ExclusiveSystem: Send + 'static {
    fn run(&mut self, world: &mut crate::world::World);

    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// Любое замыкание/функция `FnMut(&mut World)` — это эксклюзивная система.
///
/// Унифицирует регистрацию: `add_exclusive_system` / `add_systems` принимают как
/// struct-маркеры из `system!`, так и обычные `fn(&mut World)` (например,
/// `propagate_transforms`) и инлайн-замыкания — единый профессиональный путь.
/// Конфликта с ручными `impl ExclusiveSystem` из макроса нет: struct-маркеры не
/// реализуют `FnMut`.
impl<F> ExclusiveSystem for F
where
    F: FnMut(&mut crate::world::World) + Send + 'static,
{
    #[inline]
    fn run(&mut self, world: &mut crate::world::World) {
        self(world)
    }
}

// ── Extract<P> — Bevy-совместимый SystemParam для extract-систем ──

/// Bevy-совместимый параметр extract-систем — читает из [`MainWorld`].
///
/// Во время extract-стадии render-мир содержит временный ресурс `MainWorld`.
/// `Extract<P>` прозрачно применяет внутренний `SystemParam P` к этому миру,
/// а не к render-миру.
///
/// # Пример
///
/// ```ignore
/// system! {
///     fn extract_cameras(
///         q: &Extract<QueryParam<(Read<Camera>, Read<GlobalTransform>)>>,
///         out: &mut ExtractedCamera,
///     ) {
///         for (_, (cam, transform)) in q.iter() {
///             *out = ExtractedCamera::new(cam, transform);
///         }
///     }
/// }
/// ```
///
/// После extract-стадии `MainWorld` удаляется из render-мира и возвращается main-потоку.
pub struct Extract<P>(PhantomData<P>);

// Extract<QueryParam<Q>> — читает компоненты из MainWorld. Форма обязана
// быть read-only: extract по контракту НЕ пишет в main-мир (write-форма
// теперь и не скомпилируется — ReadOnlyWorldQuery).
impl<Q: WorldQuery + WorldQuerySystemAccess + crate::query::ReadOnlyWorldQuery> SystemParam
    for Extract<QueryParam<Q>>
{
    type Item<'w> = crate::world::CachedQuery<'w, Q>;

    fn access() -> AccessDescriptor {
        Q::system_access()
    }

    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> crate::world::CachedQuery<'w, Q> {
        let mw: Res<'w, crate::world::MainWorld> = ctx.resource();
        // Res.0 is &'w MainWorld; MainWorld.world() borrows and returns &'w World
        crate::world::CachedQuery::new(mw.0.world(), crate::component::Tick(0))
    }
}

// Extract<ResRead<T>> — читает ресурс из MainWorld
impl<T: Send + Sync + 'static> SystemParam for Extract<ResRead<T>> {
    type Item<'w> = Res<'w, T>;

    fn access() -> AccessDescriptor {
        <ResRead<T> as ResourceAccessList>::resource_accesses()
    }

    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> Res<'w, T> {
        let mw: Res<'w, crate::world::MainWorld> = ctx.resource();
        let world: &crate::world::World = mw.0.world();
        Res(world.resource::<T>())
    }
}

// Extract<Listen<E>> — читает события из MainWorld
impl<E: Send + Sync + 'static> SystemParam for Extract<Listen<E>> {
    type Item<'w> = EventReader<'w, E>;

    fn access() -> AccessDescriptor {
        <Listen<E> as EventAccessList>::event_accesses()
    }

    fn fetch<'w>(ctx: &'w crate::world::SystemContext<'w>) -> EventReader<'w, E> {
        let mw: Res<'w, crate::world::MainWorld> = ctx.resource();
        let world: &crate::world::World = mw.0.world();
        world.event_reader::<E>()
    }
}
