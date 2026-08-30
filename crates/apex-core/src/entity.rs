/// Entity — generational index.
///
/// `Serialize`/`Deserialize` so components holding `Entity` refs can be
/// snapshotted; on restore those refs are remapped via [`MapEntities`](crate::MapEntities) (E6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entity {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl Entity {
    /// Sentinel value — guaranteed to never match a real entity.
    pub const PLACEHOLDER: Entity = Entity {
        index: u32::MAX,
        generation: u32::MAX,
    };

    /// Construct an `Entity` from raw parts. Only for bridge/ECS infrastructure.
    #[inline]
    pub const fn from_raw_parts(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    #[inline]
    pub fn index(self) -> u32 {
        self.index
    }
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

impl std::fmt::Display for Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Entity({}v{})", self.index, self.generation)
    }
}

const NO_LOCATION: u64 = u64::MAX;

#[derive(Clone, Copy, Debug)]
pub struct EntityLocation {
    pub archetype_id: crate::archetype::ArchetypeId,
    pub row: u32,
}

struct EntityRecord {
    generation: u32,
    encoded_location: u64,
}

impl EntityRecord {
    #[inline]
    fn location(&self) -> Option<EntityLocation> {
        if self.encoded_location == NO_LOCATION {
            None
        } else {
            let row = (self.encoded_location & 0xFFFF_FFFF) as u32;
            let archetype_id = (self.encoded_location >> 32) as u32;
            Some(EntityLocation {
                archetype_id: crate::archetype::ArchetypeId(archetype_id),
                row,
            })
        }
    }

    #[inline]
    fn set_location(&mut self, loc: EntityLocation) {
        self.encoded_location = (loc.row as u64) | ((loc.archetype_id.0 as u64) << 32);
    }

    #[inline]
    fn clear_location(&mut self) {
        self.encoded_location = NO_LOCATION;
    }

    #[inline]
    fn has_location(&self) -> bool {
        self.encoded_location != NO_LOCATION
    }
}

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Lease of free slots for reservation: a snapshot of `free_list` (index + generation), moved out of
/// `free_list` for the span between flushes, plus a decrementing cursor. **Lease slots are disjoint from
/// the current `free_list`** (the lease took them out of it) — therefore `reserve` (via the lease, `&self`)
/// and direct `allocate` (via `free_list`, `&mut`) never hand out the same index. `cursor` decrements:
/// `n = fetch_sub(1)`; `n>0` → reuse `free[n-1]`; `n≤0` → fresh index from `high_water`.
struct ReserveLease {
    cursor: AtomicI64,
    free: Box<[(u32, u32)]>,
}

/// Shared cell of the current lease. `EntityAllocator` and ALL `EntityReserver`s
/// hold a clone of ONE `Arc<LeaseCell>`, so `flush`/`refresh_lease`
/// publish a new lease via `write()`, while reservers always read the CURRENT one
/// via `read()` — a stale lease snapshot is impossible (B2: previously a reserver
/// held a captured `Arc<ReserveLease>`, and flush swapped its own ⇒ the old reserver
/// handed out slots that had already returned to free_list and been re-leased = double
/// issue of the same index). Concurrent reservers take a read-lock (parallel
/// with each other), flush takes a write-lock (exclusive, at the apply sync point where no
/// system is running).
type LeaseCell = Arc<RwLock<Arc<ReserveLease>>>;

