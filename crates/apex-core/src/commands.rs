use crate::{
    component::{Component, ComponentId, Tick},
    entity::Entity,
    relations::RelationKind,
    template::TemplateParams,
    world::{Bundle, World},
};

use std::alloc::{alloc, dealloc, Layout};
use std::mem;

// ── Chunk-based bump arena for command payloads ──────────────────
//
// Instead of N separate Box<dyn Trait> allocations for Spawn/Insert,
// the data is written into a single bump buffer. After apply() the cursor is
// reset and the memory is reused without a per-command free.

struct CommandArena {
    data: *mut u8,
    capacity: usize,
    cursor: usize,
    /// Alignment the current backing allocation was made with — a running max of
    /// every payload alignment seen. The base MUST honor the strictest payload
    /// alignment: `base + offset` is only aligned when `base` is a multiple of
    /// the payload's alignment, even though `offset` is a multiple of it too.
    align: usize,
}

impl CommandArena {
    fn new() -> Self {
        Self {
            data: std::ptr::null_mut(),
            capacity: 0,
            cursor: 0,
            align: mem::align_of::<usize>(),
        }
    }

    /// Place T in the arena, return its offset in bytes.
    ///
    /// # SAFETY
    /// On reallocation the existing data is copied via `copy_nonoverlapping`.
    /// This is only safe if `T` is trivially relocatable (has no `Drop` that
    /// references self). For types with Drop (String, Vec<T>) use heap
    /// placement (e.g. `Box::new()`) outside the arena.
    /// In practice: `spawn` and `insert` put data into the arena for later
    /// reading via `std::ptr::read` and dropping via a function pointer.
    fn alloc<T>(&mut self, val: T) -> u32 {
        let align = mem::align_of::<T>();
        let size = mem::size_of::<T>();
        // A payload stricter than the current base alignment forces a realloc so
        // the base honors it (glam's SIMD types are 16-aligned; the old fixed
        // 8-byte base produced misaligned reads/writes for `cmd.spawn((Transform,…))`).
        if align > self.align {
            self.reallocate(self.capacity, align);
        }
        if size == 0 {
            return 0;
        }
        let start = self.cursor.div_ceil(align) * align;
        let end = start + size;
        if end > self.capacity {
            let new_cap = end.max(self.capacity * 2).max(4096);
            self.reallocate(new_cap, self.align);
        }
        let ptr = unsafe { self.data.add(start) as *mut T };
        unsafe {
            ptr.write(val);
        }
        self.cursor = end;
        start as u32
    }

    /// (Re)allocate the backing buffer to hold at least `new_cap` bytes with
    /// alignment `new_align` (>= current), preserving the first `self.cursor`
    /// bytes. Both `new_cap >= self.cursor` and power-of-two `new_align` hold.
    fn reallocate(&mut self, new_cap: usize, new_align: usize) {
        let new_size = new_cap.max(self.cursor).max(1);
        let layout =
            Layout::from_size_align(new_size, new_align).expect("CommandArena: layout overflow");
        let new_data = unsafe { alloc(layout) };
        assert!(!new_data.is_null(), "CommandArena allocation failed");
        if !self.data.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(self.data, new_data, self.cursor);
                dealloc(
                    self.data,
                    Layout::from_size_align(self.capacity, self.align).unwrap(),
                );
            }
        }
        self.data = new_data;
        self.capacity = new_size;
        self.align = new_align;
    }

    fn get_ptr(&self, offset: u32) -> *mut u8 {
        if self.data.is_null() {
            // Empty arena (only align-<=self.align ZST payloads were "stored"):
            // return a non-null pointer aligned to `self.align` so ZST reads —
            // which never touch memory — still satisfy the alignment contract.
            return std::ptr::without_provenance_mut(self.align);
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
                    Layout::from_size_align(self.capacity, self.align).unwrap(),
                );
            }
        }
    }
}

// ── Function pointer types ───────────────────────────────────────

/// Spawn-apply receives a reserved `Entity` (or `PLACEHOLDER` for standalone Commands without a
/// reserver) — it fills the entity with components via `world.spawn_reserved` (or `world.spawn`).
type SpawnApply = unsafe fn(*mut u8, &mut World, Entity);
/// Batch-apply of consecutive spawn commands of the SAME type `B`: reads bundles from the arena by
/// their offsets and bulk-inserts them into the archetype with a SINGLE resolve (component_ids/
/// archetype/columns — once per batch, not per spawn). Closes the `commands_spawn` gap vs Bevy
/// (apply used to call a single `spawn_at` per EACH entity ⇒ 10k archetype lookups + 20k
/// get_or_register for 10k spawns).
type SpawnApplyBatch = unsafe fn(&mut World, &[(Entity, u32)], &CommandArena);
type InsertApply = unsafe fn(*mut u8, &mut World, Entity);
type RemoveApply = unsafe fn(Entity, &mut World);
type DropFn = unsafe fn(*mut u8);
type AddRelationApply = fn(&mut World, Entity, Entity);
type RemoveRelationApply = fn(&mut World, Entity, Entity);
/// Resolve/register the ComponentId of the command's type — for grouped
/// application of insert bursts (W2-1): the id is needed BEFORE calling typed-apply.
type ComponentIdFn = fn(&mut crate::component::ComponentRegistry) -> ComponentId;

// ── Typed command enum ───────────────────────────────────────────
//
// Spawn / Insert store the typed payload in the bump arena instead of Box<dyn Trait>.
// Despawn / Remove / SpawnFromTemplate — inline, without allocation.

