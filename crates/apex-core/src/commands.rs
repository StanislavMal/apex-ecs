use crate::{
    component::{Component, ComponentId},
    entity::Entity,
    template::TemplateParams,
    world::{Bundle, World},
};

use std::alloc::{alloc, dealloc, Layout};
use std::mem;

// ── Chunk-based bump arena для payload команд ────────────────────
//
// Вместо N отдельных Box<dyn Trait> аллокаций для Spawn/Insert,
// данные пишутся в единый bump-буфер. После apply() курсор сбрасывается,
// память переиспользуется без per-command free.

struct CommandArena {
    data: *mut u8,
    capacity: usize,
    cursor: usize,
}

impl CommandArena {
    fn new() -> Self {
        Self { data: std::ptr::null_mut(), capacity: 0, cursor: 0 }
    }

    /// Разместить T в арене, вернуть offset в байтах.
    fn alloc<T>(&mut self, val: T) -> u32 {
        let align = mem::align_of::<T>();
        let size  = mem::size_of::<T>();
        // alignment padding
        let start = ((self.cursor + align - 1) / align) * align;
        let end = start + size;
        if end > self.capacity {
            let new_cap = end.max(self.capacity * 2).max(4096);
            let new_data = unsafe {
                let ptr = alloc(Layout::from_size_align(new_cap, mem::align_of::<usize>()).unwrap());
                assert!(!ptr.is_null(), "CommandArena allocation failed");
                ptr
            };
            if !self.data.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(self.data, new_data, self.cursor);
                    dealloc(self.data, Layout::from_size_align(self.capacity, mem::align_of::<usize>()).unwrap());
                }
            }
            self.data = new_data;
            self.capacity = new_cap;
        }
        let ptr = unsafe { self.data.add(start) as *mut T };
        unsafe { ptr.write(val); }
        self.cursor = end;
        start as u32
    }

    fn get_ptr(&self, offset: u32) -> *mut u8 {
        assert!(!self.data.is_null(), "CommandArena: data is null");
        unsafe { self.data.add(offset as usize) }
    }

    fn reset(&mut self) {
        self.cursor = 0;
    }
}

impl Drop for CommandArena {
    fn drop(&mut self) {
        if !self.data.is_null() {
            unsafe { dealloc(self.data, Layout::from_size_align(self.capacity, mem::align_of::<usize>()).unwrap()); }
        }
    }
}

// ── Function pointer types ───────────────────────────────────────

type SpawnApply   = unsafe fn(*mut u8, &mut World);
type InsertApply  = unsafe fn(*mut u8, &mut World, Entity);
type RemoveApply  = unsafe fn(Entity, &mut World);
type DropFn       = unsafe fn(*mut u8);

// ── Typed command enum ───────────────────────────────────────────
//
// Spawn / Insert хранят typed payload в bump-арене вместо Box<dyn Trait>.
// Despawn / Remove / SpawnFromTemplate — inline, без аллокации.

enum Command {
    /// Spawn с данными в bump-арене (offset + apply fn)
    Spawn { offset: u32, apply: SpawnApply, drop: DropFn },
    /// Insert с данными в bump-арене (offset + apply fn)
    Insert { entity: Entity, offset: u32, apply: InsertApply, drop: DropFn },
    /// Remove — inline
    Remove { entity: Entity, component_id: ComponentId },
    /// Remove typed — без Box, через function pointer, не требует данных в арене
    RemoveTyped {
        entity: Entity,
        /// function pointer для вызова world.remove::<T>()
        remove_fn: RemoveApply,
    },
    /// Despawn — inline, без аллокации
    Despawn(Entity),
    /// SpawnFromTemplate — String уже на heap, но это исключение
    SpawnFromTemplate { name: String, params: TemplateParams },
    /// Произвольная команда — Box<dyn FnOnce>
    Apply(Box<dyn FnOnce(&mut World) + Send>),
}

/// Очередь команд — буферизует structural changes для применения после итерации.
///
/// Spawn и Insert используют chunk-based bump арену вместо per-command
/// Box<dyn Trait> аллокаций. При 10k+ команд выигрыш ~10k heap-аллокаций.
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
    arena: CommandArena,
}

