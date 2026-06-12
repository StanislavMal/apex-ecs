//! SystemConfig — конфигурация системы с условиями (value type, без ссылки на Scheduler).
//!
//! # Использование
//!
//! ```ignore
//! use apex_scheduler::{sys, seq, par};
//!
//! s.add_systems(StageLabel::Update, (
//!     sys("movement", movement).run_if(is_playing),
//!     seq("cleanup", |w| { ... }),
//!     par("log", |_: SystemContext| println!("tick")),
//! ));
//! ```

use crate::{AccessDescriptor, Condition, ConditionTree, ParSystem, SystemFn};
use apex_core::system_param::{
    AutoSystem, EventAccessList, ExclusiveSystem, ResourceAccessList, WorldQuerySystemAccess,
};
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
    pub(crate) condition_access: AccessDescriptor,
    pub(crate) has_deferred: bool,
}

impl SystemConfig {
    pub fn run_if<F>(self, condition: F) -> Self
    where
        F: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        self.push_and_leaf(ConditionTree::leaf(condition), AccessDescriptor::new())
    }

    pub fn run_if_cond<C: Condition>(self, condition: C) -> Self {
        let acc = condition.access();
        let leaf = ConditionTree::Leaf(condition.into_check_fn());
        self.push_and_leaf(leaf, acc)
    }

    pub fn or_else<F>(self, condition: F) -> Self
    where
        F: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        self.push_or_leaf(ConditionTree::leaf(condition), AccessDescriptor::new())
    }

    pub fn or_else_cond<C: Condition>(self, condition: C) -> Self {
        let acc = condition.access();
        let leaf = ConditionTree::Leaf(condition.into_check_fn());
        self.push_or_leaf(leaf, acc)
    }

    fn push_and_leaf(mut self, leaf: ConditionTree, acc: AccessDescriptor) -> Self {
        self.condition_access = std::mem::take(&mut self.condition_access).merge(&acc);
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

    fn push_or_leaf(mut self, leaf: ConditionTree, acc: AccessDescriptor) -> Self {
        self.condition_access = std::mem::take(&mut self.condition_access).merge(&acc);
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
    pub fn sys<S: AutoSystem + 'static>(name: impl Into<String>, s: S) -> Self {
        let mut access = S::Query::system_access()
            .merge(&S::Resources::resource_accesses())
            .merge(&S::Events::event_accesses());
        if S::NEEDS_WHOLE_WORLD {
            access.needs_whole_world = true;
        }
        // W3-4: система с состоянием → без ASD row-split (run(&mut self)
        // одного экземпляра нельзя звать из нескольких задач конкурентно).
        if std::mem::size_of::<S>() > 0 {
            access.stateful = true;
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
            condition_access: AccessDescriptor::new(),
            has_deferred: S::HAS_DEFERRED,
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
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
        }
    }

    /// Эксклюзивная система (`system!` с `world: &mut World`).
    ///
    /// Объявляет FULL access и регистрируется как Sequential — планировщик
    /// исполняет её в одиночку. Имя берётся из самой системы.
    pub fn exclusive<S: ExclusiveSystem>(mut system: S) -> Self {
        let name = system.name().to_string();
        Self {
            name,
            kind: SystemConfigKind::Sequential(Box::new(move |w| system.run(w))),
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
        }
    }

    /// Plain-fn система (D2-1): обычная функция с Bevy-параметрами
    /// (`Res<T>`/`ResMut<T>`/`Query<Q>`/`EventReader<E>`/`EventWriter<E>`/
    /// `&mut Commands`). Access выводится из параметров, имя — из имени
    /// функции. Обычно вызывается неявно через `add_systems(stage, (fn1, …))`.
    pub fn fn_sys<F, M>(f: F) -> Self
    where
        F: apex_core::SystemParamFunction<M>,
    {
        let mut access = <F::Param as apex_core::SystemParam>::access();
        // W3-4: замыкание с захватами = состояние → без ASD row-split
        // (fn-item — ZST, не попадает).
        if std::mem::size_of::<F>() > 0 {
            access.stateful = true;
        }
        let has_deferred = <F::Param as apex_core::SystemParam>::has_deferred();
        let mut f = f;
        let func: Box<dyn FnMut(SystemContext<'_>) + Send + Sync> =
            Box::new(move |ctx: SystemContext<'_>| {
                let item = <F::Param as apex_core::SystemParam>::fetch(&ctx);
                f.run(item);
            });
        Self {
            name: apex_core::short_system_name::<F>().to_string(),
            kind: SystemConfigKind::ParClosure { access, func },
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred,
        }
    }

    /// Параллельное замыкание (без доступа к компонентам).
    pub fn par<F>(name: impl Into<String>, f: F) -> Self
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let mut access = AccessDescriptor::new();
        // W3-4: замыкание с захватами = состояние → без ASD row-split.
        if std::mem::size_of::<F>() > 0 {
            access.stateful = true;
        }
        Self {
            name: name.into(),
            kind: SystemConfigKind::ParClosure {
                access,
                func: Box::new(f),
            },
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
        }
    }

    /// Параллельное замыкание с явным AccessDescriptor.
    pub fn par_access<F>(name: impl Into<String>, access: AccessDescriptor, f: F) -> Self
    where
        F: FnMut(SystemContext<'_>) + Send + Sync + 'static,
    {
        let mut access = access;
        // W3-4: замыкание с захватами = состояние → без ASD row-split.
        if std::mem::size_of::<F>() > 0 {
            access.stateful = true;
        }
        Self {
            name: name.into(),
            kind: SystemConfigKind::ParClosure { access, func: Box::new(f) },
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
        }
    }
}

