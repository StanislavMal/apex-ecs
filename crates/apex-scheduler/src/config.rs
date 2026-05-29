//! SystemConfig — конфигурация системы с условиями (value type, без ссылки на Scheduler).
//!
//! # Использование
//!
//! ```ignore
//! use apex_scheduler::{SystemConfig, IntoScheduleConfigs};
//!
//! s.add_systems(StageLabel::Update, (
//!     SystemConfig::auto("movement", movement).run_if(is_playing),
//!     SystemConfig::seq("cleanup", |w| { ... }),
//!     SystemConfig::par("log", |_: SystemContext| println!("tick")),
//! ));
//! ```

use crate::{AccessDescriptor, ConditionTree, ParSystem, SystemFn};
use apex_core::system_param::{AutoSystem, EventAccessList, ResourceAccessList, WorldQuerySystemAccess};
use apex_core::world::SystemContext;

// ── SystemConfig ───────────────────────────────────────────

pub(crate) enum SystemConfigKind {
    Auto(Box<dyn ParSystem>, AccessDescriptor),
    Sequential(SystemFn),
    ParClosure {
        access: AccessDescriptor,
        func: Box<dyn FnMut(SystemContext<'_>) + Send + Sync>,
    },
}

/// Конфигурация одной системы (имя + тело + условия).
/// Value type — не ссылается на Scheduler, можно делать tuple.
pub struct SystemConfig {
    pub(crate) name: String,
    pub(crate) kind: SystemConfigKind,
    pub(crate) condition: ConditionTree,
}

impl SystemConfig {
    /// AND-условие: добавляет condition в AND-группу.
    pub fn run_if<F>(mut self, condition: F) -> Self
    where
        F: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        let leaf = ConditionTree::leaf(condition);
        match &mut self.condition {
            ConditionTree::And(ref mut conds) => conds.push(leaf),
            _ => {
                let old = std::mem::replace(&mut self.condition, ConditionTree::And(Vec::new()));
                if let ConditionTree::And(ref mut conds) = self.condition {
                    conds.push(old);
                    conds.push(leaf);
                }
            }
        }
        self
    }

    /// OR-условие: добавляет condition в OR-группу.
    pub fn or_else<F>(mut self, condition: F) -> Self
    where
        F: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        let leaf = ConditionTree::leaf(condition);
        match &mut self.condition {
            ConditionTree::Or(ref mut conds) => conds.push(leaf),
            _ => {
                let old = std::mem::replace(&mut self.condition, ConditionTree::Or(Vec::new()));
                if let ConditionTree::Or(ref mut conds) = self.condition {
                    conds.push(old);
                    conds.push(leaf);
                }
            }
        }
        self
    }

    /// Установить всё дерево условий целиком.
    pub fn condition(mut self, tree: ConditionTree) -> Self {
        self.condition = tree;
        self
    }
}

// ── Конструкторы ───────────────────────────────────────────

impl SystemConfig {
    /// AutoSystem (включает `system!` struct).
    pub fn auto<S: AutoSystem + 'static>(name: impl Into<String>, s: S) -> Self {
        let mut access = S::Query::system_access()
            .merge(&S::Resources::resource_accesses())
            .merge(&S::Events::event_accesses());
        if S::NEEDS_WHOLE_WORLD {
            access.needs_whole_world = true;
        }
        struct Adapter<S: AutoSystem>(S);
        impl<S: AutoSystem + 'static> ParSystem for Adapter<S> {
            fn access() -> AccessDescriptor { unreachable!() }
            fn run(&mut self, ctx: SystemContext<'_>) { self.0.run(ctx); }
        }
        Self {
            name: name.into(),
            kind: SystemConfigKind::Auto(Box::new(Adapter(s)), access),
            condition: ConditionTree::default(),
        }
    }

    /// Sequential система.
    pub fn seq<F>(name: impl Into<String>, f: F) -> Self
    where
        F: FnMut(&mut apex_core::world::World) + Send + 'static,
    {
        Self {
            name: name.into(),
            kind: SystemConfigKind::Sequential(Box::new(f)),
            condition: ConditionTree::default(),
        }
    }

    /// Параллельное замыкание (без доступа к компонентам).
    pub fn par<F>(name: impl Into<String>, f: F) -> Self
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            kind: SystemConfigKind::ParClosure {
                access: AccessDescriptor::new(),
                func: Box::new(f),
            },
            condition: ConditionTree::default(),
        }
    }

    /// Параллельное замыкание с явным AccessDescriptor.
    pub fn par_access<F>(name: impl Into<String>, access: AccessDescriptor, f: F) -> Self
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            kind: SystemConfigKind::ParClosure { access, func: Box::new(f) },
            condition: ConditionTree::default(),
        }
    }
}

// ── IntoScheduleConfigs — tuple развёртка ──────────────────

/// Трейт для конвертации в Vec<SystemConfig>.
/// Реализован для SystemConfig и кортежей до 12 элементов.
pub trait IntoScheduleConfigs {
    fn into_vec(self) -> Vec<SystemConfig>;
}

impl IntoScheduleConfigs for SystemConfig {
    fn into_vec(self) -> Vec<SystemConfig> {
        vec![self]
    }
}

macro_rules! impl_into_schedule_configs_tuple {
    ($($T:ident),+) => {
        impl<$($T: IntoScheduleConfigs),+> IntoScheduleConfigs for ($($T,)+) {
            #[allow(non_snake_case)]
            fn into_vec(self) -> Vec<SystemConfig> {
                let ($($T,)+) = self;
                let mut v = Vec::new();
                $( v.extend($T.into_vec()); )+
                v
            }
        }
    };
}

impl_into_schedule_configs_tuple!(A);
impl_into_schedule_configs_tuple!(A, B);
impl_into_schedule_configs_tuple!(A, B, C);
impl_into_schedule_configs_tuple!(A, B, C, D);
impl_into_schedule_configs_tuple!(A, B, C, D, E);
impl_into_schedule_configs_tuple!(A, B, C, D, E, F);
impl_into_schedule_configs_tuple!(A, B, C, D, E, F, G);
impl_into_schedule_configs_tuple!(A, B, C, D, E, F, G, H);
impl_into_schedule_configs_tuple!(A, B, C, D, E, F, G, H, I);
impl_into_schedule_configs_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_into_schedule_configs_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_into_schedule_configs_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);