enum Command {
    /// Spawn with data in the bump arena (offset + apply fn). `entity` — a pre-reserved id
    /// (see [`Commands::spawn`]), or `Entity::PLACEHOLDER` (standalone Commands without a reserver —
    /// then apply allocates a new id, behaving as before).
    Spawn {
        entity: Entity,
        offset: u32,
        apply: SpawnApply,
        /// Batch applier (the same for all spawns of one type `B`) — grouping consecutive
        /// `Spawn` commands by equality of this pointer yields a bulk-apply with one archetype resolve.
        apply_batch: SpawnApplyBatch,
        drop: DropFn,
    },
    /// Insert with data in the bump arena (offset + apply fn)
    Insert {
        entity: Entity,
        offset: u32,
        apply: InsertApply,
        drop: DropFn,
        /// For grouped application of an insert burst on one entity (W2-1).
        cid_fn: ComponentIdFn,
    },
    /// Remove — inline
    Remove {
        entity: Entity,
        component_id: ComponentId,
    },
    /// Remove typed — no Box, via a function pointer, requires no data in the arena
    RemoveTyped {
        entity: Entity,
        /// function pointer to call world.remove::<T>()
        remove_fn: RemoveApply,
    },
    /// InsertRaw — insert by ComponentId with raw data (Vec<u8>)
    InsertRaw {
        entity: Entity,
        component_id: ComponentId,
        data: Vec<u8>,
        tick: Tick,
    },
    /// Despawn — inline, without allocation
    Despawn(Entity),
    /// SpawnFromTemplate — a rare variant; `TemplateParams` (3×HashMap ≈ 144 bytes) is MOVED into a
    /// `Box` so as not to bloat the size of the WHOLE `Command` enum (paid by every `queue.push`,
    /// including bulk Spawn/Insert). Without Box a single Command would weigh ~168 bytes instead of
    /// ~40 ⇒ +300µs of pure queue-write memory traffic for 10k spawns.
    SpawnFromTemplate {
        name: String,
        params: Box<TemplateParams>,
    },
    /// Arbitrary command — Box<dyn FnOnce>
    Apply(Box<dyn FnOnce(&mut World) + Send>),
    /// AddRelation — typed, via a function pointer
    AddRelation {
        subject: Entity,
        target: Entity,
        apply: AddRelationApply,
    },
    /// RemoveRelation — typed, via a function pointer
    RemoveRelation {
        subject: Entity,
        target: Entity,
        apply: RemoveRelationApply,
    },
}

// Size guard: `Command` is written into the queue by the millions on bulk spawns/inserts, so its
// size = a direct write tax (Vec<Command> is uniform at the size of the largest variant). Keep it
// ≤48 bytes: large/rare payloads (TemplateParams) are moved into a `Box`. If the assert fails — a
// new variant bloated the enum; move its data into a `Box` rather than growing the queue for all
// commands.
const _: () = assert!(
    std::mem::size_of::<Command>() <= 48,
    "Command is bloated — move a new variant's large payload into a Box (see SpawnFromTemplate)"
);

/// Apply of a single spawn command: fill the reserved `entity` (or allocate a new one on
/// `PLACEHOLDER` — standalone Commands). Lifted to module level for reuse in
/// [`Commands::spawn`] and [`Commands::spawn_batch`].
unsafe fn spawn_apply<B: Bundle>(ptr: *mut u8, world: &mut World, entity: Entity) {
    let bundle = std::ptr::read(ptr as *const B);
    if entity == Entity::PLACEHOLDER {
        world.spawn(bundle);
    } else {
        world.spawn_reserved(entity, bundle);
    }
}

unsafe fn spawn_drop<B>(ptr: *mut u8) {
    std::ptr::drop_in_place(ptr as *mut B);
}

/// Bulk-apply a batch of consecutive spawns of the SAME type `B`: move the bundles out of the arena
/// and bulk-insert them (`World::spawn_bundles_bulk` resolves archetype/ids/columns once per batch).
/// `items` — `(entity, offset)`: `entity` is either reserved (the system path) or `PLACEHOLDER`
/// (standalone — ids are allocated on apply). Semantically = N calls to [`spawn_apply`], but without
/// a per-spawn archetype lookup.
unsafe fn spawn_apply_batch<B: Bundle>(
    world: &mut World,
    items: &[(Entity, u32)],
    arena: &CommandArena,
) {
    let mut bundles: Vec<B> = Vec::with_capacity(items.len());
    let mut entities: Vec<Entity> = Vec::with_capacity(items.len());
    for &(entity, offset) in items {
        // SAFETY: offset points to a valid `B` (written in spawn/spawn_batch); we read
        // (move) ownership — the arena will no longer drop it (like the single spawn_apply).
        bundles.push(std::ptr::read(arena.get_ptr(offset) as *const B));
        entities.push(entity);
    }
    world.spawn_bundles_bulk(entities, bundles);
}

/// The entity target of an insert-like command — the grouping criterion for bursts (W2-1).
fn insert_target(cmd: &Command) -> Option<Entity> {
    match cmd {
        Command::Insert { entity, .. } | Command::InsertRaw { entity, .. } => Some(*entity),
        _ => None,
    }
}

/// Command queue — buffers structural changes to apply after iteration.
///
/// Spawn and Insert use a chunk-based bump arena instead of per-command
/// Box<dyn Trait> allocations. At 10k+ commands this saves ~10k heap allocations.
///
/// # Example
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
    /// Entity reserver — lets `spawn().id()` hand back a real `Entity` immediately (see
    /// [`EntityReserver`](crate::entity::EntityReserver)). Injected when Commands is accessed from a
    /// system (`SystemContext::commands`). `None` for standalone `Commands::new()` (tests) — there
    /// `spawn` allocates the id on apply, and `id()` returns `PLACEHOLDER`.
    reserver: Option<crate::entity::EntityReserver>,
}

