//! Common run conditions — из коробки.
//!
//! Каждый built-in condition реализует [`Condition`] с типизированным `access()`.
//! Планировщик знает какие данные читает условие → автоматически строит dependency edges.
//!
//! # Использование
//!
//! ```ignore
//! use apex_scheduler::conditions;
//!
//! sched.add_auto_system("movement", movement)
//!     .run_if(conditions::resource_exists::<GameState>())
//!     .run_if(conditions::resource_equals(GamePhase::Playing));
//!
//! sched.add_system("init", init_system)
//!     .run_if(conditions::run_until(1));
//! ```

use crate::Condition;
use apex_core::component::Component;
use apex_core::query::{Query, Read};
use apex_core::world::World;
use apex_core::AccessDescriptor;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct ResourceExists<T>(PhantomData<T>);

impl<T> Clone for ResourceExists<T> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<T: Send + Sync + 'static> Condition for ResourceExists<T> {
    fn check(&self, w: &World) -> bool {
        w.has_resource::<T>()
    }
    fn access(&self) -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

pub fn resource_exists<T: Send + Sync + 'static>() -> ResourceExists<T> {
    ResourceExists(PhantomData)
}

pub struct ResourceEquals<T> {
    value: T,
}

impl<T: Clone> Clone for ResourceEquals<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
        }
    }
}

impl<T: Send + Sync + 'static + PartialEq> Condition for ResourceEquals<T> {
    fn check(&self, w: &World) -> bool {
        w.try_resource::<T>()
            .map(|r| *r == self.value)
            .unwrap_or(false)
    }
    fn access(&self) -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

pub fn resource_equals<T: Send + Sync + 'static + PartialEq>(value: T) -> ResourceEquals<T> {
    ResourceEquals { value }
}

pub struct AnyWithComponent<T>(PhantomData<T>);

impl<T> Clone for AnyWithComponent<T> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<T: Component> Condition for AnyWithComponent<T> {
    fn check(&self, w: &World) -> bool {
        Query::<Read<T>>::new(w).iter().count() > 0
    }
    fn access(&self) -> AccessDescriptor {
        AccessDescriptor::new().read::<T>()
    }
}

pub fn any_with_component<T: Component>() -> AnyWithComponent<T> {
    AnyWithComponent(PhantomData)
}

pub struct RunUntil {
    limit: u32,
    counter: AtomicU32,
}

impl Condition for RunUntil {
    fn check(&self, _: &World) -> bool {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        n < self.limit
    }
}

pub fn run_until(limit: u32) -> RunUntil {
    RunUntil {
        limit,
        counter: AtomicU32::new(0),
    }
}

pub struct EveryNFrames {
    n: u32,
    counter: AtomicU32,
}

impl Condition for EveryNFrames {
    fn check(&self, _: &World) -> bool {
        let tick = self.counter.fetch_add(1, Ordering::Relaxed);
        tick.is_multiple_of(self.n)
    }
}

pub fn every_n_frames(n: u32) -> EveryNFrames {
    EveryNFrames {
        n,
        counter: AtomicU32::new(0),
    }
}

pub fn not<C: Condition>(cond: C) -> crate::NotCondition<C> {
    cond.not()
}
