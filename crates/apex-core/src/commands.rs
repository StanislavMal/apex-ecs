use crate::{
    component::{Component, ComponentId},
    entity::Entity,
    template::TemplateParams,
    world::{Bundle, World},
};

// ── Type-erased traits для хранения в enum ─────────────────────

/// Type-erased bundle — может быть применён к миру.
pub trait ErasedBundle: Send {
    fn apply(self: Box<Self>, world: &mut World);
}

impl<B: Bundle + Send + 'static> ErasedBundle for B {
    fn apply(self: Box<Self>, world: &mut World) {
        world.spawn_bundle(*self);
    }
}

/// Type-erased component — может быть вставлен в entity.
pub trait ErasedComponent: Send {
    fn apply(self: Box<Self>, world: &mut World, entity: Entity);
}

impl<T: Component + Send + 'static> ErasedComponent for T {
    fn apply(self: Box<Self>, world: &mut World, entity: Entity) {
        world.insert(entity, *self);
    }
}

type BundleBox    = Box<dyn ErasedBundle>;
type ComponentBox = Box<dyn ErasedComponent>;

// ── Typed command enum ─────────────────────────────────────────
//
// Вместо Box<dyn FnOnce(&mut World)> используем конкретный enum
// для часто используемых команд. Это уменьшает размер vtable
// и делает диспетчеризацию более предсказуемой.
// Vec<Command> — плотный массив, cache-friendly при apply.

enum Command {
    Spawn { bundle: BundleBox },
    Insert { entity: Entity, component: ComponentBox },
    Remove { entity: Entity, component_id: ComponentId },
    Despawn(Entity),
    SpawnFromTemplate { name: String, params: TemplateParams },
    Apply(Box<dyn FnOnce(&mut World) + Send>),
}

/// Очередь команд — буферизует structural changes для применения после итерации.
///
/// Все команды хранятся в плотном `Vec<Command>` — `Despawn` и `Remove`
/// без heap-аллокаций, `Spawn`/`Insert` с type-erased trait object.
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
        self.queue.push(Command::Spawn {
            bundle: Box::new(bundle),
        });
    }

    /// Добавить компонент к entity
    pub fn insert<T: Component + Send + 'static>(&mut self, entity: Entity, component: T) {
        self.queue.push(Command::Insert {
            entity,
            component: Box::new(component),
        });
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

    /// Создать entity из зарегистрированного шаблона с параметрами.
    ///
    /// # Пример
    /// ```ignore
    /// cmds.spawn_from_template("Monster", TemplateParams::new()
    ///     .with("speed", 10.0f32));
    /// ```
    pub fn spawn_from_template(&mut self, name: &str, params: TemplateParams) {
        self.queue.push(Command::SpawnFromTemplate {
            name:  name.to_string(),
            params,
        });
    }

    /// Создать entity из шаблона с параметрами по умолчанию.
    ///
    /// # Пример
    /// ```ignore
    /// cmds.spawn_template("Monster");
    /// ```
    pub fn spawn_template(&mut self, name: &str) {
        self.queue.push(Command::SpawnFromTemplate {
            name:  name.to_string(),
            params: TemplateParams::new(),
        });
    }

    /// Применить все накопленные команды к миру
    pub fn apply(&mut self, world: &mut World) {
        for cmd in self.queue.drain(..) {
            match cmd {
                Command::Spawn { bundle }          => { bundle.apply(world); }
                Command::Insert { entity, component } => { component.apply(world, entity); }
                Command::Remove { entity, component_id } => { world.remove_raw(entity, component_id); }
                Command::Despawn(entity)           => { world.despawn(entity); }
                Command::SpawnFromTemplate { name, params } => { world.spawn_from_template(&name, &params); }
                Command::Apply(f)                  => { f(world); }
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