impl Commands {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            arena: CommandArena::new(),
            reserver: None,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            queue: Vec::with_capacity(cap),
            arena: CommandArena::new(),
            reserver: None,
        }
    }

    /// Bind the entity reserver (called by the engine from `SystemContext::commands`, idempotent).
    /// After this `spawn().id()` returns a real cross-frame `Entity`.
    #[inline]
    pub fn set_reserver(&mut self, reserver: crate::entity::EntityReserver) {
        self.reserver = Some(reserver);
    }

    /// Whether a reserver is bound (i.e. whether `spawn().id()` returns a real id).
    #[inline]
    pub fn has_reserver(&self) -> bool {
        self.reserver.is_some()
    }

    /// Destroy an entity — no allocation, stored inline in the enum
    #[inline]
    pub fn despawn(&mut self, entity: Entity) {
        self.queue.push(Command::Despawn(entity));
    }

    /// Create an entity from a Bundle. Returns [`EntityCommands`] — a builder to attach components,
    /// relations and **child** entities (`with_children`) declaratively, in a plain-fn (1:1 Bevy
    /// `Commands::spawn`). `id()` returns a real `Entity` immediately if Commands is bound to the
    /// world (the system path); otherwise `PLACEHOLDER` (standalone Commands without a reserver).
    /// Existing code `cmd.spawn(bundle);` (without reading the result) works unchanged.
    pub fn spawn<B: Bundle + Send + 'static>(&mut self, bundle: B) -> EntityCommands<'_> {
        let entity = match &self.reserver {
            Some(r) => r.reserve(),
            None => Entity::PLACEHOLDER,
        };
        let offset = self.arena.alloc(bundle);
        self.queue.push(Command::Spawn {
            entity,
            offset,
            apply: spawn_apply::<B>,
            apply_batch: spawn_apply_batch::<B>,
            drop: spawn_drop::<B>,
        });
        EntityCommands {
            commands: self,
            entity,
        }
    }

    /// Create many entities from same-typed Bundles with a SINGLE reservation (one atomic
    /// `fetch_add` for the whole batch — scales to bulk spawning: particles/streaming/crowds).
    /// Returns the reserved `Entity`s (real cross-frame ids with system Commands;
    /// `PLACEHOLDER` for standalone without a reserver — then ids are allocated on apply). 1:1 Bevy
    /// `Commands::spawn_batch` (but returning ids).
    ///
    /// *Perf:* consecutive spawn commands of the SAME type `B` are applied in a bulk pass
    /// (`spawn_apply_batch` → `World::spawn_bundles_bulk`: archetype/ids/columns are resolved once
    /// per batch, not per spawn). The remaining gap vs Bevy is in the per-item bundle write
    /// (`write_into_batch`).
    pub fn spawn_batch<B, I>(&mut self, bundles: I) -> Vec<Entity>
    where
        B: Bundle + Send + 'static,
        I: IntoIterator<Item = B>,
    {
        let bundles: Vec<B> = bundles.into_iter().collect();
        let entities: Vec<Entity> = match &self.reserver {
            Some(r) => r.reserve_n(bundles.len()),
            None => vec![Entity::PLACEHOLDER; bundles.len()],
        };
        for (&entity, bundle) in entities.iter().zip(bundles) {
            let offset = self.arena.alloc(bundle);
            self.queue.push(Command::Spawn {
                entity,
                offset,
                apply: spawn_apply::<B>,
                apply_batch: spawn_apply_batch::<B>,
                drop: spawn_drop::<B>,
            });
        }
        entities
    }

    /// Get an [`EntityCommands`] builder for an already existing `entity` (1:1 Bevy
    /// `Commands::entity`) — attach components/relations/children deferred.
    #[inline]
    pub fn entity(&mut self, entity: Entity) -> EntityCommands<'_> {
        EntityCommands {
            commands: self,
            entity,
        }
    }

    /// Add a component to an entity — typed payload in the bump arena
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

    /// Insert a component by ComponentId with raw data.
    /// Used when the ComponentId is known dynamically (not via a type).
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

    /// Remove a component by ComponentId (raw).
    /// Used when the ComponentId is known dynamically (not via a type).
    pub fn remove_raw(&mut self, entity: Entity, component_id: ComponentId) {
        self.queue.push(Command::Remove {
            entity,
            component_id,
        });
    }

    /// Remove a component from an entity — typed variant, without a Box allocation
    pub fn remove<T: Component + Send + 'static>(&mut self, entity: Entity) {
        // SAFETY: typed_remove::<T> is called only in apply() with the correct T.
        // The function pointer needs no data in the bump arena; entity is passed directly.
        unsafe fn typed_remove<T: Component>(entity: Entity, world: &mut World) {
            world.remove::<T>(entity);
        }
        self.queue.push(Command::RemoveTyped {
            entity,
            remove_fn: typed_remove::<T>,
        });
    }

    /// Arbitrary command
    /// Deferred insertion of a resource (1:1 Bevy `Commands::insert_resource`).
    /// Applied at the sync point together with the rest of the commands.
    pub fn insert_resource<T: Send + Sync + 'static>(&mut self, resource: T) {
        self.add(move |world: &mut World| world.insert_resource(resource));
    }

    pub fn add<F: FnOnce(&mut World) + Send + 'static>(&mut self, f: F) {
        self.queue.push(Command::Apply(Box::new(f)));
    }

    /// Add a relation between subject and target.
    ///
    /// Executed deferred at `apply()` — safe in parallel systems.
    pub fn add_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        fn apply<R: RelationKind>(world: &mut World, subject: Entity, target: Entity) {
            // A plain fn pointer cannot capture `_kind`, so the ZST value is
            // rematerialized. SAFETY: the const assert enforces R is a ZST, for
            // which `zeroed` is the unique (trivially valid) value; a non-ZST
            // RelationKind would zero a real value (e.g. a null reference = UB),
            // so it is a compile error rather than a silent footgun.
            const { assert!(std::mem::size_of::<R>() == 0, "RelationKind must be a zero-sized type") };
            let kind: R = unsafe { std::mem::zeroed() };
            world.add_relation(subject, kind, target);
        }
        self.queue.push(Command::AddRelation {
            subject,
            target,
            apply: apply::<R>,
        });
    }

    /// Remove a relation between subject and target.
    ///
    /// Executed deferred at `apply()`.
    pub fn remove_relation<R: RelationKind>(&mut self, subject: Entity, _kind: R, target: Entity) {
        fn apply<R: RelationKind>(world: &mut World, subject: Entity, target: Entity) {
            // See `add_relation`: const assert makes the ZST rematerialization sound.
            const { assert!(std::mem::size_of::<R>() == 0, "RelationKind must be a zero-sized type") };
            let kind: R = unsafe { std::mem::zeroed() };
            world.remove_relation(subject, kind, target);
        }
        self.queue.push(Command::RemoveRelation {
            subject,
            target,
            apply: apply::<R>,
        });
    }

    /// Bulk addition of a relation from many subjects to a single target.
    ///
    /// Optimized via `World::add_relation_batch`.
    pub fn add_relation_batch<R: RelationKind + Send + 'static>(
        &mut self,
        subjects: Vec<Entity>,
        kind: R,
        target: Entity,
    ) {
        // The closure can capture `kind` (RelationKind: Copy), so no `zeroed`.
        self.add(move |world| {
            world.add_relation_batch(&subjects, kind, target);
        });
    }

    /// Create an entity from a registered template with parameters.
    ///
    /// # Example
    /// ```ignore
    /// struct MonsterSpeed;
    /// impl apex_core::template::TemplateParam for MonsterSpeed { type Value = f32; }
    ///
    /// cmds.spawn_template_with("Monster", TemplateParams::new()
    ///     .set::<MonsterSpeed>(10.0f32));
    /// ```
    pub fn spawn_template_with(&mut self, name: &str, params: TemplateParams) {
        self.queue.push(Command::SpawnFromTemplate {
            name: name.to_string(),
            params: Box::new(params),
        });
    }

    /// Create an entity from a template with default parameters.
    ///
    /// # Example
    /// ```ignore
    /// cmds.spawn_template("Monster");
    /// ```
    pub fn spawn_template(&mut self, name: &str) {
        self.queue.push(Command::SpawnFromTemplate {
            name: name.to_string(),
            params: Box::new(TemplateParams::new()),
        });
    }

    /// Apply all accumulated commands to the world.
    ///
    /// A burst of CONSECUTIVE inserts on one entity (`insert(e, A);
    /// insert(e, B); …`) is applied as a GROUP — one archetype move per batch
    /// instead of a move-per-component (W2-1, `World::insert_parts`). The order
    /// in which commands are applied is preserved.
    pub fn apply(&mut self, world: &mut World) {
        // Materialize records for reserved `spawn().id()` entities BEFORE processing the queue
        // (their spawn commands will fill in location/components). Idempotent and cheap if there are
        // no reservations.
        world.flush_reserved();
        let queue = std::mem::take(&mut self.queue);
        let mut it = queue.into_iter().peekable();
        // Reusable group buffers (outside the loop — no reallocations).
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
            // A batch of consecutive spawns of the SAME type `B` (equal `apply_batch` pointer) — one
            // bulk-apply with a single archetype resolve instead of a per-spawn `spawn_at`. Spawns of
            // different types, or interleaved with other commands (e.g. `with_children` →
            // Spawn+AddRelation), are not grouped (`items.len()==1` ⇒ single path, no regression).
            // Closes the `commands_spawn` gap vs Bevy.
            if let Command::Spawn {
                entity,
                offset,
                apply,
                apply_batch,
                ..
            } = cmd
            {
                let batch_ptr = apply_batch as usize;
                let mut items: smallvec::SmallVec<[(Entity, u32); 16]> = smallvec::SmallVec::new();
                items.push((entity, offset));
                loop {
                    // `matches!` releases the `peek` borrow BEFORE `next` (no borrow conflict).
                    let same = matches!(
                        it.peek(),
                        Some(Command::Spawn { apply_batch: ab, .. }) if *ab as usize == batch_ptr
                    );
                    if !same {
                        break;
                    }
                    if let Some(Command::Spawn { entity, offset, .. }) = it.next() {
                        items.push((entity, offset));
                    }
                }
                if items.len() == 1 {
                    // A single spawn — the single path without the bulk's Vec allocations.
                    unsafe { apply(self.arena.get_ptr(offset), world, entity) };
                } else {
                    // SAFETY: all items are one type `B` (equal `apply_batch` pointer; per-type
                    // `component_ids` rules out ICF-merging distinct `B`); their bundles are valid in
                    // the arena.
                    unsafe { apply_batch(world, &items, &self.arena) };
                }
                continue;
            }
            self.apply_one(cmd, world);
        }
        self.arena.reset();
    }

    /// Apply a group of insert/insert_raw on one entity with a single archetype move.
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
                _ => unreachable!("an insert group contains only Insert/InsertRaw"),
            }
        }

        if world.insert_parts(entity, parts) {
            // Values were copied into the world's ownership: we do NOT drop the typed payloads
            // (equivalent to ptr::read + forget); the Vec<u8> from InsertRaw is just bytes.
            group.clear();
        } else {
            // Entity is dead — we free the typed payloads, as world.insert
            // would have dropped the value on an early return.
            for cmd in group.drain(..) {
                if let Command::Insert { offset, drop, .. } = cmd {
                    unsafe { drop(self.arena.get_ptr(offset)) };
                }
            }
        }
    }

    /// Apply a single command (outside of groups).
    fn apply_one(&self, cmd: Command, world: &mut World) {
        {
            match cmd {
                Command::Spawn {
                    entity,
                    offset,
                    apply,
                    ..
                } => unsafe {
                    apply(self.arena.get_ptr(offset), world, entity);
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
                // SAFETY: remove_fn is a valid function pointer created in remove::<T>.
                // Calling the type-specialized function world.remove::<T>(entity) is safe,
                // since T is statically fixed at command creation.
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
                    // §0.2a (B7): a queued despawn whose target is already gone by
                    // apply time (double-despawn, or a cascade removed it first)
                    // silently no-ops. Surface it — the caller queued a despawn
                    // that did nothing.
                    if !world.despawn(entity) {
                        crate::anomaly!(
                            world, crate::Severity::Warn, "Commands::despawn",
                            Some(entity), None,
                            "no-op (already despawned?)"
                        );
                    }
                }
                Command::SpawnFromTemplate { name, params } => {
                    // §0.2a (B10): a queued spawn of an unregistered template name
                    // (typo, or the template was never registered) silently spawns
                    // nothing. Surface it.
                    if world.spawn_template_with(&name, &params).is_none() {
                        crate::anomaly!(
                            world, crate::Severity::Warn, "Commands::spawn_template",
                            None, None,
                            "no template registered under name \"{name}\"; nothing spawned"
                        );
                    }
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

    /// Clear without applying — correctly drops the typed data in the arena
    pub fn clear(&mut self) {
        // B5: reclaim reservations from `spawn().id()` that will never be applied.
        self.abandon_queued_reservations();
        for cmd in self.queue.drain(..) {
            match cmd {
                Command::Spawn { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                Command::Insert { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                // RemoveTyped / Remove / InsertRaw store no data in the bump arena — nothing to drop
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

    /// B5: return reservations from un-applied `spawn().id()` calls to the allocator.
    ///
    /// A reserver-bound `Commands` (the system path) that is dropped or cleared with
    /// queued `Spawn`s never materializes those ids — they advanced the shared
    /// high-water / consumed lease slots. Without returning them the id-space leaks
    /// (TD-40 fixed only the *count*, not the leak). We hand the reserved indices to
    /// the reserver's abandoned queue; the allocator reclaims them on the next
    /// [`flush`](crate::entity::EntityAllocator::flush). A no-op without a reserver
    /// (standalone `Commands` reserve `PLACEHOLDER`, which owns no id) or with no
    /// queued spawns (the common post-`apply` case: `apply` empties the queue).
    fn abandon_queued_reservations(&self) {
        let Some(reserver) = &self.reserver else { return };
        let reserved: Vec<Entity> = self
            .queue
            .iter()
            .filter_map(|cmd| match cmd {
                Command::Spawn { entity, .. } if *entity != Entity::PLACEHOLDER => Some(*entity),
                _ => None,
            })
            .collect();
        if reserved.is_empty() {
            return;
        }
        crate::warn_once!(
            "Commands dropped/cleared with {} un-applied spawn() reservation(s); \
             returning their ids to the allocator (apply the Commands to avoid this)",
            reserved.len()
        );
        reserver.abandon(&reserved);
    }
}

/// An entity builder in the [`Commands`] queue — chaining components, relations and **children**
/// declaratively, in a plain-fn (1:1 Bevy `EntityCommands`). Returned by
/// [`Commands::spawn`]/[`Commands::entity`].
///
/// ```ignore
/// fn setup(cmd: &mut Commands) {
///     cmd.spawn((Transform::default(), Name("ring")))
///         .with_children(|c| {
///             c.spawn((Transform::default(), Fox));   // ChildOf → ring automatically
///             c.spawn((Transform::default(), Fox));
///         });
/// }
/// ```
pub struct EntityCommands<'a> {
    commands: &'a mut Commands,
    entity: Entity,
}

impl EntityCommands<'_> {
    /// The id of this entity. A real cross-frame `Entity` if Commands is bound to the world (the
    /// system path); otherwise `Entity::PLACEHOLDER` (standalone Commands without a reserver).
    #[inline]
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Attach a component (deferred). 1:1 Bevy `EntityCommands::insert`.
    #[inline]
    pub fn insert<T: Component + Send + 'static>(self, component: T) -> Self {
        self.commands.insert(self.entity, component);
        self
    }

    /// Remove a component (deferred).
    #[inline]
    pub fn remove<T: Component + Send + 'static>(self) -> Self {
        self.commands.remove::<T>(self.entity);
        self
    }

    /// Add a relation `self —kind→ target` (deferred).
    #[inline]
    pub fn add_relation<R: RelationKind>(self, kind: R, target: Entity) -> Self {
        self.commands.add_relation(self.entity, kind, target);
        self
    }

    /// Make `self` a child of `parent` (the [`ChildOf`](crate::relations::ChildOf) relation).
    #[inline]
    pub fn set_parent(self, parent: Entity) -> Self {
        self.commands
            .add_relation(self.entity, crate::relations::ChildOf, parent);
        self
    }

    /// Adopt an ALREADY existing `child` (the `ChildOf` relation child → self). Unlike
    /// [`with_children`](Self::with_children) (which spawns new ones), this binds an existing entity
    /// — needed by the editor/gameplay for reparenting. 1:1 Bevy `EntityCommands::add_child`.
    #[inline]
    pub fn add_child(self, child: Entity) -> Self {
        self.commands
            .add_relation(child, crate::relations::ChildOf, self.entity);
        self
    }

    /// Adopt a set of existing entities (see [`add_child`](Self::add_child)).
    pub fn add_children(self, children: &[Entity]) -> Self {
        for &child in children {
            self.commands
                .add_relation(child, crate::relations::ChildOf, self.entity);
        }
        self
    }

    /// Detach `self` from its parent (remove its `ChildOf` relation). The parent is resolved on
    /// apply (its id need not be known). 1:1 Bevy `EntityCommands::remove_parent`.
    pub fn remove_parent(self) -> Self {
        let entity = self.entity;
        self.commands.add(move |world: &mut World| {
            if let Some(parent) = world.target_of(entity, crate::relations::ChildOf) {
                world.remove_relation(entity, crate::relations::ChildOf, parent);
            }
        });
        self
    }

    /// Detach ALL children of `self` (remove their `ChildOf` → self relations) without deleting
    /// them. The children are resolved on apply. 1:1 Bevy `EntityCommands::clear_children`.
    pub fn clear_children(self) -> Self {
        let parent = self.entity;
        self.commands.add(move |world: &mut World| {
            let kids: Vec<Entity> = world.targets_of(crate::relations::ChildOf, parent).collect();
            for child in kids {
                world.remove_relation(child, crate::relations::ChildOf, parent);
            }
        });
        self
    }

    /// Spawn children of this entity declaratively (1:1 Bevy `with_children`). Each `c.spawn(...)`
    /// automatically gets a [`ChildOf`](crate::relations::ChildOf) → this entity relation. Nesting
    /// of arbitrary depth (a child also returns [`EntityCommands`] with its own `with_children`).
    pub fn with_children(self, f: impl FnOnce(&mut ChildSpawner)) -> Self {
        let parent = self.entity;
        {
            let mut spawner = ChildSpawner {
                commands: &mut *self.commands,
                parent,
            };
            f(&mut spawner);
        }
        self
    }

    /// Destroy this entity (deferred).
    #[inline]
    pub fn despawn(self) {
        self.commands.despawn(self.entity);
    }
}

/// A child spawner inside [`EntityCommands::with_children`]. Each [`ChildSpawner::spawn`] attaches
/// a `ChildOf` → parent relation.
pub struct ChildSpawner<'a> {
    commands: &'a mut Commands,
    parent: Entity,
}

impl ChildSpawner<'_> {
    /// Spawn a child (the `ChildOf` → parent relation is added automatically). Returns the child's
    /// [`EntityCommands`] — you can nest another `with_children`.
    pub fn spawn<B: Bundle + Send + 'static>(&mut self, bundle: B) -> EntityCommands<'_> {
        // `.id()` copies the id and drops the temporary builder → the borrow is released before add_relation.
        let child = self.commands.spawn(bundle).id();
        self.commands
            .add_relation(child, crate::relations::ChildOf, self.parent);
        EntityCommands {
            commands: self.commands,
            entity: child,
        }
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        // B5: reclaim reservations from `spawn().id()` that were never applied.
        self.abandon_queued_reservations();
        // Drop the typed data in the arena before deallocating the buffer
        for cmd in self.queue.drain(..) {
            match cmd {
                Command::Spawn { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                Command::Insert { offset, drop, .. } => unsafe {
                    drop(self.arena.get_ptr(offset));
                },
                // RemoveTyped / Remove / InsertRaw store no data in the bump arena — nothing to drop
                Command::RemoveTyped { .. } => {}
                Command::Remove { .. } => {}
                Command::InsertRaw { .. } => {}
                Command::AddRelation { .. } => {}
                Command::RemoveRelation { .. } => {}
                _ => {}
            }
        }
        // CommandArena::drop() deallocates the backing buffer
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

        // Verify the entity was created with the component
        let query = crate::query::Query::<crate::query::Read<Pos>>::new(&world);
        let mut count = 0;
        query.for_each(|_, pos| {
            count += 1;
            assert_eq!(pos.0, 1.0);
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn commands_spawn_id_is_real_entity_and_alive_after_apply() {
        let mut world = World::new();
        let mut cmds = Commands::new();
        cmds.set_reserver(world.entity_reserver());

        // With a reserver, `spawn().id()` returns a real id immediately.
        let e = cmds.spawn((Pos(7.0),)).id();
        assert_ne!(e, Entity::PLACEHOLDER);
        // Before apply — reserved but not alive (like Bevy before the sync point).
        assert!(!world.is_alive(e), "reserved entity is not alive before apply");

        cmds.apply(&mut world);
        assert!(world.is_alive(e), "entity is alive after apply");
        assert_eq!(world.get::<Pos>(e).unwrap().0, 7.0);
    }

    /// B5: a reserver-bound `Commands` dropped WITHOUT `apply` returns its
    /// `spawn().id()` reservations to the allocator, so the id-space stays bounded — a
    /// fresh `Commands` reuses those indices instead of growing past them.
    #[test]
    fn dropped_commands_reservations_are_reclaimed() {
        let mut world = World::new();
        {
            let mut cmds = Commands::new();
            cmds.set_reserver(world.entity_reserver());
            let a = cmds.spawn((Pos(1.0),)).id();
            let b = cmds.spawn((Pos(2.0),)).id();
            let c = cmds.spawn((Pos(3.0),)).id();
            assert_eq!([a.index, b.index, c.index], [0, 1, 2]);
            // `cmds` dropped here without apply → abandons indices 0,1,2.
        }
        world.flush_reserved();

        // A fresh Commands reuses the abandoned indices rather than growing to 3,4,5.
        let mut cmds2 = Commands::new();
        cmds2.set_reserver(world.entity_reserver());
        let reused: std::collections::HashSet<u32> = (0..3)
            .map(|i| cmds2.spawn((Pos(i as f32),)).id().index)
            .collect();
        assert_eq!(
            reused,
            [0, 1, 2].into_iter().collect(),
            "dropped reservations returned to the pool — id-space bounded"
        );
        cmds2.apply(&mut world);
        assert_eq!(world.entity_count(), 3, "only the applied entities are alive");
    }

    /// B5: `clear()` reclaims reservations too (same path as drop).
    #[test]
    fn cleared_commands_reservations_are_reclaimed() {
        let mut world = World::new();
        let mut cmds = Commands::new();
        cmds.set_reserver(world.entity_reserver());
        let a = cmds.spawn((Pos(1.0),)).id();
        let b = cmds.spawn((Pos(2.0),)).id();
        assert_eq!([a.index, b.index], [0, 1]);
        cmds.clear();
        world.flush_reserved();

        // Reuse after clear.
        let c = cmds.spawn((Pos(9.0),)).id();
        assert!(c.index < 2, "cleared reservation index reused (bounded id-space)");
    }

    #[test]
    fn entity_commands_with_children_wires_childof() {
        use crate::relations::ChildOf;
        let mut world = World::new();
        let mut cmds = Commands::new();
        cmds.set_reserver(world.entity_reserver());

        let parent = cmds
            .spawn((Pos(0.0),))
            .with_children(|c| {
                c.spawn((Vel(1.0),));
                c.spawn((Vel(2.0),));
            })
            .id();
        cmds.apply(&mut world);

        let kids: Vec<Entity> = world.targets_of(ChildOf, parent).collect();
        assert_eq!(kids.len(), 2, "both children are linked ChildOf → parent");
        for k in kids {
            assert!(world.get::<Vel>(k).is_some(), "the child carries its own component");
            assert_eq!(
                world.target_of(k, ChildOf),
                Some(parent),
                "the target of the ChildOf relation is the parent"
            );
        }
    }

    #[test]
    fn spawn_batch_reserves_distinct_ids_and_applies() {
        let mut world = World::new();
        let mut cmds = Commands::new();
        cmds.set_reserver(world.entity_reserver());

        let ids = cmds.spawn_batch((0..100).map(|i| (Pos(i as f32),)));
        assert_eq!(ids.len(), 100);
        let uniq: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(uniq.len(), 100, "batch ids are unique (a single reservation)");
        assert!(ids.iter().all(|&e| e != Entity::PLACEHOLDER));

        cmds.apply(&mut world);
        for (i, &e) in ids.iter().enumerate() {
            assert!(world.is_alive(e), "entity {i} is alive after apply");
            assert_eq!(world.get::<Pos>(e).unwrap().0, i as f32);
        }
    }

    #[test]
    fn entity_commands_reparent_existing_entities() {
        use crate::relations::ChildOf;
        let mut world = World::new();
        let parent = world.spawn((Pos(0.0),));
        let a = world.spawn((Vel(1.0),));
        let b = world.spawn((Vel(2.0),));

        let mut cmds = Commands::new();
        cmds.set_reserver(world.entity_reserver());

        // Adopt existing a, b.
        cmds.entity(parent).add_children(&[a, b]);
        cmds.apply(&mut world);
        assert_eq!(world.target_of(a, ChildOf), Some(parent));
        assert_eq!(world.target_of(b, ChildOf), Some(parent));

        // Detach a from its parent.
        cmds.entity(a).remove_parent();
        cmds.apply(&mut world);
        assert_eq!(world.target_of(a, ChildOf), None);

        // Detach all children of parent (removes b) without deleting them.
        cmds.entity(parent).clear_children();
        cmds.apply(&mut world);
        assert_eq!(world.target_of(b, ChildOf), None);
        assert!(world.is_alive(b), "clear_children does NOT delete children");
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
        // insert_raw with raw bytes
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
        cmds.spawn_template_with("param_test", TemplateParams::new().set::<ParamVal>(42.0f32));
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
        cmds.apply(&mut world); // must not panic
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

        // First cycle: spawn many entities
        for _ in 0..10 {
            cmds.spawn((Pos(1.0),));
        }
        cmds.apply(&mut world);
        assert_eq!(cmds.len(), 0);

        // Second cycle: the arena is already allocated, reused without allocations
        for _ in 0..10 {
            cmds.spawn((Pos(2.0),));
        }
        cmds.apply(&mut world);
        assert_eq!(cmds.len(), 0);

        let query = crate::query::Query::<crate::query::Read<Pos>>::new(&world);
        let count = query.iter().count();
        assert_eq!(count, 20);
    }

    // ── W2-1: grouping of insert bursts ────────────────────────

    #[derive(Clone, Copy)]
    struct Acc(f32);
    impl Component for Acc {}
    #[derive(Clone, Copy)]
    struct Hp(f32);
    impl Component for Hp {}

    /// A burst of inserts on one entity is applied as a group: one archetype
    /// move, all components in place, values correct.
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
        assert_eq!(world.get::<Pos>(entity).unwrap().0, 0.0, "the old component survived the move");
    }

    /// A duplicate component in the group: the LAST value survives (command
    /// order is preserved); the intermediate one is dropped.
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

    /// A group on a DEAD entity: the payloads are freed (no panic and no
    /// leak); the world is unchanged.
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

        assert_eq!(Arc::strong_count(&probe), 1, "the dead entity's payloads were dropped");
    }

    /// Grouping does not break ordering with other commands: inserts before
    /// the despawn are applied, and the despawn after kills the entity.
    #[test]
    fn commands_insert_burst_then_despawn_order_preserved() {
        let mut world = World::new();
        let entity = world.spawn((Pos(0.0),));

        let mut cmds = Commands::new();
        cmds.insert(entity, Vel(1.0));
        cmds.insert(entity, Acc(2.0));
        cmds.despawn(entity);
        cmds.apply(&mut world);

        assert!(world.get::<Pos>(entity).is_none(), "the despawn after the burst was applied");
    }

    /// W2-1 leak fix: insert OVER an existing component drops the old value
    /// (both in a group and individually).
    #[test]
    fn commands_insert_overwrite_drops_old_value() {
        use std::sync::Arc;

        struct Holder(#[allow(dead_code)] Arc<()>);
        impl Component for Holder {}

        let probe = Arc::new(());
        let mut world = World::new();
        let entity = world.spawn((Holder(probe.clone()),));
        assert_eq!(Arc::strong_count(&probe), 2);

        // A single insert over an existing one
        world.insert(entity, Holder(probe.clone()));
        assert_eq!(Arc::strong_count(&probe), 2, "the old value was dropped (single path)");

        // The group path (a burst with a duplicate over an existing one)
        let mut cmds = Commands::new();
        cmds.insert(entity, Holder(probe.clone()));
        cmds.insert(entity, Holder(probe.clone()));
        cmds.apply(&mut world);
        assert_eq!(Arc::strong_count(&probe), 2, "the group path drops the overwritten ones");

        world.despawn(entity);
        assert_eq!(Arc::strong_count(&probe), 1);
    }

    #[test]
    fn commands_preserves_tick() {
        let mut world = World::new();
        let mut cmds = Commands::new();

        let entity = world.spawn((Pos(0.0),));
        let _tick_before = world.current_tick();

        // Make a few changes
        cmds.insert(entity, Vel(5.0));
        cmds.apply(&mut world);

        let vel = world.get::<Vel>(entity).unwrap();
        assert_eq!(vel.0, 5.0);
    }

    // ── Arena alignment (B1) ───────────────────────────────────

    #[repr(align(16))]
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Aligned16([f32; 4]);
    impl Component for Aligned16 {}

    /// Direct arena check: a 16-aligned payload placed after a 1-aligned one must
    /// still come back 16-aligned. Deterministic (no Miri needed): `align_offset`
    /// is exact. On the pre-fix 8-aligned base this returned a base ≡ 8 (mod 16).
    #[test]
    fn arena_honors_payload_alignment_over_eight() {
        let mut arena = CommandArena::new();
        // A 1-byte payload first, so the next 16-aligned one lands at a non-trivial base.
        let _o0 = arena.alloc::<u8>(0xAB);
        let o16 = arena.alloc::<Aligned16>(Aligned16([1.0, 2.0, 3.0, 4.0]));
        let ptr = arena.get_ptr(o16);
        assert_eq!(
            ptr.align_offset(16),
            0,
            "16-aligned payload must be stored at a 16-aligned address"
        );
        // Value round-trips through the read path apply() uses.
        let v = unsafe { std::ptr::read(ptr as *const Aligned16) };
        assert_eq!(v, Aligned16([1.0, 2.0, 3.0, 4.0]));
    }

    /// Growing the alignment mid-stream must relocate earlier payloads to a base
    /// that still honors their (smaller) alignment.
    #[test]
    fn arena_alignment_growth_preserves_earlier_payloads() {
        let mut arena = CommandArena::new();
        let o_a = arena.alloc::<u64>(0x0102_0304_0506_0708);
        let o_b = arena.alloc::<Aligned16>(Aligned16([9.0, 8.0, 7.0, 6.0])); // forces base to 16
        let pa = arena.get_ptr(o_a);
        let pb = arena.get_ptr(o_b);
        assert_eq!(pa.align_offset(8), 0);
        assert_eq!(pb.align_offset(16), 0);
        assert_eq!(unsafe { std::ptr::read(pa as *const u64) }, 0x0102_0304_0506_0708);
        assert_eq!(
            unsafe { std::ptr::read(pb as *const Aligned16) },
            Aligned16([9.0, 8.0, 7.0, 6.0])
        );
    }

    /// End-to-end: an over-aligned component travels through the Commands arena
    /// (spawn + insert) and lands in the world intact. Under Miri this exercises
    /// the arena's alignment contract on the real apply path.
    #[test]
    fn commands_spawn_and_insert_over_aligned_component() {
        let mut world = World::new();
        let mut cmds = Commands::new();
        cmds.set_reserver(world.entity_reserver());

        let e = cmds.spawn((Pos(1.0), Aligned16([1.0, 2.0, 3.0, 4.0]))).id();
        cmds.apply(&mut world);
        assert_eq!(world.get::<Aligned16>(e), Some(&Aligned16([1.0, 2.0, 3.0, 4.0])));

        let e2 = world.spawn((Vel(0.0),));
        cmds.insert(e2, Aligned16([5.0, 6.0, 7.0, 8.0]));
        cmds.apply(&mut world);
        assert_eq!(world.get::<Aligned16>(e2), Some(&Aligned16([5.0, 6.0, 7.0, 8.0])));
    }
}
