//! SystemParam — типобезопасные обёртки для параметров систем.
//!
//! Используются внутри `SystemContext` и напрямую в функциях-системах
//! через `add_fn_par_system`.
//!
//! # API
//!
//! ## ParSystem trait (struct-based, явный access)
//! ```ignore
//! struct PhysicsSystem;
//! impl ParSystem for PhysicsSystem {
//!     fn access() -> AccessDescriptor {
//!         AccessDescriptor::new().read::<Mass>().write::<Velocity>().write::<Position>()
//!     }
//!     fn run(&mut self, ctx: SystemContext<'_>) {
//!         let dt = ctx.resource::<PhysicsConfig>().dt;
//!         ctx.query::<(Read<Mass>, Write<Velocity>, Write<Position>)>()
//!            .for_each_component(|(mass, vel, pos)| { ... });
//!     }
//! }
//! sched.add_par_system("physics", PhysicsSystem);
//! ```
//!
//! ## FnParSystem (fn-based, явный access через builder)
//! ```ignore
//! fn physics(ctx: SystemContext<'_>) {
//!     let dt = ctx.resource::<PhysicsConfig>().dt;
//!     ctx.query::<(Read<Mass>, Write<Velocity>, Write<Position>)>()
//!        .for_each_component(|(mass, vel, pos)| { ... });
//! }
//!
//! sched.add_fn_par_system("physics", physics,
//!     AccessDescriptor::new()
//!         .read::<Mass>()
//!         .write::<Velocity>()
//!         .write::<Position>()
//! );
//! ```
//!
//! ## Sequential система (полный &mut World)
//! ```ignore
//! sched.add_system("commands", |world: &mut World| {
//!     // полный доступ
//! });
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
/// Используется в `SystemContext::resource::<T>()`.
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
/// Используется в `SystemContext::resource_mut::<T>()`.
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

// SAFETY: уникальный доступ гарантирован AccessDescriptor + compile().
unsafe impl<T: Send + Sync + 'static> Send for ResMut<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for ResMut<'_, T> {}

// ── EventReader / EventWriter ──────────────────────────────────

/// Читатель событий — иммутабельный доступ к EventQueue.
pub struct EventReader<'w, T: Send + Sync + 'static>(pub &'w EventQueue<T>);

impl<'w, T: Send + Sync + 'static> EventReader<'w, T> {
    /// Итерация по событиям предыдущего тика (стандартный режим).
    #[inline] pub fn iter(&self) -> std::slice::Iter<'_, T> { self.0.iter_previous() }
    /// Итерация по событиям текущего тика.
    #[inline] pub fn iter_current(&self) -> std::slice::Iter<'_, T> { self.0.iter_current() }
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

// SAFETY: уникальный доступ гарантирован AccessDescriptor + compile().
unsafe impl<T: Send + Sync + 'static> Send for EventWriter<'_, T> {}
unsafe impl<T: Send + Sync + 'static> Sync for EventWriter<'_, T> {}

// ── WorldQuery::system_access ──────────────────────────────────

/// Расширение WorldQuery — описание R/W доступа для планировщика.
///
/// Реализовано для Read<T>, Write<T>, With<T>, Without<T>, Changed<T>
/// и кортежей из них.
pub trait WorldQuerySystemAccess: WorldQuery {
    fn system_access() -> AccessDescriptor;
}