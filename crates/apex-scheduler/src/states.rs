//! App-состояния (D2-6, аналог Bevy `States`): `State<S>` / `NextState<S>` +
//! условия `in_state` / `on_enter` / `on_exit` поверх существующих run
//! conditions.
//!
//! ```ignore
//! #[derive(Clone, Copy, PartialEq, Eq, Debug)]
//! enum GameState { Menu, Playing }
//!
//! init_state(app.world_mut(), app.scheduler_mut(), GameState::Menu);
//! // или app.add_state(GameState::Menu) в apex-app.
//!
//! sched.add_systems(StageLabel::Update, (
//!     sys("menu_ui", MenuUi).run_if(in_state(GameState::Menu)),
//!     sys("spawn_level", SpawnLevel).run_if(on_enter(GameState::Playing)),
//!     sys("save", Save).run_if(on_exit(GameState::Playing)),
//! ));
//!
//! // Переход — из любой системы (ресурс NextState):
//! next.set(GameState::Playing);
//! ```
//!
//! Семантика: переход применяется эксклюзивной системой в начале кадра
//! (`StageLabel::First`) — кадр целиком видит ОДНО состояние; `on_enter`/
//! `on_exit`-условия истинны ровно один кадр (кадр применения перехода;
//! `on_enter(initial)` — первый кадр после `init_state`). Это
//! condition-модель, а не отдельные расписания Bevy `OnEnter`-schedule;
//! функционально покрывает те же сценарии и дружит с любым этапом.

use apex_core::world::World;

use crate::{Scheduler, StageLabel};

/// Маркер-границы типа состояния (enum'ы со `derive(Clone, Copy, PartialEq)`).
pub trait States: Clone + Copy + PartialEq + Send + Sync + 'static {}
impl<T: Clone + Copy + PartialEq + Send + Sync + 'static> States for T {}

/// Текущее состояние (ресурс; читать через [`in_state`] или напрямую).
pub struct State<S: States>(pub S);

impl<S: States> State<S> {
    #[inline]
    pub fn get(&self) -> S {
        self.0
    }
}

/// Запрос перехода (ресурс). Применяется в начале СЛЕДУЮЩЕГО кадра.
/// Несколько `set` за кадр — выигрывает последний.
pub struct NextState<S: States>(pub Option<S>);

impl<S: States> NextState<S> {
    #[inline]
    pub fn set(&mut self, next: S) {
        self.0 = Some(next);
    }
}

/// Переходы ТЕКУЩЕГО кадра (ресурс, выставляется системой переходов):
/// основа условий [`on_enter`]/[`on_exit`].
pub struct StateTransitions<S: States> {
    pub entered: Option<S>,
    pub exited: Option<S>,
}

/// Зарегистрировать состояние: ресурсы `State`/`NextState`/`StateTransitions`
/// плюс эксклюзивная система применения переходов в `StageLabel::First`;
/// `on_enter(initial)` сработает на первом кадре.
pub fn init_state<S: States>(world: &mut World, sched: &mut Scheduler, initial: S) {
    world.insert_resource(State(initial));
    world.insert_resource(NextState::<S>(None));
    world.insert_resource(StateTransitions::<S> {
        entered: Some(initial),
        exited: None,
    });

    sched.add_system_to_stage(
        format!("apply_state_transition<{}>", std::any::type_name::<S>()),
        apply_state_transition::<S>,
        StageLabel::First,
    );
}

/// Система применения перехода (эксклюзивная, начало кадра).
fn apply_state_transition<S: States>(world: &mut World) {
    // Сначала гасим переходы прошлого кадра (on_enter/on_exit — один кадр).
    {
        let tr = world.resource_mut::<StateTransitions<S>>();
        tr.entered = None;
        tr.exited = None;
    }
    let Some(next) = world.resource_mut::<NextState<S>>().0.take() else {
        return;
    };
    let current = world.resource::<State<S>>().0;
    if next == current {
        return;
    }
    world.resource_mut::<State<S>>().0 = next;
    let tr = world.resource_mut::<StateTransitions<S>>();
    tr.exited = Some(current);
    tr.entered = Some(next);
}

/// Условие «мир в состоянии `s`» — для `run_if`.
pub fn in_state<S: States>(s: S) -> impl Fn(&World) -> bool + Send + Sync + 'static {
    move |world: &World| {
        world
            .try_resource::<State<S>>()
            .map(|st| st.0 == s)
            .unwrap_or(false)
    }
}

/// Условие «в ЭТОМ кадре вошли в `s`» (истинно один кадр).
pub fn on_enter<S: States>(s: S) -> impl Fn(&World) -> bool + Send + Sync + 'static {
    move |world: &World| {
        world
            .try_resource::<StateTransitions<S>>()
            .map(|tr| tr.entered == Some(s))
            .unwrap_or(false)
    }
}

/// Условие «в ЭТОМ кадре вышли из `s`» (истинно один кадр).
pub fn on_exit<S: States>(s: S) -> impl Fn(&World) -> bool + Send + Sync + 'static {
    move |world: &World| {
        world
            .try_resource::<StateTransitions<S>>()
            .map(|tr| tr.exited == Some(s))
            .unwrap_or(false)
    }
}
