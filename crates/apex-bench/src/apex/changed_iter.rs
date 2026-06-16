use apex_core::prelude::*;
use apex_core::component::Tick;
use apex_core::query::Changed;
use apex_macros::Component;

// ChangedIter — реалистичный «обработать только изменённое»: каждый кадр меняется 10% сущностей,
// система итерирует `Changed<Data>`. Меряет СКАН change-tick'ов (10k) + сбор изменённых (1k).
// Не покрыто другими бенчами; ключевой паттерн реактивных систем. Возвращает ЧИСЛО изменённых
// (для верификации честности: apex и bevy обязаны выдавать одинаковые ~1000).
#[derive(Component, Clone, Copy)]
pub struct Data(pub f32);

pub struct ChangedIter {
    world: World,
    entities: Vec<Entity>,
    last_run: Tick,
    // Персистентный QueryState (идиоматичный путь = аналог bevy stored QueryState) — матч
    // архетипов кэшируется, не пересобирается каждый кадр.
    state: QueryState<(Changed<Data>, Read<Data>)>,
}

impl Default for ChangedIter {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangedIter {
    pub fn new() -> Self {
        let mut world = World::new();
        let entities = world.spawn_many(10_000, |_| (Data(0.0),));
        world.advance_change_tick();
        let last_run = world.current_tick();
        Self {
            world,
            entities,
            last_run,
            state: QueryState::new(),
        }
    }

    pub fn run(&mut self) -> u32 {
        self.world.advance_change_tick();
        let now = self.world.current_tick();
        // Мутируем первые 10% — стампит текущий тик (> last_run).
        for &e in &self.entities[..1000] {
            if let Some(mut d) = self.world.get_mut::<Data>(e) {
                d.0 += 1.0;
            }
        }
        // Итерируем Changed относительно прошлого кадра — скан 10k тиков, сбор ~1000.
        let mut count = 0u32;
        self.state
            .query_with_tick(&self.world, self.last_run)
            .for_each(|_, _| count += 1);
        self.last_run = now;
        count
    }
}
