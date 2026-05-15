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

use std::marker::PhantomData;
use crate::{
    access::AccessDescriptor,
    events::{EventCursor, Events, EventReadGuard},
    query::WorldQuery,
};

// ── Res / ResMut ───────────────────────────────────────────────

/// Иммутабельный доступ к ресурсу.
#[derive(Clone, Copy)]
pub struct Res<'w, T: Send + Sync + 'static>(pub &'w T);

impl<T: Send + Sync + 'static> std::ops::Deref for Res<'_, T> {
    type Target = T;
    #[inline] fn deref(&self) -> &T { self.0 }
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
    /// # Safety: ptr валиден на 'w, уникальный доступ гарантирован планировщиком.
    pub unsafe fn from_ptr(ptr: *mut T) -> Self {
        Self { ptr, _marker: PhantomData }
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for ResMut<'_, T> {
    type Target = T;
    #[inline] fn deref(&self) -> &T { unsafe { &*self.ptr } }
}

impl<T: Send + Sync + 'static> std::ops::DerefMut for ResMut<'_, T> {
    #[inline] fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.ptr } }
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
        unsafe { (self.ptr as *mut Events<T>).as_mut().unwrap().read(&self.cursor) }
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

/// Отправитель событий — мутабельный доступ к Events.
pub struct EventWriter<'w, T: Send + Sync + 'static> {
    ptr: *mut Events<T>,
    _marker: PhantomData<&'w mut Events<T>>,
}

impl<'w, T: Send + Sync + 'static> EventWriter<'w, T> {
    /// # Safety: ptr валиден на 'w, уникальный доступ гарантирован планировщиком.
    pub unsafe fn from_ptr(ptr: *mut Events<T>) -> Self {
        Self { ptr, _marker: PhantomData }
    }

    #[inline]
    pub fn send(&mut self, event: T) {
        unsafe { (*self.ptr).send(event); }
    }

    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        unsafe { (*self.ptr).send_batch(events); }
    }

    /// Предварительно выделить capacity для отправляемых событий.
    /// Позволяет избежать реаллокаций при массовой отправке.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        unsafe { (*self.ptr).reserve(additional); }
    }
}

unsafe impl<T: Send + Sync + 'static> Send for EventWriter<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for EventWriter<'_, T> {}

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

impl<T: Send + Sync + 'static> ResourceAccessList for ResWrite<T> {
    #[inline]
    fn resource_accesses() -> crate::access::AccessDescriptor {
        crate::access::AccessDescriptor::new().write::<T>()
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

impl_event_access_list_tuple!(A, B);
impl_event_access_list_tuple!(A, B, C);
impl_event_access_list_tuple!(A, B, C, D);
impl_event_access_list_tuple!(A, B, C, D, E);
impl_event_access_list_tuple!(A, B, C, D, E, F);
impl_event_access_list_tuple!(A, B, C, D, E, F, G);
impl_event_access_list_tuple!(A, B, C, D, E, F, G, H);

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

    fn run(&mut self, ctx: crate::world::SystemContext<'_>);

    fn name() -> &'static str where Self: Sized {
        std::any::type_name::<Self>()
    }
}