// ── Free function aliases — короткий API ───────────────────

/// AutoSystem (включает `system!` struct). Алиас для `SystemConfig::sys()`.
pub fn sys<S: AutoSystem + 'static>(name: impl Into<String>, s: S) -> SystemConfig {
    SystemConfig::sys(name, s)
}

/// Sequential система. Алиас для `SystemConfig::seq()`.
pub fn seq<F>(name: impl Into<String>, f: F) -> SystemConfig
where F: FnMut(&mut apex_core::world::World) + Send + 'static
{
    SystemConfig::seq(name, f)
}

/// Параллельное замыкание (без доступа). Алиас для `SystemConfig::par()`.
pub fn par<F>(name: impl Into<String>, f: F) -> SystemConfig
where F: FnMut(SystemContext<'_>) + Send + Sync + 'static
{
    SystemConfig::par(name, f)
}

/// Параллельное замыкание с явным AccessDescriptor. Алиас для `SystemConfig::par_access()`.
pub fn par_access<F>(name: impl Into<String>, access: AccessDescriptor, f: F) -> SystemConfig
where F: FnMut(SystemContext<'_>) + Send + Sync + 'static
{
    SystemConfig::par_access(name, access, f)
}

// ── IntoScheduleConfigs — единый вход регистрации (U.3/U.4) ─────
//
// Маркер-параметр `M` различает источники (Bevy-style disambiguation), что
// позволяет передавать в `add_systems` РАЗНОРОДНЫЕ элементы одним кортежом:
//   - bare `AutoSystem`-маркер (параллельная система из `system!`) — имя из fn;
//   - bare `ExclusiveSystem`-маркер (`system!` с `world: &mut World`) — имя из fn;
//   - готовый `SystemConfig` (`sys()`/`seq()`/`.run_if()`…).
// Разные маркеры → разные инстанцирования трейта → нет конфликта когерентности.

/// Маркер: элемент — готовый `SystemConfig`.
#[doc(hidden)]
pub struct ConfigMarker;
/// Маркер: элемент — bare `AutoSystem` (параллельная система).
#[doc(hidden)]
pub struct AutoMarker;
/// Маркер: элемент — bare `ExclusiveSystem`.
#[doc(hidden)]
pub struct ExclusiveMarker;
/// Маркер: элемент — plain-fn система (D2-1, `SystemParamFunction`).
/// Не кортеж — иначе пересекался бы с кортежным имплом `IntoScheduleConfigs`.
#[doc(hidden)]
pub struct FnSystemMarker<M>(std::marker::PhantomData<M>);

/// Трейт конвертации в `Vec<SystemConfig>`. Реализован для `SystemConfig`,
/// bare `AutoSystem`/`ExclusiveSystem`-маркеров и кортежей до 12 элементов.
pub trait IntoScheduleConfigs<M> {
    fn into_vec(self) -> Vec<SystemConfig>;
}

impl IntoScheduleConfigs<ConfigMarker> for SystemConfig {
    fn into_vec(self) -> Vec<SystemConfig> {
        vec![self]
    }
}

impl<S: AutoSystem + 'static> IntoScheduleConfigs<AutoMarker> for S {
    fn into_vec(self) -> Vec<SystemConfig> {
        vec![SystemConfig::sys(S::name(), self)]
    }
}

impl<S: ExclusiveSystem> IntoScheduleConfigs<ExclusiveMarker> for S {
    fn into_vec(self) -> Vec<SystemConfig> {
        vec![SystemConfig::exclusive(self)]
    }
}

/// Plain-fn система (D2-1): `fn movement(time: Res<Time>, q: Query<…>)` →
/// `add_systems(stage, (movement, …))`. Маркер несёт сигнатуру `fn(P1, …)`,
/// поэтому когерентность с Auto/Exclusive-имплами не конфликтует.
impl<F, M> IntoScheduleConfigs<FnSystemMarker<M>> for F
where
    F: apex_core::SystemParamFunction<M>,
{
    fn into_vec(self) -> Vec<SystemConfig> {
        vec![SystemConfig::fn_sys(self)]
    }
}

/// Builder-методы прямо на plain-fn системе (П4, как Bevy `IntoSystemConfigs`):
/// `movement.run_if(in_state(Game::Playing))` — без обёртки в
/// `SystemConfig::fn_sys`.
pub trait FnSystemExt<M>: apex_core::SystemParamFunction<M> + Sized {
    /// Превратить fn в [`SystemConfig`] (имя — из имени функции).
    fn into_config(self) -> SystemConfig {
        SystemConfig::fn_sys(self)
    }

    /// Условие выполнения (см. [`SystemConfig::run_if`]).
    fn run_if<C>(self, cond: C) -> SystemConfig
    where
        C: Fn(&apex_core::world::World) -> bool + Send + Sync + 'static,
    {
        self.into_config().run_if(cond)
    }
}

impl<F, M> FnSystemExt<M> for F where F: apex_core::SystemParamFunction<M> {}

macro_rules! impl_into_schedule_configs_tuple {
    ($($T:ident : $M:ident),+) => {
        impl<$($T, $M),+> IntoScheduleConfigs<($($M,)+)> for ($($T,)+)
        where
            $( $T: IntoScheduleConfigs<$M>, )+
        {
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

impl_into_schedule_configs_tuple!(A: MA);
impl_into_schedule_configs_tuple!(A: MA, B: MB);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI, J: MJ);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI, J: MJ, K: MK);
impl_into_schedule_configs_tuple!(A: MA, B: MB, C: MC, D: MD, E: ME, F: MF, G: MG, H: MH, I: MI, J: MJ, K: MK, L: ML);
