//! Common run conditions — из коробки.
//!
//! Аналог Bevy `common_conditions`: `resource_exists`, `resource_changed`, `any_with_component`.
//! Для Apex — stateless функции `Fn(&World) -> bool`.
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

use crate::RunCondition;
use apex_core::component::Component;
use apex_core::query::{Query, Read};
use apex_core::world::World;
use std::sync::atomic::{AtomicU32, Ordering};

/// Условие: ресурс типа `T` существует в мире.
///
/// ```ignore
/// .run_if(conditions::resource_exists::<GameState>())
/// ```
pub fn resource_exists<T: Send + Sync + 'static>() -> RunCondition {
    Box::new(|w: &World| w.has_resource::<T>())
}

/// Условие: ресурс типа `T` существует и равен заданному значению.
///
/// ```ignore
/// .run_if(conditions::resource_equals(GamePhase::Playing))
/// ```
pub fn resource_equals<T: Send + Sync + 'static + PartialEq>(value: T) -> RunCondition {
    Box::new(move |w: &World| w.try_resource::<T>().map(|r| *r == value).unwrap_or(false))
}

/// Условие: в мире есть хотя бы один entity с компонентом `T`.
///
/// ```ignore
/// .run_if(conditions::any_with_component::<Player>())
/// ```
pub fn any_with_component<T: Component>() -> RunCondition {
    Box::new(|w: &World| Query::<Read<T>>::new(w).iter().count() > 0)
}

/// Условие: выполняется ровно N первых раз, затем всегда false.
///
/// ```ignore
/// // Startup система — выполнится 1 раз
/// s.add_system("init", init_fn).run_if(conditions::run_until(1));
/// ```
pub fn run_until(limit: u32) -> RunCondition {
    let counter = AtomicU32::new(0);
    Box::new(move |_: &World| {
        let n = counter.fetch_add(1, Ordering::Relaxed);
        n < limit
    })
}

/// Условие: выполняется не чаще чем раз в N кадров.
///
/// ```ignore
/// // Тяжёлая система — раз в 60 кадров
/// .run_if(conditions::every_n_frames(60))
/// ```
pub fn every_n_frames(n: u32) -> RunCondition {
    let counter = AtomicU32::new(0);
    Box::new(move |_: &World| {
        let tick = counter.fetch_add(1, Ordering::Relaxed);
        tick % n == 0
    })
}

/// Инвертировать условие. Возвращает `!cond(world)`.
///
/// ```ignore
/// // Система работает когда нет паузы:
/// .run_if(conditions::not(|w| w.resource::<GameState>().paused))
/// ```
pub fn not(cond: RunCondition) -> RunCondition {
    Box::new(move |w: &World| !cond(w))
}