#[inline]
fn read_lease(cell: &LeaseCell) -> Arc<ReserveLease> {
    // Clone under a brief read-lock; cursor operations run on the clone (the shared
    // atomic `cursor` — the same lease that other reservers see).
    cell.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Hand out one `Entity` from the lease (reuse) or a fresh one from high-water. Shared by
/// [`EntityReserver::reserve`] and the [`EntityAllocator::allocate`] fallback.
#[inline]
fn reserve_from(lease: &ReserveLease, high_water: &AtomicU32) -> Entity {
    let n = lease.cursor.fetch_sub(1, Ordering::Relaxed);
    if n > 0 {
        let (index, generation) = lease.free[(n - 1) as usize];
        Entity { index, generation }
    } else {
        let index = high_water.fetch_add(1, Ordering::Relaxed);
        Entity { index, generation: 0 }
    }
}

/// Lock-free reserver of entity indices — split off from [`EntityAllocator`] so that
/// [`Commands`](crate::commands::Commands) can obtain a real `Entity` via `&self` (without
/// `&mut World`) while a system is still running (1:1 Bevy `Entities::reserve_entity`).
///
/// **Reuses free slots** (TD-39): holds a shared high-water (`Arc<AtomicU32>`) AND the current
/// lease of free slots ([`ReserveLease`], `Arc`, re-leased on every flush). `reserve()`
/// first reuses lease slots (the generation from the snapshot is valid — a slot in the lease is not touched until
/// consumed), and only bumps high-water once the lease is exhausted. This keeps `records` from growing per
/// `cmd.spawn` (previously reservation was monotonic ⇒ unbounded leak under command churn).
/// Until flush the entity is "reserved but not alive" (`is_alive`=false) — like `commands.spawn().id()` in
/// Bevy before the sync point.
/// D8b: private deterministic id-block cursor. A per-system reserver hands out
/// `base, base+1, …` from `next` until `end`, with NO cross-system contention and a
/// deterministic order (the block base is assigned by the scheduler in system-rank
/// order). `next` is atomic only so the reserver stays `Send + Sync`; spawning
/// systems are never row-split (`Commands` ⇒ `non_query_side_effects` ⇒ single ASD
/// task), so in practice one thread drives one block.
/// D8b: a private, pre-computed deterministic id-block. The scheduler reserves it on
/// the main thread in system-rank order ([`EntityAllocator::reserve_block`]), which
/// draws REUSED freed slots (deterministic order, carrying their generations) plus
/// fresh indices into `ids`. A per-system reserver hands out `ids[0], ids[1], …` via
/// `next` with NO cross-system contention and a deterministic order. Because the block
/// draws freed slots (reuse-aware), the id-space stays BOUNDED under despawn+respawn
/// churn and reuse is deterministic; the unconsumed tail (`ids[next..]`) is reclaimed
/// to the free-list after the stage ([`EntityAllocator::reclaim_block_tail`]).
struct BlockCursor {
    ids: Box<[Entity]>,
    next: AtomicU32,
    /// D8b overflow frontier (escrow): set when a reserve falls THROUGH this block
    /// (block + escrow tail exhausted) to the shared, NON-deterministic path. Read on
    /// the main thread post-apply for loud overflow telemetry (§0.2a). Written only on
    /// the rare overflow path — the common in-block reserve never touches it.
    overflowed: AtomicBool,
}

/// B5: shared channel for reservation indices abandoned WITHOUT `apply` (a
/// reserver-bound `Commands` dropped/cleared with un-applied `spawn().id()`s). The
/// `pending` flag lets [`EntityAllocator::flush`] skip the lock on the hot path (the
/// overwhelmingly common case is nothing abandoned), so the fast path stays
/// lock-free; the `Mutex` is taken only when something was actually abandoned (a rare
/// drop/clear) or drained.
#[derive(Default)]
struct AbandonQueue {
    pending: std::sync::atomic::AtomicBool,
    indices: Mutex<Vec<u32>>,
}

#[derive(Clone)]
pub struct EntityReserver {
    high_water: Arc<AtomicU32>,
    lease: LeaseCell,
    /// D8b: optional deterministic block. When present, `reserve()` draws from the
    /// private block (deterministic, contention-free) until exhausted, then falls
    /// back to the shared high-water/lease.
    block: Option<Arc<BlockCursor>>,
    /// B5: shared channel for reservations abandoned without `apply` (see
    /// [`abandon`](Self::abandon) and [`EntityAllocator::flush`]). Same `Arc` the
    /// owning allocator drains.
    abandoned: Arc<AbandonQueue>,
}

impl EntityReserver {
    /// Reserve an `Entity` (reuses a free slot or a fresh one). Safe from
    /// parallel systems: read-lock on the lease cell (parallel across reservers) → atomic
    /// cursor of the current lease + high-water. Always sees the CURRENT lease (B2).
    ///
    /// D8b: if the reserver is seeded with a block (`with_ids`), it first hands out ids from the private
    /// block (deterministic, contention-free); on block exhaustion — the shared path.
    #[inline]
    pub fn reserve(&self) -> Entity {
        if let Some(block) = &self.block {
            let i = block.next.fetch_add(1, Ordering::Relaxed);
            if (i as usize) < block.ids.len() {
                return block.ids[i as usize];
            }
            // Block + escrow exhausted — fall through to the shared reserver
            // (non-deterministic that frame). Adaptive sizing + the escrow margin make
            // this a rare spike-only event; the fall-through is now flagged for LOUD
            // post-apply telemetry (§0.2a) rather than silent.
            block.overflowed.store(true, Ordering::Relaxed);
        }
        let lease = read_lease(&self.lease);
        reserve_from(&lease, &self.high_water)
    }

    /// D8b: a clone of the reserver bound to the deterministic block `ids`. `reserve()`
    /// hands out `ids[0], ids[1], …` via a private counter (contention-free, deterministic),
    /// and after exhaustion — the shared path. The caller ([`EntityAllocator::reserve_block`])
    /// builds `ids`, drawing reused slots from the lease + fresh ones from high-water.
    pub(crate) fn with_ids(&self, ids: Box<[Entity]>) -> EntityReserver {
        EntityReserver {
            high_water: Arc::clone(&self.high_water),
            lease: Arc::clone(&self.lease),
            block: Some(Arc::new(BlockCursor {
                ids,
                next: AtomicU32::new(0),
                overflowed: AtomicBool::new(false),
            })),
            abandoned: Arc::clone(&self.abandoned),
        }
    }

    /// D8b overflow telemetry: did any reserve fall THROUGH this reserver's block
    /// (block + escrow) to the shared, non-deterministic path? Read on the main thread
    /// after the stage applies. `false` for a reserver without a block. When `true`,
    /// this stage's overflow ids are NOT run-to-run deterministic — the scheduler warns
    /// loudly and grows the block (§0.2a).
    pub fn block_overflowed(&self) -> bool {
        self.block
            .as_ref()
            .map(|b| b.overflowed.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// B5: return reserved-but-un-applied `Entity`s to the owning allocator's
    /// abandoned queue so their id-space is reclaimed. Called by [`Commands`] when it
    /// is dropped or cleared with un-applied `spawn().id()` reservations still queued
    /// — those ids advanced the shared high-water / consumed lease slots but were
    /// never materialized, so without this they leak the id-space (TD-40 fixed only
    /// the count). `PLACEHOLDER`s (standalone `Commands` with no reserver) carry no
    /// reservation and are skipped. The allocator drains the queue in [`flush`] AFTER
    /// growing `records`, pushing the indices back to `free_list` WITHOUT a generation
    /// bump (they were never alive — same rationale as
    /// [`reclaim_block_tail`](EntityAllocator::reclaim_block_tail)).
    pub fn abandon(&self, entities: &[Entity]) {
        let mut guard = self
            .abandoned
            .indices
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.extend(
            entities
                .iter()
                .filter(|e| **e != Entity::PLACEHOLDER)
                .map(|e| e.index),
        );
        if guard.len() != before {
            // Publish AFTER the push (release) so a flush that observes the flag also
            // sees the queued indices.
            self.abandoned
                .pending
                .store(true, Ordering::Release);
        }
    }

    /// D8b: how many ids remain in the block (for adaptive sizing/reclaim).
    /// `None` — a reserver without a block.
    pub fn block_remaining(&self) -> Option<u32> {
        self.block
            .as_ref()
            .map(|b| (b.ids.len() as u32).saturating_sub(b.next.load(Ordering::Relaxed)))
    }

    /// D8b: the unused tail of the block (`ids[next..]`) — reserved but NOT
    /// materialized ids (the system spawned fewer than the block size). Their generations are
    /// untouched (never were alive), so the scheduler returns them to the reuse pool
    /// after the stage ([`EntityAllocator::reclaim_block_tail`]) → the id-space stays bounded
    /// under churn. Empty if there is no block or it is exhausted (overflow).
    pub fn unused_block_ids(&self) -> Vec<Entity> {
        self.block
            .as_ref()
            .map(|b| {
                let next = b.next.load(Ordering::Relaxed) as usize;
                b.ids.get(next..).map(|s| s.to_vec()).unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Reserve `n` `Entity`s with a minimum of atomic operations (one lease `fetch_sub` + one
    /// high-water `fetch_add` per batch) — for bulk spawn
    /// ([`Commands::spawn_batch`](crate::commands::Commands::spawn_batch)).
    pub fn reserve_n(&self, n: usize) -> Vec<Entity> {
        if n == 0 {
            return Vec::new();
        }
        // D8b: block mode — draw `n` contiguously from the private block (deterministic,
        // no contention). Single-task per spawning system, so plain load+store is sound.
        // On block overflow, fall through to the shared path (rare; rank-ordered overflow
        // determinism is a follow-up, §4.1).
        if let Some(block) = &self.block {
            let cur = block.next.load(Ordering::Relaxed) as usize;
            if cur + n <= block.ids.len() {
                block.next.store((cur + n) as u32, Ordering::Relaxed);
                return block.ids[cur..cur + n].to_vec();
            }
            // Not enough contiguous block+escrow ids for this batch — fall through to
            // the shared (non-deterministic) path and flag for loud telemetry.
            block.overflowed.store(true, Ordering::Relaxed);
        }
        let lease = read_lease(&self.lease);
        let old = lease.cursor.fetch_sub(n as i64, Ordering::Relaxed);
        let reuse = old.max(0).min(n as i64) as usize; // how many taken from the lease
        let fresh = n - reuse;
        let mut out = Vec::with_capacity(n);
        for k in 0..reuse {
            let (index, generation) = lease.free[old as usize - 1 - k];
            out.push(Entity { index, generation });
        }
        if fresh > 0 {
            let start = self.high_water.fetch_add(fresh as u32, Ordering::Relaxed);
            for j in 0..fresh as u32 {
                out.push(Entity { index: start + j, generation: 0 });
            }
        }
        out
    }
}

/// Entity manager — generational IDs with a batch API + reservation with reuse (TD-39).
pub struct EntityAllocator {
    /// Shared high-water of fresh indices (shared with the reserver and the allocator) — advances only
    /// when the lease/free_list are exhausted, so `records.len()` ≈ the peak of concurrent entities.
    high_water: Arc<AtomicU32>,
    /// Shared cell of the current lease (re-leased on flush; see [`LeaseCell`]).
    /// Shared with ALL [`EntityReserver`]s — publishing a new lease through it
    /// rules out a stale snapshot (B2).
    lease: LeaseCell,
    /// The allocator's OWN handle on the current lease — the same `Arc` the cell holds, not a
    /// snapshot of it.
    ///
    /// [`allocate`](Self::allocate) used to reach the lease the way a RESERVER has to: read-lock
    /// the cell, clone the `Arc` out of it, use it, drop it. That is four atomic
    /// read-modify-writes per entity spent looking up a value THIS struct publishes and is the
    /// only writer of. The ladder measured the whole of `allocate` at 30 ns — 46 % of a
    /// four-component `World::spawn` and more than bevy's entire spawn (probe `spawn_ladder`,
    /// 2026-08-30). The cell stays exactly as it was: reservers read it, and reading it is what
    /// makes a stale snapshot impossible for them (B2). What is removed is the allocator asking a
    /// lock for something it is holding.
    ///
    /// INVARIANT this rests on: it is `ptr_eq` to the cell's content at all times.
    /// [`refresh_lease`](Self::refresh_lease) is the ONLY writer of the cell and it sets both
    /// from one value. Gated by `the_allocators_lease_handle_is_the_shared_one`.
    owned_lease: Arc<ReserveLease>,
    records: Vec<EntityRecord>,
    free_list: Vec<u32>,
    /// Count of **live** (located) records — maintained in O(1) on location transitions (like `Entities::len`
    /// in Bevy). [`len`](Self::len) returns it directly: this is both cheaper than the formula `records − free − lease`,
    /// and CONSISTENT with [`is_alive`](Self::is_alive) — reservations materialized by `flush` as
    /// location-less records (for example orphaned when `Commands` is dropped without `apply`) are NOT counted
    /// here (previously they inflated the count — TD-40).
    live: u32,
    /// B5: shared channel of reservation indices abandoned WITHOUT `apply` (a
    /// reserver-bound `Commands` dropped/cleared with un-applied `spawn().id()`s).
    /// Pushed from [`EntityReserver::abandon`] (rare drop/clear path — never the hot
    /// reserve loop), drained into `free_list` by [`flush`](Self::flush) so the
    /// id-space stays bounded (TD-40 fixed the count; this stops the id-space leak).
    /// Shared via `Arc` with every reserver handle; the `pending` flag keeps `flush`
    /// lock-free when nothing was abandoned.
    abandoned: Arc<AbandonQueue>,
}

impl EntityAllocator {
    pub fn new() -> Self {
        let lease = Arc::new(ReserveLease {
            cursor: AtomicI64::new(0),
            free: Box::new([]),
        });
        Self {
            high_water: Arc::new(AtomicU32::new(0)),
            owned_lease: Arc::clone(&lease),
            lease: Arc::new(RwLock::new(lease)),
            records: Vec::new(),
            free_list: Vec::new(),
            live: 0,
            abandoned: Arc::new(AbandonQueue::default()),
        }
    }

    /// A reserver handle sharing high-water AND the current lease. Cloned into [`Commands`] (see
    /// [`World::entity_reserver`](crate::world::World::entity_reserver)).
    #[inline]
    pub fn reserver(&self) -> EntityReserver {
        EntityReserver {
            high_water: Arc::clone(&self.high_water),
            // Clone of the Arc onto the SAME cell — the reserver sees all future re-leases.
            lease: Arc::clone(&self.lease),
            block: None,
            abandoned: Arc::clone(&self.abandoned),
        }
    }

    /// D8b: reserve a deterministic block of `size` ids and return a reserver bound to
    /// it. **Reuse-aware:** first draws freed slots from the current
    /// lease (in deterministic descending order, like `reserve_from` — one bulk
    /// cursor `fetch_sub`), then tops up with fresh ones from high-water. Called by the scheduler
    /// on the main thread in stage system-rank order → each block's lease slice and fresh indices
    /// are deterministic, and reuse of freed slots keeps the
    /// id-space bounded under churn (see [`reclaim_block_tail`](Self::reclaim_block_tail)).
    /// `&self`: both the lease cursor and high-water are atomic (main thread, no system running).
    pub fn reserve_block(&self, size: u32) -> EntityReserver {
        let lease = read_lease(&self.lease);
        let mut ids: Vec<Entity> = Vec::with_capacity(size as usize);
        // Reused freed slots from the lease (descending, matching `reserve_from`).
        let old = lease.cursor.fetch_sub(size as i64, Ordering::Relaxed);
        let reuse = old.max(0).min(size as i64);
        for k in 0..reuse {
            let (index, generation) = lease.free[(old - 1 - k) as usize];
            ids.push(Entity { index, generation });
        }
        // Remainder: fresh indices from high-water (generation 0).
        let fresh = size - reuse as u32;
        if fresh > 0 {
            let start = self.high_water.fetch_add(fresh, Ordering::Relaxed);
            for j in 0..fresh {
                ids.push(Entity { index: start + j, generation: 0 });
            }
        }
        self.reserver().with_ids(ids.into_boxed_slice())
    }

    /// D8b: return the block's unused tail to the reuse pool. These `unused` ids were
    /// reserved (from the lease or high-water) but NOT materialized (the system
    /// spawned fewer than the block size), so their generations are untouched (never were
    /// alive) — we push the indices into `free_list` WITHOUT the `free()` generation bump. This keeps
    /// the id-space bounded under churn; the slots are re-leased on the
    /// next `flush`. The scheduler calls this in rank order → deterministic reuse.
    ///
    /// Call AFTER the stage `flush` (records grown up to high-water, so all
    /// `unused` indices are covered by records and `refresh_lease` will read their generation).
    pub fn reclaim_block_tail(&mut self, unused: &[Entity]) {
        for e in unused {
            debug_assert!(
                (e.index as usize) < self.records.len(),
                "reclaim_block_tail: index {} beyond records (call after flush)",
                e.index
            );
            self.free_list.push(e.index);
        }
    }

    /// Re-lease: the current `free_list` (new frees + returned) → a new lease; cursor
    /// = lease size. `free_list` is emptied (its slots are now in the lease, disjoint from future
    /// frees).
    fn refresh_lease(&mut self) {
        let free: Vec<(u32, u32)> = self
            .free_list
            .drain(..)
            .map(|i| (i, self.records[i as usize].generation))
            .collect();
        let cursor = free.len() as i64;
        let lease = Arc::new(ReserveLease {
            cursor: AtomicI64::new(cursor),
            free: free.into_boxed_slice(),
        });
        // ONE place sets BOTH: the shared cell every reserver reads (B2: the old snapshot is no
        // longer handed out) and the allocator's own handle on the same `Arc`. Writing them from
        // one value is what makes them incapable of disagreeing — see `owned_lease`.
        self.owned_lease = Arc::clone(&lease);
        *self.lease.write().unwrap_or_else(|e| e.into_inner()) = lease;
        // The invariant, asserted where it is established rather than only where it is used: a
        // handle that stopped being the cell's content hands out slots that were already
        // re-leased (the B2 defect, from the allocator's side this time). Every debug and test
        // build of every caller of `flush` is a witness.
        debug_assert!(
            self.lease_handle_is_the_shared_one(),
            "refresh_lease published a lease the allocator's own handle does not point at"
        );
    }

    /// Reconciliation at the apply boundary (`&mut World`): (1) return UN-consumed lease slots to free_list;
    /// (2) grow `records` up to high-water (fresh slots, generation 0); (3) re-lease. After
    /// this `spawn_reserved` sets components/location on the reserved entities.
    pub fn flush(&mut self) {
        // (2) Fresh indices (reserve/allocate advanced high-water) — materialize records.
        let hw = self.high_water.load(Ordering::Relaxed) as usize;
        if self.records.len() < hw {
            self.records.resize_with(hw, || EntityRecord {
                generation: 0,
                encoded_location: NO_LOCATION,
            });
        }
        // B5: reclaim indices abandoned by a dropped/cleared reserver-bound `Commands`.
        // `records` now covers them (step 2 grew to high-water), so pushing to free_list
        // is safe (refresh_lease reads their generation). No generation bump — they were
        // never alive (same rationale as `reclaim_block_tail`). The `pending` flag keeps
        // this lock-free on the hot path (nothing abandoned → no lock); drained before
        // the fast-path check so a non-empty free_list re-leases the returned slots.
        if self.abandoned.pending.swap(false, Ordering::Acquire) {
            let drained: Vec<u32> = {
                let mut guard = self
                    .abandoned
                    .indices
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *guard)
            };
            for index in drained {
                debug_assert!(
                    (index as usize) < self.records.len(),
                    "abandoned reservation index {} beyond records (flush grows to high-water first)",
                    index
                );
                self.free_list.push(index);
            }
        }
        // Read the current lease from the cell (a clone — the read-lock is released immediately, before
        // the write-lock in refresh_lease, otherwise self-deadlock).
        let lease = read_lease(&self.lease);
        let cursor = lease.cursor.load(Ordering::Relaxed).max(0) as usize;
        let unconsumed = cursor.min(lease.free.len());
        // Fast path — nothing to reconcile: no new frees, nothing abandoned, AND the lease
        // is untouched (every slot still unconsumed). The return + re-lease below would
        // then rebuild a byte-identical lease (same indices, same order, same generations
        // — `free` is the only path that changes a generation, and it always pushes to
        // `free_list`), so skipping it is observationally a no-op.
        //
        // The check used to be `free_list.is_empty() && lease.free.is_empty()`, which
        // stopped firing once ANY entity had been despawned: the freed indices live in the
        // lease from then on, so every later flush walked the whole pool and allocated a
        // fresh Vec + Arc. `Commands::apply` calls `World::flush_reserved` for every command
        // buffer of every stage of every frame, so the steady-state per-frame cost became
        // proportional to the number of DELETED entities — the app got SLOWER after a mass
        // delete, with FEWER entities alive. Found by the user in the editor after deleting
        // several hundred models; measured 87 µs per idle flush at 20k freed, tens of calls
        // per frame. Regression test: `idle_flush_does_not_rebuild_the_lease`.
        if self.free_list.is_empty() && unconsumed == lease.free.len() {
            return;
        }
        // (1) Un-consumed lease slots (free[0..cursor]) are still free — return them to free_list.
        for k in 0..unconsumed {
            self.free_list.push(lease.free[k].0);
        }
        drop(lease);
        // (3) Re-lease from the updated free_list (new frees + returned).
        self.refresh_lease();
    }

    /// Ensure `records` covers `index` (fresh intermediate slots — generation 0). Needed
    /// because reservations advance high-water without touching `records`; a direct `allocate` of a fresh index
    /// can run ahead of flush.
    #[inline]
    pub(crate) fn ensure_record(&mut self, index: u32) {
        let needed = index as usize + 1;
        if self.records.len() < needed {
            self.records.resize_with(needed, || EntityRecord {
                generation: 0,
                encoded_location: NO_LOCATION,
            });
        }
    }

    /// Allocate one entity. First reuses a NEW free (`free_list`, immediately), otherwise
    /// a lease slot or a fresh one (via the shared cursor/high-water — coordinated with the reserver; lease
    /// and free_list slots are disjoint ⇒ no collision).
    pub fn allocate(&mut self) -> Entity {
        if let Some(index) = self.free_list.pop() {
            let generation = self.records[index as usize].generation;
            Entity { index, generation }
        } else {
            let e = self.take_reused_or_fresh();
            self.ensure_record(e.index);
            e
        }
    }

    /// One id from the current lease, or a fresh one from high-water: [`reserve_from`] without
    /// the lock and the `Arc` traffic a `&self` reserver has to pay (see
    /// [`owned_lease`](Self::owned_lease)).
    ///
    /// The cursor is LOADED before it is decremented. An exhausted lease (`<= 0`) is the steady
    /// state of any world that spawns more than it despawns, and the unconditional decrement was
    /// an atomic read-modify-write per entity that could never hand anything back. The load is
    /// safe against a concurrent reserver: only `refresh_lease` raises the cursor and it needs
    /// `&mut self`, so a reserver can move it down but never back above zero — a lease this call
    /// saw as exhausted cannot become non-exhausted under it.
    #[inline]
    fn take_reused_or_fresh(&self) -> Entity {
        let lease = &self.owned_lease;
        if lease.cursor.load(Ordering::Relaxed) > 0 {
            let n = lease.cursor.fetch_sub(1, Ordering::Relaxed);
            if n > 0 {
                let (index, generation) = lease.free[(n - 1) as usize];
                return Entity { index, generation };
            }
            // Lost the race to a reserver between the load and the decrement — fall through to a
            // fresh index, exactly as `reserve_from` does.
        }
        Entity {
            index: self.high_water.fetch_add(1, Ordering::Relaxed),
            generation: 0,
        }
    }

    /// The invariant [`owned_lease`](Self::owned_lease) rests on, as a value a test can assert:
    /// the allocator's handle IS the lease the reservers read, not a copy that once matched.
    pub(crate) fn lease_handle_is_the_shared_one(&self) -> bool {
        let guard = self.lease.read().unwrap_or_else(|e| e.into_inner());
        Arc::ptr_eq(&self.owned_lease, &guard)
    }

    /// Allocate N entities in a single pass — batch API.
    ///
    /// **Perf (TD-39 regression fix):** atomic operations and the growth of `records` are BATCHED over the whole run
    /// (one lease `cursor.fetch_sub(N)` + one `high_water.fetch_add(N)` + one `resize_with`),
    /// rather than per-entity `reserve_from`/`ensure_record`. Semantics are identical (the shared cursor/high-water
    /// are shared with the reserver; the lease consumption order is the same descending one as in `reserve_n`),
    /// but 2 atomics + 1 resize per batch instead of 2×N atomics + N resizes. On `simple_insert`
    /// (10k) this removed ~150µs of "tax" from 20,000 lock instructions.
    pub fn allocate_batch(&mut self, count: usize) -> Vec<Entity> {
        let mut entities = Vec::with_capacity(count);
        // 1. Drain new frees (free_list) — immediately available.
        let from_free = count.min(self.free_list.len());
        for _ in 0..from_free {
            let index = self.free_list.pop().unwrap();
            let generation = self.records[index as usize].generation;
            entities.push(Entity { index, generation });
        }
        let remaining = count - from_free;
        if remaining == 0 {
            return entities;
        }
        // 2. Remainder from the lease — ONE fetch_sub (like reserve_n): consume free[old-1..old-reuse].
        //    Through the allocator's own handle: the batch path pays the lock only once, but
        //    there is no reason for it to pay it at all (see `owned_lease`).
        let reuse = {
            let lease = &self.owned_lease;
            let old = lease.cursor.fetch_sub(remaining as i64, Ordering::Relaxed);
            let reuse = old.max(0).min(remaining as i64) as usize;
            for k in 0..reuse {
                let (index, generation) = lease.free[old as usize - 1 - k];
                entities.push(Entity { index, generation });
            }
            reuse
        };
        // 3. Fresh indices — ONE high-water fetch_add + ONE records resize.
        let fresh = remaining - reuse;
        if fresh > 0 {
            let start = self.high_water.fetch_add(fresh as u32, Ordering::Relaxed);
            let end = start as usize + fresh;
            if self.records.len() < end {
                self.records.resize_with(end, || EntityRecord {
                    generation: 0,
                    encoded_location: NO_LOCATION,
                });
            }
            for j in 0..fresh as u32 {
                entities.push(Entity {
                    index: start + j,
                    generation: 0,
                });
            }
        }
        entities
    }

    /// Batch set_location — a single pass over the Vec without repeated bounds checks.
    ///
    /// Called from `spawn_many` after a batch allocate.
    /// `entities[i]` gets `EntityLocation { archetype_id, row: start_row + i }`.
    pub fn set_locations_batch(
        &mut self,
        entities: &[Entity],
        archetype_id: crate::archetype::ArchetypeId,
        start_row: u32,
    ) {
        for (i, entity) in entities.iter().enumerate() {
            let record = &mut self.records[entity.index as usize];
            // Check generation only in debug
            debug_assert_eq!(record.generation, entity.generation);
            let became_live = !record.has_location(); // location-less → located ⇒ +1 live
            record.set_location(EntityLocation {
                archetype_id,
                row: start_row + i as u32,
            });
            if became_live {
                self.live += 1;
            }
        }
    }

    pub fn free(&mut self, entity: Entity) -> bool {
        let record = match self.records.get_mut(entity.index as usize) {
            Some(r) => r,
            None => return false,
        };
        if record.generation != entity.generation {
            return false;
        }
        let was_live = record.has_location(); // located → location-less ⇒ −1 live
        record.generation = record.generation.wrapping_add(1);
        record.clear_location();
        if was_live {
            self.live -= 1;
        }
        // W3-3: ABA protection on generation wrap. free_list is LIFO, i.e.
        // churn concentrates on ONE slot (spawn+despawn of a temporary entity
        // every frame ≈ 2³² reuses over ~198 days @250Hz). A slot
        // that reaches u32::MAX is RETIRED (not returned to free_list):
        // no generation value is handed out twice → a stuck handle
        // from a past life never "comes alive" on a foreign entity. The cost is one
        // EntityRecord per 2³² reuses (PLACEHOLDER uses
        // generation == u32::MAX — it is never alive anyway).
        if record.generation != u32::MAX {
            self.free_list.push(entity.index);
        }
        true
    }

    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.records
            .get(entity.index as usize)
            .map(|r| r.generation == entity.generation && r.has_location())
            .unwrap_or(false)
    }

    #[inline]
    pub fn get_location(&self, entity: Entity) -> Option<EntityLocation> {
        self.records
            .get(entity.index as usize)
            .filter(|r| r.generation == entity.generation)
            .and_then(|r| r.location())
    }

    #[inline]
    pub fn set_location(&mut self, entity: Entity, location: EntityLocation) {
        let mut became_live = false;
        if let Some(record) = self.records.get_mut(entity.index as usize) {
            if record.generation == entity.generation {
                became_live = !record.has_location(); // location-less → located ⇒ +1 live
                record.set_location(location);
            }
        }
        if became_live {
            self.live += 1;
        }
    }

    /// Count of **live** (located) entities — consistent with [`is_alive`](Self::is_alive). O(1): returns
    /// the maintained `live` counter (does not count reserved-but-not-materialized records
    /// that `flush` creates location-less — TD-40). Cheaper than the former formula `records − free − lease`.
    pub fn len(&self) -> usize {
        self.live as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_by_index(&self, index: u32) -> Option<Entity> {
        let record = self.records.get(index as usize)?;
        if record.has_location() {
            Some(Entity {
                index,
                generation: record.generation,
            })
        } else {
            None
        }
    }
}

impl Default for EntityAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archetype::ArchetypeId;

    fn make_loc() -> EntityLocation {
        EntityLocation {
            archetype_id: ArchetypeId::EMPTY,
            row: 0,
        }
    }

    #[test]
    fn allocate_free_reuse() {
        let mut alloc = EntityAllocator::new();
        let e1 = alloc.allocate();
        let e2 = alloc.allocate();
        assert_ne!(e1, e2);

        alloc.set_location(e1, make_loc());
        alloc.set_location(e2, make_loc());
        assert!(alloc.is_alive(e1));

        alloc.free(e1);
        assert!(!alloc.is_alive(e1));

        let e3 = alloc.allocate();
        assert_eq!(e3.index, e1.index);
        assert_ne!(e3.generation, e1.generation);
        alloc.set_location(e3, make_loc());
        assert!(alloc.is_alive(e3));
    }

    /// An IDLE `flush` (nothing spawned, freed or abandoned since the last one) must
    /// be a no-op — it must NOT return the lease's untouched slots to `free_list` only
    /// to re-publish an identical lease.
    ///
    /// Found on a live editor: after mass-deleting several hundred models the app kept
    /// getting slower with FEWER entities alive. `Commands::apply` calls
    /// `World::flush_reserved` on every command buffer, every stage, every frame; with a
    /// non-empty lease the old fast-path check (`free_list.is_empty() && lease.free
    /// .is_empty()`) never fired, so each of those calls walked the whole free pool and
    /// allocated a fresh `Vec` + `Arc` of thousands of entries. The per-frame cost was
    /// proportional to the number of DELETED entities, which is why deleting made it
    /// slower. `Arc::ptr_eq` is the structural witness: an idle flush keeps the lease.
    #[test]
    fn idle_flush_does_not_rebuild_the_lease() {
        let mut alloc = EntityAllocator::new();
        let entities = alloc.allocate_batch(4096);
        for e in &entities {
            alloc.set_location(*e, make_loc());
        }
        // Mass delete: every index lands in the reuse pool, then the lease.
        for e in &entities {
            alloc.free(*e);
        }
        alloc.flush();
        let leased = read_lease(&alloc.lease);
        assert_eq!(leased.free.len(), 4096, "the freed indices are leased for reuse");

        // Idle frames: no spawn, no despawn, nothing abandoned.
        for _ in 0..8 {
            alloc.flush();
            let now = read_lease(&alloc.lease);
            assert!(
                Arc::ptr_eq(&leased, &now),
                "an idle flush must not re-publish the lease — that is O(free pool) \
                 plus an allocation on every Commands::apply of every stage"
            );
        }

        // …and the pool is still fully usable afterwards (the skip is not a leak).
        let reused = alloc.allocate_batch(4096);
        assert_eq!(reused.len(), 4096);
        let fresh_indices: Vec<u32> =
            reused.iter().map(|e| e.index).filter(|i| *i >= 4096).collect();
        assert!(fresh_indices.is_empty(), "every id came from the reuse pool: {fresh_indices:?}");
    }

    #[test]
    fn allocate_batch_basic() {
        let mut alloc = EntityAllocator::new();
        let batch = alloc.allocate_batch(100);
        assert_eq!(batch.len(), 100);
        // All indices are unique
        let mut indices: Vec<u32> = batch.iter().map(|e| e.index).collect();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(indices.len(), 100);
    }

    #[test]
    fn allocate_batch_uses_free_list() {
        let mut alloc = EntityAllocator::new();
        // Create 5, free 2, batch 5 — must reuse 2
        let entities: Vec<Entity> = (0..5)
            .map(|_| {
                let e = alloc.allocate();
                alloc.set_location(e, make_loc());
                e
            })
            .collect();

        alloc.free(entities[1]);
        alloc.free(entities[3]);

        let batch = alloc.allocate_batch(5);
        assert_eq!(batch.len(), 5);

        // Two of the batch must have the same indices as the freed ones
        let batch_indices: std::collections::HashSet<u32> = batch.iter().map(|e| e.index).collect();
        assert!(batch_indices.contains(&entities[1].index));
        assert!(batch_indices.contains(&entities[3].index));
    }

    // ── W3-3: generation-wrap / ABA ────────────────────────────

    #[test]
    fn slot_at_max_generation_is_retired_not_reused() {
        let mut alloc = EntityAllocator::new();
        let e = alloc.allocate();
        alloc.set_location(e, make_loc());

        // Simulate 2³²−2 reuses of the slot: bump generation
        // up to the second-to-last value (fields are private — the test is in the same module).
        alloc.records[e.index as usize].generation = u32::MAX - 1;
        let last_life = Entity::from_raw_parts(e.index, u32::MAX - 1);
        alloc.set_location(last_life, make_loc());
        assert!(alloc.is_alive(last_life));

        // free brings generation up to MAX → the slot is retired.
        assert!(alloc.free(last_life));
        assert!(!alloc.is_alive(last_life));
        assert!(
            alloc.free_list.is_empty(),
            "a slot at the generation boundary is not returned to free_list"
        );

        // The next allocate takes a NEW index, not the retired slot.
        let fresh = alloc.allocate();
        assert_ne!(fresh.index, e.index);

        // The ancient handle from the slot's first life (generation 0) is dead forever —
        // ABA is impossible: neither is_alive nor get_location yields a foreign entity.
        assert!(!alloc.is_alive(e));
        assert!(alloc.get_location(e).is_none());
    }

    #[test]
    fn get_by_index_dead_slot_returns_none() {
        let mut alloc = EntityAllocator::new();
        let e = alloc.allocate();
        alloc.set_location(e, make_loc());
        assert_eq!(alloc.get_by_index(e.index), Some(e));
        alloc.free(e);
        assert_eq!(
            alloc.get_by_index(e.index),
            None,
            "a slot without location (free/retired) does not yield an entity"
        );
    }

    // ── Atomic reservation (Commands::spawn().id()) ──────────

    #[test]
    fn reserve_produces_fresh_gen0_ids_then_flush_materializes() {
        let mut alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let a = r.reserve();
        let b = r.reserve();
        assert_ne!(a.index, b.index, "reservations yield different indices");
        assert_eq!(a.generation, 0);
        assert_eq!(b.generation, 0);
        // Before flush the slots do not exist → the entity is not alive.
        assert!(!alloc.is_alive(a));

        // flush creates the records; the spawn_reserved equivalent = set_location.
        alloc.flush();
        alloc.set_location(a, make_loc());
        alloc.set_location(b, make_loc());
        assert!(alloc.is_alive(a));
        assert!(alloc.is_alive(b));
    }

    /// TD-40 regression: `len()` (= `entity_count`) counts only LIVE (located) entities and is CONSISTENT with
    /// `is_alive` — reserved-but-not-materialized records (orphaned, as when
    /// `Commands` is dropped without `apply`) do NOT inflate the count, even after `flush` creates them as location-less.
    #[test]
    fn len_counts_only_located_ignoring_orphaned_reservations() {
        let mut alloc = EntityAllocator::new();
        // Two real entities.
        let a = alloc.allocate();
        alloc.set_location(a, make_loc());
        let b = alloc.allocate();
        alloc.set_location(b, make_loc());
        assert_eq!(alloc.len(), 2);

        // Three reservations that are NEVER materialized (like a dropped Commands buffer): they advance
        // the shared high-water, but no one sets a location on them.
        let r = alloc.reserver();
        let orphans = [r.reserve(), r.reserve(), r.reserve()];
        // flush grows `records` up to high-water → the orphans become location-less records.
        alloc.flush();

        assert_eq!(alloc.len(), 2, "orphaned reservations must not inflate entity_count (TD-40)");
        for o in orphans {
            assert!(!alloc.is_alive(o), "an orphaned reservation is not alive");
        }
        // Despawn a real entity → the count decreases; a free slot is also not counted.
        assert!(alloc.free(a));
        assert_eq!(alloc.len(), 1, "len is consistent with is_alive after despawn");
        assert!(!alloc.is_alive(a) && alloc.is_alive(b));
    }

    /// B5: reservations abandoned WITHOUT `apply` (as a dropped/cleared reserver-bound
    /// `Commands` produces) are returned to the pool on `flush`, so the id-space stays
    /// bounded instead of leaking one index per abandoned reservation.
    #[test]
    fn abandoned_reservations_are_reclaimed_on_flush() {
        let mut alloc = EntityAllocator::new();
        let r = alloc.reserver();
        // Reserve three fresh ids (indices 0,1,2) — this advances the shared high-water.
        let a = r.reserve();
        let b = r.reserve();
        let c = r.reserve();
        assert_eq!([a.index, b.index, c.index], [0, 1, 2]);

        // Abandon them (as a dropped reserver-bound Commands would) + flush.
        r.abandon(&[a, b, c]);
        alloc.flush();

        // The next reservations REUSE the abandoned indices rather than growing the
        // id-space to 3,4,5 — the leak is gone.
        let reused: std::collections::HashSet<u32> =
            [r.reserve().index, r.reserve().index, r.reserve().index]
                .into_iter()
                .collect();
        assert_eq!(
            reused,
            [0, 1, 2].into_iter().collect(),
            "abandoned indices are reused — id-space stays bounded"
        );

        // PLACEHOLDER carries no reservation → skipped (no panic, no bogus index).
        r.abandon(&[Entity::PLACEHOLDER]);
        alloc.flush();
        // Still only indices 0,1,2 ever handed out (no phantom index from PLACEHOLDER).
        assert_eq!(alloc.high_water.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn reserve_and_direct_allocate_never_collide() {
        let mut alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let mut indices = std::collections::HashSet::new();
        // Interleave reservation and direct allocate — the shared high-water prevents index collisions.
        for _ in 0..50 {
            assert!(indices.insert(r.reserve().index));
            let e = alloc.allocate();
            alloc.set_location(e, make_loc());
            assert!(indices.insert(e.index), "direct allocate does not intersect reservations");
        }
        alloc.flush();
    }

    #[test]
    fn reservation_reuses_freed_slots_after_flush() {
        let mut alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let a = r.reserve();
        let b = r.reserve();
        let c = r.reserve();
        alloc.flush();
        for e in [a, b, c] {
            alloc.set_location(e, make_loc());
        }
        let records_after_3 = alloc.records.len();
        assert_eq!(records_after_3, 3);

        // Free b, re-lease — slot b ends up in the lease.
        alloc.free(b);
        alloc.flush();

        // The next reservation REUSES slot b (not a fresh index) ⇒ records does NOT grow.
        let r2 = alloc.reserver();
        let d = r2.reserve();
        assert_eq!(d.index, b.index, "the reservation reused the freed slot");
        assert_ne!(d.generation, b.generation, "a new generation of the slot");
        alloc.flush();
        alloc.set_location(d, make_loc());
        assert!(alloc.is_alive(d));
        assert!(!alloc.is_alive(b), "the old handle b is dead (ABA-safe)");
        assert_eq!(
            alloc.records.len(),
            records_after_3,
            "records did NOT grow — the reservation reused a slot rather than allocating a fresh one"
        );
    }

    /// B2: a reserver that SURVIVED a flush must hand out from the CURRENT lease, not
    /// from a stale snapshot. Previously it held a captured `Arc<ReserveLease>`; after
    /// flush its unconsumed slots returned to free_list and were re-leased,
    /// The allocator holds its OWN handle on the lease so `allocate` does not read-lock a cell
    /// and clone an `Arc` per entity (see `owned_lease`). That handle is only ever safe while it
    /// IS the cell's content — a handle that merely matched once would hand out slots that have
    /// already been re-leased, which is exactly the B2 defect from the reserver side.
    ///
    /// So the invariant is asserted after every operation that can publish a new lease, and the
    /// reuse is checked to actually come THROUGH the handle: a stale handle with a live cursor
    /// would still hand ids back, so `ptr_eq` alone would not notice it had gone quiet.
    #[test]
    fn the_allocators_lease_handle_is_the_shared_one() {
        let mut a = EntityAllocator::new();
        assert!(a.lease_handle_is_the_shared_one(), "fresh allocator");

        let first: Vec<Entity> = (0..8).map(|_| a.allocate()).collect();
        assert!(a.lease_handle_is_the_shared_one(), "after fresh allocations");

        // Free half of them, then flush: `refresh_lease` drains free_list INTO a new lease, so
        // the next allocate must reach those slots through the handle and not through free_list.
        for e in &first[..4] {
            a.free(*e);
        }
        a.flush();
        assert!(a.lease_handle_is_the_shared_one(), "after flush");

        let reused = a.allocate();
        assert!(
            first[..4].iter().any(|e| e.index == reused.index),
            "reuse did not come from the leased slots — the handle is not the lease being read"
        );
        assert_eq!(
            reused.generation, 1,
            "a reused slot must carry the bumped generation from the lease"
        );
        assert!(a.lease_handle_is_the_shared_one(), "after a leased allocation");

        // A reserver taken out, then another flush: the cell is republished while a handle to the
        // old lease is alive elsewhere.
        let reserver = a.reserver();
        a.flush();
        assert!(a.lease_handle_is_the_shared_one(), "after a flush with a reserver alive");
        let from_reserver = reserver.reserve();
        assert!(
            first.iter().all(|e| e.index != from_reserver.index)
                || from_reserver.generation != 0,
            "reserver and allocator handed out the same live index"
        );
        assert!(a.lease_handle_is_the_shared_one(), "after the reserver drew");
    }

    /// while the old reserver still handed them out — the same indices also went out via
    /// direct `allocate` (double issue of one slot).
    #[test]
    fn reserver_surviving_flush_never_double_issues() {
        let mut alloc = EntityAllocator::new();
        // Fill free_list with six slots (allocate → materialization → free).
        let first: Vec<Entity> = (0..6).map(|_| alloc.allocate()).collect();
        alloc.flush();
        for &e in &first {
            alloc.set_location(e, make_loc());
        }
        for &e in &first {
            assert!(alloc.free(e));
        }
        alloc.flush(); // lease L1 received 6 slots, free_list is empty

        // The reserver is captured ONCE and survives the next flush.
        let r = alloc.reserver();
        let x0 = r.reserve(); // partially consume L1
        let x1 = r.reserve();
        alloc.flush(); // L1's unconsumed slots → free_list → new lease L2

        // All handed-out indices must be unique: the old reserver `r`
        // (through the same cell now sees L2) and direct allocate share one cursor.
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(x0.index));
        assert!(seen.insert(x1.index));
        for _ in 0..4 {
            let a = r.reserve();
            assert!(seen.insert(a.index), "the reserver handed out index {} twice", a.index);
            let b = alloc.allocate();
            alloc.set_location(b, make_loc());
            assert!(seen.insert(b.index), "allocate collided with the reserver at {}", b.index);
        }
        alloc.flush();
    }

    /// Main TD-39 test: steady churn via RESERVATION (like `cmd.spawn`) does NOT grow `records`
    /// unboundedly — slots are reused. Previously (monotonic reservation) records = sum-of-all-
    /// spawns (unbounded leak). 500 "frames" of refill-to-peak + despawn of half.
    #[test]
    fn reservation_churn_keeps_records_bounded() {
        let mut alloc = EntityAllocator::new();
        let peak = 200usize;
        let mut live: Vec<Entity> = Vec::new();
        for frame in 0..500 {
            // Refill to peak via RESERVATION (reuses last frame's slots).
            let r = alloc.reserver();
            while live.len() < peak {
                live.push(r.reserve());
            }
            alloc.flush();
            for &e in &live {
                if alloc.get_location(e).is_none() {
                    alloc.set_location(e, make_loc());
                }
            }
            // Despawn half (deterministically), re-lease.
            let mut kept = Vec::new();
            for (i, &e) in live.iter().enumerate() {
                if i % 2 == frame % 2 {
                    assert!(alloc.free(e), "free of a live entity");
                } else {
                    kept.push(e);
                }
            }
            live = kept;
            alloc.flush();
            for &e in &live {
                assert!(alloc.is_alive(e), "the surviving entity stayed alive with the correct generation");
            }
        }
        // records ≈ peak, NOT ~frames×refill (≈50k in the old leak) — proof of the fix.
        assert!(
            alloc.records.len() <= peak * 2,
            "records is bounded by the peak ({peak}) — leak eliminated; got {}",
            alloc.records.len()
        );
    }

    #[test]
    fn parallel_reserve_yields_unique_indices() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let alloc = EntityAllocator::new();
        let r = alloc.reserver();
        let count = AtomicU32::new(0);
        // 8 threads × 1000 reservations each — all indices unique (lock-free fetch_add).
        let all: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..8 {
                let r = r.clone();
                let all = &all;
                let count = &count;
                s.spawn(move || {
                    let mut local = Vec::with_capacity(1000);
                    for _ in 0..1000 {
                        local.push(r.reserve().index);
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    all.lock().unwrap().extend(local);
                });
            }
        });
        assert_eq!(count.load(Ordering::Relaxed), 8000);
        let mut v = all.into_inner().unwrap();
        v.sort_unstable();
        v.dedup();
        assert_eq!(v.len(), 8000, "no index handed out twice");
    }

    #[test]
    fn set_locations_batch() {
        let mut alloc = EntityAllocator::new();
        let entities = alloc.allocate_batch(10);
        let arch_id = ArchetypeId(42);
        alloc.set_locations_batch(&entities, arch_id, 0);

        for (i, entity) in entities.iter().enumerate() {
            let loc = alloc.get_location(*entity).unwrap();
            assert_eq!(loc.archetype_id.0, 42);
            assert_eq!(loc.row as usize, i);
        }
    }

    // ── D8b: deterministic id-block reserver ────────────────────────────────
    #[test]
    fn block_reserver_hands_out_contiguous_deterministic_ids() {
        let alloc = EntityAllocator::new();
        let r = alloc.reserve_block(5);
        let ids: Vec<u32> = (0..5).map(|_| r.reserve().index).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4], "block yields base+0..size in order");
        assert_eq!(r.block_remaining(), Some(0));
    }

    #[test]
    fn block_reserver_overflow_falls_back_to_shared() {
        let alloc = EntityAllocator::new();
        let r = alloc.reserve_block(2); // reserves [0,2) from high_water; hw now 2
        assert_eq!(r.reserve().index, 0);
        assert_eq!(r.reserve().index, 1);
        // block exhausted → shared path draws the next fresh index (2).
        assert_eq!(r.reserve().index, 2);
    }

    #[test]
    fn block_reserver_n_is_contiguous() {
        let alloc = EntityAllocator::new();
        let r = alloc.reserve_block(10);
        let batch = r.reserve_n(4);
        assert_eq!(
            batch.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(r.reserve().index, 4);
    }

    #[test]
    fn per_system_blocks_are_deterministic_across_runs() {
        // Model a stage: systems in rank order get blocks; each spawns some ids.
        // Two independent runs must produce IDENTICAL id assignment (D8b capstone
        // shape at the reserver level — the scheduler seeds blocks in rank order).
        fn run(sizes: &[u32], spawns: &[u32]) -> Vec<Vec<u32>> {
            let alloc = EntityAllocator::new();
            let reservers: Vec<EntityReserver> =
                sizes.iter().map(|&s| alloc.reserve_block(s)).collect();
            // Spawns happen "in parallel" but each system drives its own block, so
            // the id set per system is a pure function of (rank, spawn index).
            reservers
                .iter()
                .zip(spawns)
                .map(|(r, &n)| (0..n).map(|_| r.reserve().index).collect())
                .collect()
        }
        let sizes = [4u32, 4, 4];
        let spawns = [3u32, 2, 4];
        let a = run(&sizes, &spawns);
        let b = run(&sizes, &spawns);
        assert_eq!(a, b, "per-system block id assignment is deterministic");
        // System 0 (rank 0, block base 0) → 0,1,2; system 1 (base 4) → 4,5;
        // system 2 (base 8) → 8,9,10,11.
        assert_eq!(a, vec![vec![0, 1, 2], vec![4, 5], vec![8, 9, 10, 11]]);
    }

    // ── D8b reuse-aware blocks: deterministic reuse + bounded id-space under churn ──

    /// `reserve_block` draws FREED slots (reuse) before fresh high-water indices, in a
    /// deterministic descending order — the same slots a plain `reserve()` would reuse.
    #[test]
    fn reserve_block_draws_freed_slots_before_fresh() {
        let mut alloc = EntityAllocator::new();
        // Materialize + free three slots so the lease holds them.
        let es: Vec<Entity> = (0..3).map(|_| alloc.allocate()).collect();
        for e in &es {
            alloc.set_location(*e, make_loc());
        }
        for e in &es {
            assert!(alloc.free(*e)); // gen -> 1; pushed to free_list
        }
        alloc.flush(); // free_list -> lease (3 reusable slots, gen 1)
        // A block of 5 draws the 3 reused slots (gen 1) + 2 fresh (gen 0).
        let r = alloc.reserve_block(5);
        let ids: Vec<Entity> = (0..5).map(|_| r.reserve()).collect();
        let reused: Vec<u32> = ids.iter().filter(|e| e.generation == 1).map(|e| e.index).collect();
        let fresh: Vec<u32> = ids.iter().filter(|e| e.generation == 0).map(|e| e.index).collect();
        assert_eq!(reused.len(), 3, "block reused the 3 freed slots (gen 1): {ids:?}");
        assert_eq!(fresh.len(), 2, "block drew 2 fresh slots (gen 0): {ids:?}");
        // The reused indices are exactly the freed ones (0,1,2), no double-issue.
        let mut ru = reused.clone();
        ru.sort_unstable();
        assert_eq!(ru, vec![0, 1, 2]);
    }

    /// Capstone (churn): a spawn+despawn steady-state loop driven by reuse-aware blocks
    /// keeps the id-space BOUNDED (no unbounded high-water growth) AND assigns identical
    /// ids run-to-run, while never double-issuing an index to a live entity.
    #[test]
    fn reuse_aware_blocks_bound_id_space_and_are_deterministic_under_churn() {
        const FRAMES: usize = 200;
        const K: u32 = 8; // spawns/despawns per frame
        const BLOCK: u32 = K + 4; // slack ⇒ an unused tail to reclaim each frame

        fn run() -> (u32, Vec<u32>) {
            let mut alloc = EntityAllocator::new();
            let mut prev: Vec<Entity> = Vec::new();
            let mut max_index = 0u32;
            let mut trace: Vec<u32> = Vec::new(); // first spawned index per frame
            for _ in 0..FRAMES {
                // 1. Reserve a deterministic block (reuse-aware) — stage start.
                let r = alloc.reserve_block(BLOCK);
                // 2. Spawn K ids from the block (the "parallel" phase).
                let spawned: Vec<Entity> = (0..K).map(|_| r.reserve()).collect();
                trace.push(spawned[0].index);
                for e in &spawned {
                    max_index = max_index.max(e.index);
                }
                // 3. Despawn the previous frame's entities (steady churn).
                for e in &prev {
                    assert!(alloc.free(*e), "despawn of a live entity must succeed");
                }
                // 4. Apply/flush — materialize records, refresh the lease.
                alloc.flush();
                // 5. Materialize the spawns (make them live). No id may already be live.
                for e in &spawned {
                    assert!(
                        !alloc.is_alive(*e),
                        "reused id {e:?} must not already be live (no double-issue)"
                    );
                    alloc.set_location(*e, make_loc());
                    assert!(alloc.is_alive(*e));
                }
                // 6. Reclaim the block's unused tail (after flush ⇒ records grown).
                alloc.reclaim_block_tail(&r.unused_block_ids());
                prev = spawned;
            }
            (max_index, trace)
        }

        let (max_a, trace_a) = run();
        let (max_b, trace_b) = run();

        // Determinism: identical id assignment across two independent runs — the
        // record/replay guarantee, now holding UNDER despawn+respawn churn.
        assert_eq!(trace_a, trace_b, "churn id assignment is deterministic run-to-run");
        assert_eq!(max_a, max_b);
        // Bounded id-space: reuse keeps the max index near the concurrent peak (~2·K),
        // NOT growing with FRAMES. Without reuse it would be ~FRAMES·K = 1600.
        assert!(
            max_a < 100,
            "id-space must stay bounded under churn (max_index={max_a}, peak≈2K=16)"
        );
    }
}