impl Commands {
    pub fn new() -> Self {
        Self { queue: Vec::new(), arena: CommandArena::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { queue: Vec::with_capacity(cap), arena: CommandArena::new() }
    }

    /// Уничтожить entity — без аллокации, хранится inline в enum
    #[inline]
    pub fn despawn(&mut self, entity: Entity) {
        self.queue.push(Command::Despawn(entity));
    }

    /// Создать entity из Bundle — typed payload в bump-арене
    pub fn spawn_bundle<B: Bundle + Send + 'static>(&mut self, bundle: B) {
        unsafe fn apply_spawn<B: Bundle>(ptr: *mut u8, world: &mut World) {
                let bundle = std::ptr::read(ptr as *const B);
                world.spawn_bundle(bundle);
            }
        unsafe fn drop_typed<T>(ptr: *mut u8) {
            std::ptr::drop_in_place(ptr as *mut T);
        }
        let offset = self.arena.alloc(bundle);
        self.queue.push(Command::Spawn {
            offset,
            apply: apply_spawn::<B>,
            drop: drop_typed::<B>,
        });
    }

    /// Добавить компонент к entity — typed payload в bump-арене
    pub fn insert<T: Component + Send + 'static>(&mut self, entity: Entity, component: T) {
        unsafe fn apply_insert<T: Component>(ptr: *mut u8, world: &mut World, entity: Entity) {
                let component = std::ptr::read(ptr as *const T);
                world.insert(entity, component);
            }
        unsafe fn drop_typed<T>(ptr: *mut u8) {
            std::ptr::drop_in_place(ptr as *mut T);
        }
        let offset = self.arena.alloc(component);
        self.queue.push(Command::Insert {
            entity,
            offset,
            apply: apply_insert::<T>,
            drop: drop_typed::<T>,
        });
    }

    /// Удалить компонент у entity — typed variant, без Box-аллокации
    pub fn remove<T: Component + Send + 'static>(&mut self, entity: Entity) {
        // SAFETY: typed_remove::<T> вызывается только в apply() с корректным T.
        // function pointer не требует данных в bump-арене, entity передаётся напрямую.
        unsafe fn typed_remove<T: Component>(entity: Entity, world: &mut World) {
            world.remove::<T>(entity);
        }
        self.queue.push(Command::RemoveTyped {
            entity,
            remove_fn: typed_remove::<T>,
        });
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
                Command::Spawn { offset, apply, .. } => unsafe { apply(self.arena.get_ptr(offset), world); },
                Command::Insert { entity, offset, apply, .. } => unsafe { apply(self.arena.get_ptr(offset), world, entity); },
                Command::Remove { entity, component_id } => { world.remove_raw(entity, component_id); }
                // SAFETY: remove_fn — корректный function pointer, созданный в remove::<T>.
                // Вызов типа-специализированной функции world.remove::<T>(entity) безопасен,
                // т.к. T статически задан при создании команды.
                Command::RemoveTyped { entity, remove_fn } => unsafe { remove_fn(entity, world); },
                Command::Despawn(entity)           => { world.despawn(entity); }
                Command::SpawnFromTemplate { name, params } => { world.spawn_from_template(&name, &params); }
                Command::Apply(f)                  => { f(world); }
            }
        }
        self.arena.reset();
    }

    #[inline] pub fn len(&self) -> usize { self.queue.len() }
    #[inline] pub fn is_empty(&self) -> bool { self.queue.is_empty() }

    /// Очистить без применения — корректно дропает typed данные в арене
    pub fn clear(&mut self) {
        for cmd in self.queue.drain(..) {
            match cmd {
                Command::Spawn { offset, drop, .. } => unsafe { drop(self.arena.get_ptr(offset)); },
                Command::Insert { offset, drop, .. } => unsafe { drop(self.arena.get_ptr(offset)); },
                // RemoveTyped не хранит данных в bump-арене — ничего не надо дропать
                Command::RemoveTyped { .. } => {}
                _ => {}
            }
        }
        self.arena.reset();
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        // Дропаем typed данные в арене перед деаллокацией буфера
        for cmd in self.queue.drain(..) {
            match cmd {
                Command::Spawn { offset, drop, .. } => unsafe { drop(self.arena.get_ptr(offset)); },
                Command::Insert { offset, drop, .. } => unsafe { drop(self.arena.get_ptr(offset)); },
                // RemoveTyped не хранит данных в bump-арене — ничего не надо дропать
                Command::RemoveTyped { .. } => {}
                _ => {}
            }
        }
        // CommandArena::drop() деаллоцирует backing buffer
    }
}

impl Default for Commands {
    fn default() -> Self { Self::new() }
}
