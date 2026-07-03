use apex_core::prelude::*;
use crate::{Transform, Position, Rotation, Velocity};
use cgmath::{Matrix4, Vector3};

// SimpleIter — итерация по 10K сущностей, Position += Velocity
// CachedQuery кешируется через QueryCache внутри query()
pub struct SimpleIter {
    world: World,
    // W2-0/W2-0.5: per-state запрос для chunked-варианта (модель Bevy QueryState)
    state: QueryState<(Read<Velocity>, Write<Position>)>,
}

impl Default for SimpleIter {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleIter {
    pub fn new() -> Self {
        let mut world = World::new();

        world.spawn_many(10_000, |_| (
            Transform(Matrix4::from_scale(1.0)),
            Position(Vector3::new(0.0, 0.0, 0.0)),
            Rotation(Vector3::new(0.0, 0.0, 0.0)),
            Velocity(Vector3::new(1.0, 0.0, 0.0)),
        ));

        Self { world, state: QueryState::new() }
    }

    /// Per-element итерация через ПЕРСИСТЕНТНЫЙ `QueryState` — идиоматичный
    /// быстрый путь apex, прямой аналог bevy `QueryState::iter_mut` (bevy-бенч
    /// тоже хранит состояние между вызовами). Прежний вариант звал
    /// `world.query()` каждую итерацию (пересборка матча архетипов) — это был
    /// нечестный гандикап apex против кэширующего bevy.
    pub fn run(&mut self) {
        self.state.query_mut(&mut self.world).for_each(|_, (vel, mut pos)| {
            pos.0 += vel.0;
        });
    }

    /// W2-0.5: плотная chunk-итерация (слайсы колонок + stamp_range).
    pub fn run_chunked(&mut self) {
        self.state.query_mut(&mut self.world).for_each_chunk(|_, (vel, pos)| {
            for (p, v) in pos.iter_mut().zip(vel) {
                p.0 += v.0;
            }
        });
    }
}
