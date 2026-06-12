use crate::{
    component::{Component, ComponentId, Tick},
    entity::Entity,
    relations::RelationKind,
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
        Self {
            data: std::ptr::null_mut(),
            capacity: 0,
            cursor: 0,
        }
    }

    /// Разместить T в арене, вернуть offset в байтах.
    ///
    /// # SAFETY
    /// При реаллокации существующие данные копируются через `copy_nonoverlapping`.
    /// Это безопасно только если `T` тривиально перемещаемо (не имеет `Drop`,
    /// ссылающегося на self). Для типов с Drop (String, Vec<T>) используйте
    /// размещение на куче (например, `Box::new()`) вне арены.
    /// На практике: `spawn` и `insert` передают данные в арену для последующего
    /// чтения через `std::ptr::read` и дропа через function pointer.
    fn alloc<T>(&mut self, val: T) -> u32 {
        let align = mem::align_of::<T>();
        let size = mem::size_of::<T>();
        if size == 0 {
            return 0;
        }
        let start = self.cursor.div_ceil(align) * align;
        let end = start + size;
        if end > self.capacity {
            let new_cap = end.max(self.capacity * 2).max(4096);
            let new_data = unsafe {
                let ptr =
                    alloc(Layout::from_size_align(new_cap, mem::align_of::<usize>()).unwrap());
                assert!(!ptr.is_null(), "CommandArena allocation failed");
                ptr
            };
            if !self.data.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(self.data, new_data, self.cursor);
                    dealloc(
                        self.data,
                        Layout::from_size_align(self.capacity, mem::align_of::<usize>()).unwrap(),
                    );
                }
            }
            self.data = new_data;
            self.capacity = new_cap;
        }
        let ptr = unsafe { self.data.add(start) as *mut T };
        unsafe {
            ptr.write(val);
        }
        self.cursor = end;
        start as u32
    }

    fn get_ptr(&self, offset: u32) -> *mut u8 {
        if self.data.is_null() {
            return std::ptr::NonNull::<u8>::dangling().as_ptr();
        }
        unsafe { self.data.add(offset as usize) }
    }

    fn reset(&mut self) {
        self.cursor = 0;
    }
}

impl Drop for CommandArena {
    fn drop(&mut self) {
        if !self.data.is_null() {
            unsafe {
                dealloc(
                    self.data,
                    Layout::from_size_align(self.capacity, mem::align_of::<usize>()).unwrap(),
                );
            }
        }
    }
}

// ── Function pointer types ───────────────────────────────────────

type SpawnApply = unsafe fn(*mut u8, &mut World);
type InsertApply = unsafe fn(*mut u8, &mut World, Entity);
type RemoveApply = unsafe fn(Entity, &mut World);
type DropFn = unsafe fn(*mut u8);
type AddRelationApply = fn(&mut World, Entity, Entity);
type RemoveRelationApply = fn(&mut World, Entity, Entity);
/// Разрешить/зарегистрировать ComponentId типа команды — для группового
/// применения insert-бёрстов (W2-1): id нужен ДО вызова typed-apply.
type ComponentIdFn = fn(&mut crate::component::ComponentRegistry) -> ComponentId;

// ── Typed command enum ───────────────────────────────────────────
//
// Spawn / Insert хранят typed payload в bump-арене вместо Box<dyn Trait>.
// Despawn / Remove / SpawnFromTemplate — inline, без аллокации.

