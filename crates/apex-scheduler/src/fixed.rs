//! FixedUpdate — фиксированный шаг симуляции (D2-5, аналог Bevy `Time<Fixed>`).
//!
//! Ресурс [`FixedTime`] накапливает реальное время кадра; планировщик перед
//! стадией `StageLabel::FixedUpdate` спрашивает у него число шагов и
//! исполняет стадию столько раз (каждый шаг — со своим применением команд и
//! per-stage флашем событий). Остаток времени переносится на следующий кадр.
//!
//! Подключение:
//! - в движке `apex-app` вставляет `FixedTime` по умолчанию и кормит его
//!   `Time.delta_seconds` каждый кадр;
//! - в чистом ядре: `world.insert_resource(FixedTime::from_hz(60.0))` и
//!   `world.resource_mut::<FixedTime>().accumulate(frame_dt)` перед `run()`.
//! - БЕЗ ресурса стадия FixedUpdate выполняется один раз за кадр (как любая).
//!
//! ⚠ Взаимодействие с DtConditioner (apex-window): кондиционированный sim-dt
//! почти кратен периоду дисплея — аккумулятор работает поверх него штатно,
//! `dt` фиксированного шага независим (типично 1/60). EMA/deadband
//! кондиционера убирает джиттер ВХОДА; спираль смерти ограничена
//! `max_steps_per_frame`.

/// Ресурс фиксированного шага (см. модульную документацию).
#[derive(Debug, Clone)]
pub struct FixedTime {
    /// Фиксированный шаг симуляции, секунды (типично 1/60).
    pub dt: f32,
    /// Кап шагов за кадр — защита от «спирали смерти» (медленный кадр →
    /// больше шагов → ещё медленнее). Излишек времени ОТБРАСЫВАЕТСЯ.
    pub max_steps_per_frame: u32,
    accumulator: f32,
}

impl Default for FixedTime {
    fn default() -> Self {
        Self::from_hz(60.0)
    }
}

impl FixedTime {
    pub fn from_hz(hz: f32) -> Self {
        Self {
            dt: 1.0 / hz,
            max_steps_per_frame: 8,
            accumulator: 0.0,
        }
    }

    pub fn from_dt(dt: f32) -> Self {
        Self {
            dt,
            max_steps_per_frame: 8,
            accumulator: 0.0,
        }
    }

    /// Накопить реальное время кадра (зовётся раз в кадр до `run()`).
    #[inline]
    pub fn accumulate(&mut self, frame_dt: f32) {
        self.accumulator += frame_dt.max(0.0);
    }

    /// Накопленное, ещё не отработанное время (для интерполяции рендера:
    /// `alpha = overstep_fraction()`).
    #[inline]
    pub fn overstep(&self) -> f32 {
        self.accumulator
    }

    /// Доля недоработанного шага `[0, 1)` — коэффициент интерполяции.
    #[inline]
    pub fn overstep_fraction(&self) -> f32 {
        (self.accumulator / self.dt).fract()
    }

    /// Сколько шагов исполнить в этом кадре; списывает их из аккумулятора.
    /// Вызывается планировщиком перед стадией FixedUpdate.
    pub fn drain_steps(&mut self) -> usize {
        if self.dt <= 0.0 {
            return 0;
        }
        let steps = (self.accumulator / self.dt) as u32;
        if steps > self.max_steps_per_frame {
            // Спираль смерти: исполняем кап, излишек отбрасываем.
            self.accumulator = 0.0;
            return self.max_steps_per_frame as usize;
        }
        self.accumulator -= steps as f32 * self.dt;
        steps as usize
    }
}
