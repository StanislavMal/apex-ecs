use std::any::TypeId;

// ComponentMask (512-bit O(1) conflict mask) removed in the wave-4 dead-code
// pass: `assign_masks` was never called, so the masks stayed EMPTY and
// `conflicts_with` always took the linear-scan fallback — the whole mask
// fast-path (struct, fields, assign_masks, conflicts_with_fast) was dead (C8).
//
// ArchetypeMask (fixed 1024 bits) removed in CR-M4: its only live consumer was
// exclude_mask in Query::new (redundant after CR-M2 — Without is correctly
// filtered in matches_archetype), and beyond 1024 archetypes set/get silently
// degraded into a no-op (C-9).

/// Declaration of a system's Read/Write access to world data.
///
/// Stores `TypeId` vectors of read/written components and events. Conflicts
/// between systems are determined by a linear comparison of these vectors (the
/// number of components per system is small).
///
/// Conflict rules — analogous to the Rust borrow checker:
/// - Write + Read  → conflict
/// - Write + Write → conflict
/// - Read  + Read  → no conflict (parallel)
///
/// Also supports declaring access to events:
/// - `read_event<T>()` / `write_event<T>()` — declare reading/writing of events
/// - Two writers of the same event type conflict (WriteWrite)
/// - A writer and a reader of the same event type conflict (WriteRead)
/// - Two readers of the same event type — do NOT conflict
#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    pub reads: Vec<TypeId>,
    pub writes: Vec<TypeId>,
    /// Event types the system reads (TypeId, type_name).
    pub reads_event: Vec<(TypeId, &'static str)>,
    /// Event types the system writes (TypeId, type_name).
    pub writes_event: Vec<(TypeId, &'static str)>,
    /// Flag: the system uses `par_for_each` internally.
    /// The scheduler will not chunk such a system via ASD
    /// (avoids oversubscribing the rayon thread pool).
    pub uses_par_for_each: bool,
    /// Flag: the system needs ALL entities (global access).
    /// ASD chunking is forbidden — the system always receives the full SubWorld.
    pub needs_whole_world: bool,
    /// Flag: the system has STATE (`size_of::<S>() > 0` — `system!` state
    /// systems, capturing closures). ASD row-split is forbidden (W3-4): several
    /// split tasks would call `run(&mut self)` of ONE system concurrently — a
    /// race on the state. Set by the scheduler at registration; does not
    /// restrict parallelism BETWEEN systems.
    pub stateful: bool,
    /// Flag: the system MUTATES a resource (`ResMut`/`ResWrite`). ASD
    /// decomposition is FORBIDDEN (TD-37): a plain-fn system body runs once per
    /// chunk, so a resource mutation would be multiplied by the number of chunks
    /// (a silent bug only at scale) + a race on parallel chunks. Set by
    /// `ResMut::access()`/`ResWrite::access()`; does not restrict parallelism
    /// BETWEEN systems (that is decided by the conflict on the `writes` TypeId,
    /// which the resource also joins).
    pub writes_resource: bool,
    /// Flag: the system uses `Commands` (deferred structural operations). ASD
    /// decomposition is FORBIDDEN (TD-37): non-entity-local commands would be
    /// duplicated across chunks. Set by `Commands`/`CommandsParam::access()`.
    pub uses_commands: bool,
    /// Types and reserved capacities for event buffers.
    /// The scheduler calls `world.event_reserve::<T>(cap)` before running the
    /// system, to avoid reallocations on the hot send() path.
    pub event_reserves: Vec<(TypeId, usize)>,
}

impl AccessDescriptor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.reads.contains(&tid) {
            self.reads.push(tid);
        }
        self
    }

    pub fn write<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.writes.contains(&tid) {
            self.writes.push(tid);
        }
        self
    }

    /// Declare reading of events of type T.
    pub fn read_event<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        let name = std::any::type_name::<T>();
        if !self.reads_event.iter().any(|(id, _)| *id == tid) {
            self.reads_event.push((tid, name));
        }
        self
    }

    /// Mark that the system uses `par_for_each` internally.
    /// The scheduler will not additionally chunk the system via ASD.
    pub fn par_for_each_used(mut self) -> Self {
        self.uses_par_for_each = true;
        self
    }

    /// Mark that the system needs access to ALL entities (global access).
    /// The scheduler will not chunk the system via ASD.
    pub fn whole_world(mut self) -> Self {
        self.needs_whole_world = true;
        self
    }

    /// Mark that the system MUTATES a resource (`ResMut`/`ResWrite`). Forbids ASD
    /// decomposition (the system body would run once per chunk ⇒ resource
    /// mutation × number of chunks). See TD-37.
    pub fn resource_write(mut self) -> Self {
        self.writes_resource = true;
        self
    }

    /// Mark that the system uses `Commands`. Forbids ASD decomposition
    /// (non-entity-local commands would be duplicated across chunks). See TD-37.
    pub fn commands_used(mut self) -> Self {
        self.uses_commands = true;
        self
    }

    /// Declare writing of events of type T.
    pub fn write_event<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        let name = std::any::type_name::<T>();
        if !self.writes_event.iter().any(|(id, _)| *id == tid) {
            self.writes_event.push((tid, name));
        }
        self
    }

    /// Reserve capacity for the event buffer of type T.
    ///
    /// Avoids `Vec` reallocations during bulk event sending in a loop.
    /// The scheduler calls `world.event_reserve::<T>(capacity)` before running
    /// the system based on this declaration.
    pub fn event_reserve<T: 'static>(mut self, capacity: usize) -> Self {
        let tid = TypeId::of::<T>();
        self.event_reserves.push((tid, capacity));
        self
    }

    pub fn merge(mut self, other: &AccessDescriptor) -> Self {
        // O(N+M) deduplication via HashSet instead of O(N²) contains+push
        Self::dedup_push(&mut self.reads, &other.reads);
        Self::dedup_push(&mut self.writes, &other.writes);
        Self::dedup_push_event(&mut self.reads_event, &other.reads_event);
        Self::dedup_push_event(&mut self.writes_event, &other.writes_event);
        // Propagate the par_for_each flag
        self.uses_par_for_each = self.uses_par_for_each || other.uses_par_for_each;
        self.needs_whole_world = self.needs_whole_world || other.needs_whole_world;
        self.stateful = self.stateful || other.stateful;
        self.writes_resource = self.writes_resource || other.writes_resource;
        self.uses_commands = self.uses_commands || other.uses_commands;
        // Merge event reservations (take the maximum per type)
        for &(tid, cap) in &other.event_reserves {
            if let Some(entry) = self.event_reserves.iter_mut().find(|(id, _)| *id == tid) {
                entry.1 = entry.1.max(cap);
            } else {
                self.event_reserves.push((tid, cap));
            }
        }
        self
    }

    /// O(N) data-conflict check via a linear comparison of the write/read
    /// vectors.
    ///
    /// Write(self) ∩ (Read(other) ∪ Write(other)) ≠ ∅, or
    /// Write(other) ∩ Read(self) ≠ ∅. The number of components per system is
    /// small, so a linear scan is enough.
    pub fn conflicts_with(&self, other: &AccessDescriptor) -> bool {
        if !self.writes.is_empty() || !other.writes.is_empty() {
            for w in &self.writes {
                if other.reads.contains(w) || other.writes.contains(w) {
                    return true;
                }
            }
            for w in &other.writes {
                if self.reads.contains(w) || self.writes.contains(w) {
                    return true;
                }
            }
        }
        // No writes — no conflict
        false
    }

    /// O(N+M) deduplication — linear scan for small sets, HashSet for large ones.
    fn dedup_push(vec: &mut Vec<TypeId>, items: &[TypeId]) {
        if items.is_empty() {
            return;
        }
        // Threshold: if the total size < 8 — O(N²) is cheaper than allocating a HashSet
        if vec.len() + items.len() < 8 {
            for &item in items {
                if !vec.contains(&item) {
                    vec.push(item);
                }
            }
            return;
        }
        // For large sets — HashSet as before
        let mut set: std::collections::HashSet<TypeId> = vec.iter().cloned().collect();
        for &item in items {
            if set.insert(item) {
                vec.push(item);
            }
        }
    }

    /// O(N+M) deduplication for event tuples (TypeId, type_name).
    fn dedup_push_event(vec: &mut Vec<(TypeId, &'static str)>, items: &[(TypeId, &'static str)]) {
        if items.is_empty() {
            return;
        }
        if vec.len() + items.len() < 8 {
            for &item in items {
                if !vec.iter().any(|(id, _)| *id == item.0) {
                    vec.push(item);
                }
            }
            return;
        }
        let mut set: std::collections::HashSet<TypeId> = vec.iter().map(|(id, _)| *id).collect();
        for &item in items {
            if set.insert(item.0) {
                vec.push(item);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
            && self.writes.is_empty()
            && self.reads_event.is_empty()
            && self.writes_event.is_empty()
    }
}

/// Create an `AccessDescriptor` with declarative syntax.
///
/// Supports `read<T>`, `write<T>`, `read_event<T>`, `write_event<T>`.
///
/// # Example
///
/// ```ignore
/// access_desc!(read<Vel>, write<Pos>, read_event<DamageEvent>)
/// ```
#[macro_export]
macro_rules! access_desc {
    () => {
        $crate::AccessDescriptor::new()
    };
    ($($action:ident < $ty:ty >),+ $(,)?) => {{
        let mut desc = $crate::AccessDescriptor::new();
        $(
            desc = desc.$action::<$ty>();
        )+
        desc
    }};
}
