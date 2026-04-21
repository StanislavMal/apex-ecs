//! SystemParam — типобезопасные обёртки для параметров систем.
//!
//! # Иерархия API (от простого к гибкому)
//!
//! ## 1. AutoSystem — рекомендуемый способ (автовывод access)
//!
//! Access выводится статически из `type Query`. Невозможно случайно
//! забыть компонент и получить молчаливый data race через unsafe в Column.
//!
//! ```ignore
//! struct MovementSystem;
//! impl AutoSystem for MovementSystem {
//!     type Query = (Read<Velocity>, Write<Position>);
//!     fn run(&mut self, ctx: SystemContext<'_>) {
//!         ctx.for_each_component::<Self::Query, _>(|(vel, pos)| {
//!             pos.x += vel.x * 0.016;
//!         });
//!     }
//! }
//! sched.add_auto_system("movement", MovementSystem);
//! ```
//!
//! ## 2. ParSystem — явный access (для сложных систем)
//!
//! Используй когда нужно несколько Query или доступ к ресурсам/событиям
//! которые не покрываются одним типом Query.
//!
//! ```ignore
//! struct PhysicsSystem;
//! impl ParSystem for PhysicsSystem {
//!     fn access() -> AccessDescriptor {
//!         AccessDescriptor::new()
//!             .read::<PhysicsConfig>()  // ресурс — нельзя вывести из Query
//!             .read::<Mass>()
//!             .write::<Velocity>()
//!             .write::<Position>()
//!     }
//!     fn run(&mut self, ctx: SystemContext<'_>) { ... }
//! }
//! ```
//!
//! ## 3. FnParSystem — замыкание с явным access
//!
//! ```ignore
//! sched.add_fn_par_system("ai", |ctx| { ... },
//!     AccessDescriptor::new().read::<Enemy>().write::<Velocity>()
//! );
//! ```
//!
//! ## 4. Sequential — полный &mut World
//!
//! ```ignore
//! sched.add_system("commands", |world: &mut World| { ... });
//! ```

use std::marker::PhantomData;
use crate::{
    access::AccessDescriptor,
    entity::Entity,
    events::EventQueue,
    query::{Query, WorldQuery},
    world::World,
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

/// Читатель событий — иммутабельный доступ к EventQueue.
pub struct EventReader<'w, T: Send + Sync + 'static>(pub &'w EventQueue<T>);

impl<'w, T: Send + Sync + 'static> EventReader<'w, T> {
    /// Итерация по событиям предыдущего тика (стандартный режим).
    #[inline] pub fn iter(&self) -> std::slice::Iter<'_, T> { self.0.iter_previous() }
    /// Итерация по событиям текущего тика (того же кадра).
    #[inline] pub fn iter_current(&self) -> std::slice::Iter<'_, T> { self.0.iter_current() }
    /// Все события: текущий + предыдущий тик.
    #[inline] pub fn iter_all(&self) -> impl Iterator<Item = &T> { self.0.iter_all() }
    #[inline] pub fn len(&self) -> usize { self.0.len_previous() }
    #[inline] pub fn is_empty(&self) -> bool { self.0.len_previous() == 0 }
}

/// Отправитель событий — мутабельный доступ к EventQueue.
pub struct EventWriter<'w, T: Send + Sync + 'static> {
    ptr: *mut EventQueue<T>,
    _marker: PhantomData<&'w mut EventQueue<T>>,
}

impl<'w, T: Send + Sync + 'static> EventWriter<'w, T> {
    /// # Safety: ptr валиден на 'w, уникальный доступ гарантирован планировщиком.
    pub unsafe fn from_ptr(ptr: *mut EventQueue<T>) -> Self {
        Self { ptr, _marker: PhantomData }
    }

    #[inline]
    pub fn send(&mut self, event: T) {
        unsafe { (*self.ptr).send(event); }
    }

    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        unsafe { (*self.ptr).send_batch(events); }
    }
}

unsafe impl<T: Send + Sync + 'static> Send for EventWriter<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for EventWriter<'_, T> {}

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
///     ctx.for_each_component::<(Read<Velocity>, Write<Position>), _>(...)
///     //                                        ^^^^^^^^^^^^^^^ пишем, но не декларировали
/// }
/// ```
///
/// `AutoSystem` устраняет этот класс багов: access выводится из `type Query`
/// статически во время компиляции.
///
/// # Ограничения
///
/// Если система:
/// - использует несколько разных Query
/// - обращается к ресурсам через `ctx.resource_mut::<T>()`
/// - читает/пишет события
///
/// ...то access не полностью покрывается одним типом Query. В этом случае
/// используй `ParSystem` с явным `AccessDescriptor` который включает
/// ВСЕ компоненты И ресурсы И события.
///
/// # Пример
///
/// ```ignore
/// struct MovementSystem;
///
/// impl AutoSystem for MovementSystem {
///     // Планировщик автоматически получит:
///     //   reads: [TypeId::of::<Velocity>()]
///     //   writes: [TypeId::of::<Position>()]
///     type Query = (Read<Velocity>, Write<Position>);
///
///     fn run(&mut self, ctx: SystemContext<'_>) {
///         ctx.for_each_component::<Self::Query, _>(|(vel, pos)| {
///             pos.x += vel.x * 0.016;
///             pos.y += vel.y * 0.016;
///         });
///     }
/// }
///
/// // Регистрация через специализированный метод планировщика
/// sched.add_auto_system("movement", MovementSystem);
/// ```
pub trait AutoSystem: Send + Sync {
    /// Тип запроса, из которого выводится `AccessDescriptor`.
    ///
    /// Должен реализовывать `WorldQuerySystemAccess` (что автоматически
    /// выполняется для всех стандартных комбинаций Read/Write/With/Without).
    type Query: WorldQuery + WorldQuerySystemAccess;

    fn run(&mut self, ctx: crate::world::SystemContext<'_>);

    /// Имя системы для диагностики и `debug_plan()`.
    fn name() -> &'static str where Self: Sized {
        std::any::type_name::<Self>()
    }
}