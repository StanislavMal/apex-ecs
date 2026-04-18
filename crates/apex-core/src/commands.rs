use crate::{
    component::Component,
    entity::Entity,
    world::{Bundle, World},
};

// ── Команды ────────────────────────────────────────────────────

/// Тип-стёртая функция применения команды
type CommandFn = Box<dyn FnOnce(&mut World) + Send>;

/// Очередь команд — буферизует structural changes для применения после итерации.
///
/// Это решает проблему "нельзя изменять мир во время итерации по нему".
/// Паттерн из Bevy/Flecs: системы получают Commands, накапливают изменения,
/// после завершения системы World применяет всё разом.
///
/// # Пример
/// ```ignore
/// let mut cmds = Commands::new();
/// for (entity, health) in Query::<(Read<Health>,)>::new(&world).iter() {
///     if health.current <= 0.0 {
///         cmds.despawn(entity);
///     }
/// }
/// cmds.apply(&mut world);
/// ```
pub struct Commands {
    queue: Vec<CommandFn>,
}

impl Commands {
    pub fn new() -> Self {
        Self { queue: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { queue: Vec::with_capacity(cap) }
    }

    /// Создать entity из Bundle
    pub fn spawn_bundle<B: Bundle + Send + 'static>(&mut self, bundle: B) {
        self.queue.push(Box::new(move |world: &mut World| {
            world.spawn_bundle(bundle);
        }));
    }

    /// Уничтожить entity
    pub fn despawn(&mut self, entity: Entity) {
        self.queue.push(Box::new(move |world: &mut World| {
            world.despawn(entity);
        }));
    }

    /// Добавить компонент к entity
    pub fn insert<T: Component + Send + 'static>(&mut self, entity: Entity, component: T) {
        self.queue.push(Box::new(move |world: &mut World| {
            world.insert(entity, component);
        }));
    }

    /// Удалить компонент у entity
    pub fn remove<T: Component + Send + 'static>(&mut self, entity: Entity) {
        self.queue.push(Box::new(move |world: &mut World| {
            world.remove::<T>(entity);
        }));
    }

    /// Применить все накопленные команды к миру
    pub fn apply(&mut self, world: &mut World) {
        for cmd in self.queue.drain(..) {
            cmd(world);
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl Default for Commands {
    fn default() -> Self { Self::new() }
}
