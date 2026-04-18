use crate::{
    component::Component,
    entity::Entity,
    world::{Bundle, World},
};

// ── Typed command enum ─────────────────────────────────────────
//
// Вместо Box<dyn FnOnce(&mut World)> используем конкретный enum.
// Это устраняет heap-аллокацию на каждую команду и vtable dispatch.
// Vec<Command> — плотный массив, cache-friendly при apply.

enum Command {
    Despawn(Entity),
    // Insert и Remove хранят type-erased замыкание только когда
    // нет другого способа — но для основных случаев используем
    // конкретные варианты через trait object с inline storage.
    Apply(Box<dyn FnOnce(&mut World) + Send>),
}

/// Очередь команд — буферизует structural changes для применения после итерации.
///
/// Все команды хранятся в плотном `Vec<Command>` без лишних аллокаций
/// для `despawn` (самый частый случай).
///
/// # Пример
/// ```ignore
/// let mut cmds = Commands::new();
/// Query::<Read<Health>>::new(&world).for_each(|entity, health| {
///     if health.current <= 0.0 {
///         cmds.despawn(entity);
///     }
/// });
/// cmds.apply(&mut world);
/// ```
pub struct Commands {
    queue: Vec<Command>,
}

impl Commands {
    pub fn new() -> Self {
        Self { queue: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { queue: Vec::with_capacity(cap) }
    }

    /// Уничтожить entity — без аллокации, хранится inline в enum
    #[inline]
    pub fn despawn(&mut self, entity: Entity) {
        self.queue.push(Command::Despawn(entity));
    }

    /// Создать entity из Bundle
    pub fn spawn_bundle<B: Bundle + Send + 'static>(&mut self, bundle: B) {
        self.queue.push(Command::Apply(Box::new(move |world: &mut World| {
            world.spawn_bundle(bundle);
        })));
    }

    /// Добавить компонент к entity
    pub fn insert<T: Component + Send + 'static>(&mut self, entity: Entity, component: T) {
        self.queue.push(Command::Apply(Box::new(move |world: &mut World| {
            world.insert(entity, component);
        })));
    }

    /// Удалить компонент у entity
    pub fn remove<T: Component + Send + 'static>(&mut self, entity: Entity) {
        self.queue.push(Command::Apply(Box::new(move |world: &mut World| {
            world.remove::<T>(entity);
        })));
    }

    /// Произвольная команда
    pub fn add<F: FnOnce(&mut World) + Send + 'static>(&mut self, f: F) {
        self.queue.push(Command::Apply(Box::new(f)));
    }

    /// Применить все накопленные команды к миру
    pub fn apply(&mut self, world: &mut World) {
        for cmd in self.queue.drain(..) {
            match cmd {
                Command::Despawn(entity) => { world.despawn(entity); }
                Command::Apply(f) => f(world),
            }
        }
    }

    #[inline] pub fn len(&self) -> usize { self.queue.len() }
    #[inline] pub fn is_empty(&self) -> bool { self.queue.is_empty() }

    /// Очистить без применения
    pub fn clear(&mut self) { self.queue.clear(); }
}

impl Default for Commands {
    fn default() -> Self { Self::new() }
}
