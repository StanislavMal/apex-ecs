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

/// Конфигурация одной системы (имя + тело + условия + порядок).
/// Value type — не ссылается на Scheduler, можно делать tuple.
pub struct SystemConfig {
    pub(crate) name: String,
    pub(crate) kind: SystemConfigKind,
    pub(crate) condition: ConditionTree,
    pub(crate) condition_access: AccessDescriptor,
    pub(crate) has_deferred: bool,
    /// Names of systems this one must run BEFORE (declared via `.before()`).
    /// Resolved to `SystemId` at compile time (forward references allowed).
    pub(crate) before_names: Vec<String>,
    /// Names of systems this one must run AFTER (declared via `.after()`).
    pub(crate) after_names: Vec<String>,
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
    pub(crate) fn sys<S: AutoSystem + 'static>(name: impl Into<String>, s: S) -> Self {
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
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Sequential система.
    pub(crate) fn seq<F>(name: impl Into<String>, f: F) -> Self
    where
        F: FnMut(&mut apex_core::world::World) + Send + 'static,
    {
        Self {
            name: name.into(),
            kind: SystemConfigKind::Sequential(Box::new(f)),
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred: false,
            before_names: Vec::new(),
            after_names: Vec::new(),
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
            before_names: Vec::new(),
            after_names: Vec::new(),
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
        // В3: the closure OWNS the per-system state (Query arch-index caches etc.),
        // alive across frames. `State: Send + Sync + Default + 'static`, so the box
        // keeps its existing Send + Sync bounds (no ripple).
        let mut state = <<F::Param as apex_core::SystemParam>::State>::default();
        let func: Box<dyn FnMut(SystemContext<'_>) + Send + Sync> =
            Box::new(move |ctx: SystemContext<'_>| {
                // Э5: skip-семантика параметров (Single<Q> и т.п.) — Bevy
                // validate_param: невалидный параметр тихо пропускает кадр.
                if !<F::Param as apex_core::SystemParam>::validate(&ctx) {
                    return;
                }
                let item =
                    <F::Param as apex_core::SystemParam>::get_param(&ctx, &mut state);
                f.run(item);
            });
        Self {
            name: apex_core::short_system_name::<F>().to_string(),
            kind: SystemConfigKind::ParClosure { access, func },
            condition: ConditionTree::default(),
            condition_access: AccessDescriptor::new(),
            has_deferred,
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Параллельное замыкание (без доступа к компонентам).
    pub(crate) fn par<F>(name: impl Into<String>, f: F) -> Self
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
            before_names: Vec::new(),
            after_names: Vec::new(),
        }
    }

    /// Параллельное замыкание с явным AccessDescriptor.
    pub(crate) fn par_access<F>(name: impl Into<String>, access: AccessDescriptor, f: F) -> Self
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
            before_names: Vec::new(),
            after_names: Vec::new(),
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

/// Маркер: элемент — уже собранный [`ScheduleConfigs`] (напр. результат
/// `.chain()`/`.before()`/`.after()`).
#[doc(hidden)]
pub struct ScheduleConfigsMarker;

/// Группа сконфигурированных систем + их порядковые рёбра — то, во что
/// [`IntoScheduleConfigs`] превращает системы/кортежи перед регистрацией.
///
/// Порядковые методы (`.before()`/`.after()`/`.chain()`) золотого пути
/// возвращают этот тип. `edges` — позиционные рёбра «раньше→позже» между
/// элементами `configs` (индексы в `configs`); заполняются `.chain()` и
/// сохраняются при склейке кортежей (вложенные `.chain()` не теряются).
/// Именованные рёбра (`.before("x")`) хранятся на самих
/// [`SystemConfig::before_names`]/[`SystemConfig::after_names`] и резолвятся
/// планировщиком при `compile()` (forward-ссылки разрешены).
pub struct ScheduleConfigs {
    pub(crate) configs: Vec<SystemConfig>,
    /// Позиционные рёбра порядка `(before_idx, after_idx)` внутри `configs`.
    pub(crate) edges: Vec<(usize, usize)>,
}

impl ScheduleConfigs {
    fn single(cfg: SystemConfig) -> Self {
        Self { configs: vec![cfg], edges: Vec::new() }
    }
}

/// Конвертация систем/кортежей в [`ScheduleConfigs`] — единый вход `add_systems`.
///
/// Реализован для `SystemConfig`, bare `AutoSystem`/`ExclusiveSystem`/plain-fn
/// систем, [`ScheduleConfigs`] и кортежей до 12 элементов. Порядковые методы
/// (`before`/`after`/`chain`) — золотой путь декларации зависимостей прямо на
/// конфигах (Bevy-идиома); строковый `Scheduler::before/after/chain` остаётся
/// для ДИНАМИЧЕСКОГО порядка по имени (редактор/скрипты).
pub trait IntoScheduleConfigs<M>: Sized {
    /// Превратить в группу конфигов (для `add_systems`).
    fn into_configs(self) -> ScheduleConfigs;

    /// Объявить, что ЭТА система (или все системы группы) выполняются ДО системы
    /// с именем `name`. Имя резолвится при `compile()` (forward-ссылки допустимы).
    fn before(self, name: impl Into<String>) -> ScheduleConfigs {
        let mut c = self.into_configs();
        let name = name.into();
        for cfg in &mut c.configs {
            cfg.before_names.push(name.clone());
        }
        c
    }

    /// Объявить, что ЭТА система (или все системы группы) выполняются ПОСЛЕ
    /// системы с именем `name`. Имя резолвится при `compile()`.
    fn after(self, name: impl Into<String>) -> ScheduleConfigs {
        let mut c = self.into_configs();
        let name = name.into();
        for cfg in &mut c.configs {
            cfg.after_names.push(name.clone());
        }
        c
    }

    /// Сцепить системы группы в цепочку: `a → b → c` (каждая после предыдущей).
    /// Для одиночного элемента — no-op. Позиционно (по порядку в кортеже), имена
    /// не нужны.
    fn chain(self) -> ScheduleConfigs {
        let mut c = self.into_configs();
        for i in 0..c.configs.len().saturating_sub(1) {
            c.edges.push((i, i + 1));
        }
        c
    }
}

impl IntoScheduleConfigs<ScheduleConfigsMarker> for ScheduleConfigs {
    fn into_configs(self) -> ScheduleConfigs {
        self
    }
}

impl IntoScheduleConfigs<ConfigMarker> for SystemConfig {
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(self)
    }
}

impl<S: AutoSystem + 'static> IntoScheduleConfigs<AutoMarker> for S {
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(SystemConfig::sys(S::name(), self))
    }
}