enum Command {
    /// Spawn с данными в bump-арене (offset + apply fn)
    Spawn {
        offset: u32,
        apply: SpawnApply,
        drop: DropFn,
    },
    /// Insert с данными в bump-арене (offset + apply fn)
    Insert {
        entity: Entity,
        offset: u32,
        apply: InsertApply,
        drop: DropFn,
        /// Для группового применения бёрста insert'ов одной entity (W2-1).
        cid_fn: ComponentIdFn,
    },
    /// Remove — inline
    Remove {
        entity: Entity,
        component_id: ComponentId,
    },
    /// Remove typed — без Box, через function pointer, не требует данных в арене
    RemoveTyped {
        entity: Entity,
        /// function pointer для вызова world.remove::<T>()
        remove_fn: RemoveApply,
    },
    /// InsertRaw — вставка по ComponentId с сырыми данными (Vec<u8>)
    InsertRaw {
        entity: Entity,
        component_id: ComponentId,
        data: Vec<u8>,
        tick: Tick,
    },
    /// Despawn — inline, без аллокации
    Despawn(Entity),
    /// SpawnFromTemplate — String уже на heap, но это исключение
    SpawnFromTemplate {
        name: String,
        params: TemplateParams,
    },
    /// Произвольная команда — Box<dyn FnOnce>
    Apply(Box<dyn FnOnce(&mut World) + Send>),
    /// AddRelation — typed, через function pointer
    AddRelation {
        subject: Entity,
        target: Entity,
        apply: AddRelationApply,
    },
    /// RemoveRelation — typed, через function pointer
    RemoveRelation {
        subject: Entity,
        target: Entity,
        apply: RemoveRelationApply,
    },
}