impl<S: ExclusiveSystem> IntoScheduleConfigs<ExclusiveMarker> for S {
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(SystemConfig::exclusive(self))
    }
}

/// Plain-fn система (D2-1): `fn movement(time: Res<Time>, q: Query<…>)` →
/// `add_systems(stage, (movement, …))`. Маркер несёт сигнатуру `fn(P1, …)`,
/// поэтому когерентность с Auto/Exclusive-имплами не конфликтует.
impl<F, M> IntoScheduleConfigs<FnSystemMarker<M>> for F
where
    F: apex_core::SystemParamFunction<M>,
{
    fn into_configs(self) -> ScheduleConfigs {
        ScheduleConfigs::single(SystemConfig::fn_sys(self))
    }
}

/// Builder-методы прямо на plain-fn системе (П4, как Bevy `IntoSystemConfigs`):
/// `movement.run_if(in_state(Game::Playing))` — без обёртки в
/// `SystemConfig::fn_sys`. (Порядковые `.before()/.after()/.chain()` доступны
/// через [`IntoScheduleConfigs`].)
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
            fn into_configs(self) -> ScheduleConfigs {
                let ($($T,)+) = self;
                let mut configs: Vec<SystemConfig> = Vec::new();
                let mut edges: Vec<(usize, usize)> = Vec::new();
                $(
                    // Склейка: смещаем позиционные рёбра дочерней группы на текущее
                    // число уже накопленных конфигов, чтобы вложенные `.chain()`
                    // сохранялись после flatten.
                    let mut sub = $T.into_configs();
                    let offset = configs.len();
                    for (a, b) in sub.edges.drain(..) {
                        edges.push((a + offset, b + offset));
                    }
                    configs.append(&mut sub.configs);
                )+
                ScheduleConfigs { configs, edges }
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