/// Entity-цель insert-подобной команды — критерий группировки бёрстов (W2-1).
fn insert_target(cmd: &Command) -> Option<Entity> {
    match cmd {
        Command::Insert { entity, .. } | Command::InsertRaw { entity, .. } => Some(*entity),
        _ => None,
    }
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
        Self {
            queue: Vec::new(),
            arena: CommandArena::new(),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            queue: Vec::with_capacity(cap),
            arena: CommandArena::new(),
        }
    }

    /// Уничтожить entity — без аллокации, хранится inline в enum
    #[inline]
    pub fn despawn(&mut self, entity: Entity) {
        self.queue.push(Command::Despawn(entity));
    }

    /// Создать entity из Bundle — typed payload в bump-арене
    pub fn spawn<B: Bundle + Send + 'static>(&mut self, bundle: B) {
        unsafe fn apply_spawn<B: Bundle>(ptr: *mut u8, world: &mut World) {
            let bundle = std::ptr::read(ptr as *const B);
            world.spawn(bundle);
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
        fn cid<T: Component>(registry: &mut crate::component::ComponentRegistry) -> ComponentId {
            registry.get_or_register::<T>()
        }
        let offset = self.arena.alloc(component);
        self.queue.push(Command::Insert {
            entity,
            offset,
            apply: apply_insert::<T>,
            drop: drop_typed::<T>,
            cid_fn: cid::<T>,
        });
    }

    /// Вставить компонент по ComponentId с сырыми данными (raw).
    /// Используется когда ComponentId известен динамически (не через тип).
    pub fn insert_raw(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        data: Vec<u8>,
        tick: Tick,
    ) {
        self.queue.push(Command::InsertRaw {
            entity,
            component_id,
            data,
            tick,
        });
    }

    /// Удалить компонент по ComponentId (raw).
    /// Используется когда ComponentId известен динамически (не через тип).
    pub fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        self.queue.push(Command::Remove {
            entity,
            component_id,
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
    /// Отложенно вставить ресурс (1:1 Bevy `Commands::insert_resource`).
    /// Применяется в sync-точке вместе с остальными командами.
    pub fn insert_resource<T: Send + Sync + 'static>(&mut self, resource: T) {
        self.add(move |world: &mut World| world.insert_resource(resource));
    }

    pub fn add<F: FnOnce(&mut World) + Send + 'static>(&mut self, f: F) {
        self.queue.push(Command::Apply(Box::new(f)));
    }

    /// Добавить relation между subject и target.
    ///
    /// Выполняется отложенно при `apply()` — безопасно в параллельных системах.
    pub fn add_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        fn apply<R: RelationKind>(world: &mut World, subject: Entity, target: Entity) {
            // SAFETY: R is a ZST (all RelationKind impls are unit structs with Copy)
            let kind: R = unsafe { std::mem::zeroed() };
            world.add_relation(subject, kind, target);
        }
        self.queue.push(Command::AddRelation {
            subject,
            target,
            apply: apply::<R>,
        });
    }

    /// Удалить relation между subject и target.
    ///
    /// Выполняется отложенно при `apply()`.
    pub fn remove_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        fn apply<R: RelationKind>(world: &mut World, subject: Entity, target: Entity) {
            // SAFETY: R is a ZST (all RelationKind impls are unit structs with Copy)
            let kind: R = unsafe { std::mem::zeroed() };
            world.remove_relation(subject, kind, target);
        }
        self.queue.push(Command::RemoveRelation {
            subject,
            target,
            apply: apply::<R>,
        });
    }

    /// Массовое добавление relation от множества subject'ов к одному target.
    ///
    /// Оптимизировано через `World::add_relation_batch`.
    pub fn add_relation_batch<R: RelationKind + Send + 'static>(
        &mut self,
        subjects: Vec<Entity>,
        _kind: R,
        target: Entity,
    ) {
        self.add(move |world| {
            let kind: R = unsafe { std::mem::zeroed() };
            world.add_relation_batch(&subjects, kind, target);
        });
    }

    /// Создать entity из зарегистрированного шаблона с параметрами.
    ///
    /// # Пример
    /// ```ignore
    /// struct MonsterSpeed;
    /// impl apex_core::template::TemplateParam for MonsterSpeed { type Value = f32; }
    ///
    /// cmds.spawn_from_template("Monster", TemplateParams::new()
    ///     .set::<MonsterSpeed>(10.0f32));
    /// ```
    pub fn spawn_from_template(&mut self, name: &str, params: TemplateParams) {
        self.queue.push(Command::SpawnFromTemplate {
            name: name.to_string(),
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
            name: name.to_string(),
            params: TemplateParams::new(),
        });
    }

    /// Применить все накопленные команды к миру.
    ///
    /// Бёрст ПОДРЯД идущих insert'ов на одну entity (`insert(e, A);
    /// insert(e, B); …`) применяется ГРУППОЙ — один archetype move на пачку
    /// вместо move-на-компонент (W2-1, `World::insert_parts`). Порядок
    /// применения команд сохраняется.
    pub fn apply(&mut self, world: &mut World) {
        let queue = std::mem::take(&mut self.queue);
        let mut it = queue.into_iter().peekable();
        // Переиспользуемые буферы группы (вне цикла — без реаллокаций).
        let mut group: smallvec::SmallVec<[Command; 8]> = smallvec::SmallVec::new();
        let mut parts: smallvec::SmallVec<[(ComponentId, *const u8, Tick); 8]> =
            smallvec::SmallVec::new();

        while let Some(cmd) = it.next() {
            if let Some(entity) = insert_target(&cmd) {
                if it.peek().and_then(insert_target) == Some(entity) {
                    group.clear();
                    group.push(cmd);
                    while it.peek().and_then(insert_target) == Some(entity) {
                        group.push(it.next().unwrap());
                    }
                    self.apply_insert_group(world, entity, &mut group, &mut parts);
                    continue;
                }
            }
            self.apply_one(cmd, world);
        }
        self.arena.reset();
    }

    /// Применить группу insert/insert_raw одной entity одним archetype move.
    fn apply_insert_group(
        &self,
        world: &mut World,
        entity: Entity,
        group: &mut smallvec::SmallVec<[Command; 8]>,
        parts: &mut smallvec::SmallVec<[(ComponentId, *const u8, Tick); 8]>,
    ) {
        let tick = world.current_tick();
        parts.clear();
        for cmd in group.iter() {
            match cmd {
                Command::Insert { offset, cid_fn, .. } => {
                    let cid = cid_fn(&mut world.registry);
                    parts.push((cid, self.arena.get_ptr(*offset) as *const u8, tick));
                }
                Command::InsertRaw {
                    component_id,
                    data,
                    tick,
                    ..
                } => {
                    parts.push((*component_id, data.as_ptr(), *tick));
                }
                _ => unreachable!("в insert-группе только Insert/InsertRaw"),
            }
        }

        if world.insert_parts(entity, parts) {
            // Значения скопированы во владение мира: typed payload'ы НЕ дропаем
            // (эквивалент ptr::read + forget), Vec<u8> из InsertRaw — лишь байты.
            group.clear();
        } else {
            // Entity мертва — освобождаем typed payload'ы, как world.insert
            // дропал бы значение на раннем return.
            for cmd in group.drain(..) {
                if let Command::Insert { offset, drop, .. } = cmd {
                    unsafe { drop(self.arena.get_ptr(offset)) };
                }
            }
        }
    }

    /// Применить одну команду (вне групп).
    fn apply_one(&self, cmd: Command, world: &mut World) {
        {
            match cmd {
                Command::Spawn { offset, apply, .. } => unsafe {
                    apply(self.arena.get_ptr(offset), world);
                },
                Command::Insert {
                    entity,
                    offset,
                    apply,
                    ..
                } => unsafe {
                    apply(self.arena.get_ptr(offset), world, entity);
                },
                Command::Remove {
                    entity,
                    component_id,
                } => {
                    world.remove_raw(entity, component_id);
                }
                // SAFETY: remove_fn — корректный function pointer, созданный в remove::<T>.
                // Вызов типа-специализированной функции world.remove::<T>(entity) безопасен,
                // т.к. T статически задан при создании команды.
                Command::RemoveTyped { entity, remove_fn } => unsafe {
                    remove_fn(entity, world);
                },
                Command::InsertRaw {
                    entity,
                    component_id,
                    data,
                    tick,
                } => {
                    world.insert_raw(entity, component_id, data, tick);
                }
                Command::Despawn(entity) => {
                    world.despawn(entity);
                }
                Command::SpawnFromTemplate { name, params } => {
                    world.spawn_from_template(&name, &params);
                }
                Command::Apply(f) => {
                    f(world);
                }
                Command::AddRelation {
                    subject,
                    target,
                    apply,
                } => {
                    apply(world, subject, target);
                }
                Command::RemoveRelation {
                    subject,
                    target,
                    apply,
                } => {
                    apply(world, subject, target);
                }
            }
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Очистить без применения — корректно дропает typed данные в арене
    pub fn clear(&mut self) {
        for cmd in self.queue.drain(..) {
            match cmd {
                Command::Spawn { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                Command::Insert { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                // RemoveTyped / Remove / InsertRaw не хранят данных в bump-арене — ничего не надо дропать
                Command::RemoveTyped { .. } => {}
                Command::Remove { .. } => {}
                Command::InsertRaw { .. } => {}
                Command::AddRelation { .. } => {}
                Command::RemoveRelation { .. } => {}
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
                Command::Spawn { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                Command::Insert { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                // RemoveTyped / Remove / InsertRaw не хранят данных в bump-арене — ничего не надо дропать
                Command::RemoveTyped { .. } => {}
                Command::Remove { .. } => {}
                Command::InsertRaw { .. } => {}
                Command::AddRelation { .. } => {}
                Command::RemoveRelation { .. } => {}
                _ => {}
            }
        }
        // CommandArena::drop() деаллоцирует backing buffer
    }
}

impl Default for Commands {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;
    use crate::entity::Entity;

    #[derive(Clone, Copy)]
    struct Pos(f32);
    impl Component for Pos {}

    #[derive(Clone, Copy)]
    struct Vel(f32);
    impl Component for Vel {}

    // ── Basic commands ─────────────────────────────────────────

    #[test]
    fn commands_spawn_entity() {
        let mut world = World::new();
        let mut cmds = Commands::new();

        cmds.spawn((Pos(1.0),));
        assert_eq!(cmds.len(), 1);
        cmds.apply(&mut world);
        assert_eq!(cmds.len(), 0);

        // Проверяем что entity создался с компонентом
        let query = crate::query::Query::<crate::query::Read<Pos>>::new(&world);
        let mut count = 0;
        query.for_each(|_, pos| {
            count += 1;
            assert_eq!(pos.0, 1.0);
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn commands_insert_and_remove_component() {
        let mut world = World::new();
        let entity = world.spawn((Pos(0.0),));

        let mut cmds = Commands::new();
        cmds.insert(entity, Vel(5.0));
        cmds.apply(&mut world);

        let vel = world.get::<Vel>(entity).unwrap();
        assert_eq!(vel.0, 5.0);

        cmds.remove::<Vel>(entity);
        cmds.apply(&mut world);
        assert!(world.get::<Vel>(entity).is_none());
    }

    #[test]
    fn commands_despawn_entity() {
        let mut world = World::new();
        let entity = world.spawn((Pos(0.0),));

        let mut cmds = Commands::new();
        cmds.despawn(entity);
        cmds.apply(&mut world);

        assert!(world.get::<Pos>(entity).is_none());
    }

    #[test]
    fn commands_insert_raw_and_remove_raw() {
        let mut world = World::new();
        world.register_component::<Vel>();
        let vel_id = world.registry().get_id::<Vel>().unwrap();

        let entity = world.spawn((Pos(0.0),));

        let mut cmds = Commands::new();
        // insert_raw с сырыми байтами
        let vel_val: Vel = Vel(7.0);
        let data = unsafe {
            let ptr = &vel_val as *const Vel as *const u8;
            std::slice::from_raw_parts(ptr, std::mem::size_of::<Vel>()).to_vec()
        };
        let tick = world.current_tick();
        cmds.insert_raw(entity, vel_id, data, tick);
        cmds.apply(&mut world);

        let vel = world.get::<Vel>(entity).unwrap();
        assert_eq!(vel.0, 7.0);

        cmds.remove_raw(entity, vel_id);
        cmds.apply(&mut world);
        assert!(world.get::<Vel>(entity).is_none());
    }

    #[test]
    fn commands_apply_clears_queue() {
        let mut world = World::new();
        let mut cmds = Commands::new();

        cmds.spawn((Pos(0.0),));
        cmds.spawn((Pos(1.0),));
        assert_eq!(cmds.len(), 2);

        cmds.apply(&mut world);
        assert_eq!(cmds.len(), 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn commands_clear_drops_without_apply() {
        let mut cmds = Commands::new();
        cmds.spawn((Pos(1.0),));
        cmds.spawn((Pos(2.0),));
        assert_eq!(cmds.len(), 2);

        cmds.clear();
        assert_eq!(cmds.len(), 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn commands_spawn_from_template() {
        use crate::entity::Entity;
        use crate::template::{EntityTemplate, TemplateParams};

        #[derive(Clone)]
        struct TestTemplate;
        impl Component for TestTemplate {}

        impl EntityTemplate for TestTemplate {
            fn spawn(&self, world: &mut World, _params: &TemplateParams) -> Entity {
                world.spawn((Pos(99.0),))
            }
        }

        let mut world = World::new();
        world.register_template("test", TestTemplate);

        let mut cmds = Commands::new();
        cmds.spawn_template("test");
        cmds.apply(&mut world);

        let query = crate::query::Query::<crate::query::Read<Pos>>::new(&world);
        let mut count = 0;
        query.for_each(|_, pos| {
            count += 1;
            assert_eq!(pos.0, 99.0);
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn commands_spawn_from_template_with_params() {
        use crate::query::Read;
        use crate::template::{EntityTemplate, TemplateParam, TemplateParams};

        struct ParamVal;
        impl TemplateParam for ParamVal {
            type Value = f32;
        }

        struct ParamTemplate {
            default: f32,
        }
        impl Component for ParamTemplate {}

        impl EntityTemplate for ParamTemplate {
            fn spawn(&self, world: &mut World, params: &TemplateParams) -> Entity {
                let val = params.get::<ParamVal>().copied().unwrap_or(self.default);
                world.spawn((Pos(val),))
            }
        }

        let mut world = World::new();
        world.register_component::<Pos>();
        world.register_template("param_test", ParamTemplate { default: 5.0 });

        let mut cmds = Commands::new();
        cmds.spawn_from_template("param_test", TemplateParams::new().set::<ParamVal>(42.0f32));
        cmds.apply(&mut world);

        let query = crate::query::Query::<Read<Pos>>::new(&world);
        let mut found = None;
        query.for_each(|_, pos| found = Some(pos.0));
        assert_eq!(found, Some(42.0));
    }

    // ── Edge cases ─────────────────────────────────────────────

    #[test]
    fn commands_custom_add_fn() {
        let mut world = World::new();

        let mut cmds = Commands::new();
        cmds.add(|w| {
            w.insert_resource(Pos(100.0));
        });
        cmds.apply(&mut world);

        let res = world.resource::<Pos>();
        assert_eq!(res.0, 100.0);
    }

    #[test]
    fn commands_empty_apply_noop() {
        let mut world = World::new();
        let mut cmds = Commands::new();
        cmds.apply(&mut world); // не должно паниковать
        assert!(cmds.is_empty());
    }

    #[test]
    fn commands_with_capacity() {
        let cmds = Commands::with_capacity(1000);
        assert_eq!(cmds.len(), 0);
        assert!(cmds.is_empty());
    }

    #[test]
    fn commands_multiple_spawns() {
        let mut world = World::new();
        let mut cmds = Commands::new();

        for i in 0..100 {
            cmds.spawn((Pos(i as f32),));
        }
        cmds.apply(&mut world);

        let query = crate::query::Query::<crate::query::Read<Pos>>::new(&world);
        let mut values: Vec<f32> = Vec::new();
        query.for_each(|_, pos| values.push(pos.0));
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(values.len(), 100);
        assert_eq!(values[0], 0.0);
        assert_eq!(values[99], 99.0);
    }

    #[test]
    fn commands_arena_reuse() {
        let mut world = World::new();
        let mut cmds = Commands::new();

        // Первый цикл: spawn много entity
        for _ in 0..10 {
            cmds.spawn((Pos(1.0),));
        }
        cmds.apply(&mut world);
        assert_eq!(cmds.len(), 0);

        // Второй цикл: arena уже выделена, переиспользуется без аллокаций
        for _ in 0..10 {
            cmds.spawn((Pos(2.0),));
        }
        cmds.apply(&mut world);
        assert_eq!(cmds.len(), 0);

        let query = crate::query::Query::<crate::query::Read<Pos>>::new(&world);
        let count = query.iter().count();
        assert_eq!(count, 20);
    }

    // ── W2-1: группировка insert-бёрстов ───────────────────────

    #[derive(Clone, Copy)]
    struct Acc(f32);
    impl Component for Acc {}
    #[derive(Clone, Copy)]
    struct Hp(f32);
    impl Component for Hp {}

    /// Бёрст insert'ов на одну entity применяется группой: один archetype
    /// move, все компоненты на месте, значения корректны.
    #[test]
    fn commands_insert_burst_grouped_single_move() {
        let mut world = World::new();
        let entity = world.spawn((Pos(0.0),));

        let mut cmds = Commands::new();
        cmds.insert(entity, Vel(1.0));
        cmds.insert(entity, Acc(2.0));
        cmds.insert(entity, Hp(3.0));
        cmds.apply(&mut world);

        assert_eq!(world.get::<Vel>(entity).unwrap().0, 1.0);
        assert_eq!(world.get::<Acc>(entity).unwrap().0, 2.0);
        assert_eq!(world.get::<Hp>(entity).unwrap().0, 3.0);
        assert_eq!(world.get::<Pos>(entity).unwrap().0, 0.0, "старый компонент пережил move");
    }

    /// Дубликат компонента в группе: выживает ПОСЛЕДНЕЕ значение (порядок
    /// команд сохраняется), промежуточное дропается.
    #[test]
    fn commands_insert_burst_duplicate_last_wins() {
        let mut world = World::new();
        let entity = world.spawn((Pos(0.0),));

        let mut cmds = Commands::new();
        cmds.insert(entity, Vel(1.0));
        cmds.insert(entity, Vel(2.0));
        cmds.insert(entity, Acc(9.0));
        cmds.apply(&mut world);

        assert_eq!(world.get::<Vel>(entity).unwrap().0, 2.0);
        assert_eq!(world.get::<Acc>(entity).unwrap().0, 9.0);
    }

    /// Группа на МЁРТВУЮ entity: payload'ы освобождаются (нет ни паники,
    /// ни утечки), мир не меняется.
    #[test]
    fn commands_insert_burst_dead_entity_drops_payloads() {
        use std::sync::Arc;

        struct Holder(#[allow(dead_code)] Arc<()>);
        impl Component for Holder {}

        let mut world = World::new();
        let entity = world.spawn((Pos(0.0),));
        world.despawn(entity);

        let probe = Arc::new(());
        let mut cmds = Commands::new();
        cmds.insert(entity, Holder(probe.clone()));
        cmds.insert(entity, Holder(probe.clone()));
        cmds.apply(&mut world);

        assert_eq!(Arc::strong_count(&probe), 1, "payload'ы мёртвой entity дропнуты");
    }

    /// Группировка не ломает порядок с другими командами: insert'ы до
    /// despawn применяются, despawn после — убивает entity.
    #[test]
    fn commands_insert_burst_then_despawn_order_preserved() {
        let mut world = World::new();
        let entity = world.spawn((Pos(0.0),));

        let mut cmds = Commands::new();
        cmds.insert(entity, Vel(1.0));
        cmds.insert(entity, Acc(2.0));
        cmds.despawn(entity);
        cmds.apply(&mut world);

        assert!(world.get::<Pos>(entity).is_none(), "despawn после бёрста применён");
    }

    /// W2-1 фикс утечки: insert ПОВЕРХ существующего компонента дропает
    /// старое значение (и в группе, и поодиночке).
    #[test]
    fn commands_insert_overwrite_drops_old_value() {
        use std::sync::Arc;

        struct Holder(#[allow(dead_code)] Arc<()>);
        impl Component for Holder {}

        let probe = Arc::new(());
        let mut world = World::new();
        let entity = world.spawn((Holder(probe.clone()),));
        assert_eq!(Arc::strong_count(&probe), 2);

        // Одиночный insert поверх существующего
        world.insert(entity, Holder(probe.clone()));
        assert_eq!(Arc::strong_count(&probe), 2, "старое значение дропнуто (одиночный путь)");

        // Групповой путь (бёрст с дубликатом поверх существующего)
        let mut cmds = Commands::new();
        cmds.insert(entity, Holder(probe.clone()));
        cmds.insert(entity, Holder(probe.clone()));
        cmds.apply(&mut world);
        assert_eq!(Arc::strong_count(&probe), 2, "групповой путь дропает перезаписанные");

        world.despawn(entity);
        assert_eq!(Arc::strong_count(&probe), 1);
    }

    #[test]
    fn commands_preserves_tick() {
        let mut world = World::new();
        let mut cmds = Commands::new();

        let entity = world.spawn((Pos(0.0),));
        let _tick_before = world.current_tick();

        // Делаем несколько изменений
        cmds.insert(entity, Vel(5.0));
        cmds.apply(&mut world);

        let vel = world.get::<Vel>(entity).unwrap();
        assert_eq!(vel.0, 5.0);
    }
}
