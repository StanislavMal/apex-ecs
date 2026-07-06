//! Event system with per-reader cursors and automatic cleanup.
//!
//! # Concept
//!
//! - [`Events<T>`] — the core type. Holds two buffers:
//!   `pending` (written to in the current tick) and `events` (available for reading).
//! - [`EventCursor`] — a lightweight reader handle. Each reader
//!   registers its own cursor and advances it as it reads.
//! - After calling [`update()`](Events::update) the buffers are swapped:
//!   `pending` → `events`, and the old events from `events`
//!   are dropped (if every reader has read them).
//! - `update()` is called by the Scheduler after each Stage
//!   or manually via [`World::flush_all_events`].
//! - [`EntityEvent<T>`] — an event addressed to a specific entity.
//! - [`DelayedQueue<T>`] — deferred delivery of events after N ticks.
//!   Implemented via `BinaryHeap` for O(log N) insertion and O(K log N) flush.
//!
//! # `EventReadGuard` semantics
//!
//! `EventReadGuard` is an RAII wrapper. On **drop** it automatically advances
//! the reader cursor to the end of the buffer, regardless of how many events
//! were actually read via `iter()` or `as_slice()`.
//!
//! This means: if you called `guard.iter()` and read only part of the
//! events, the rest are skipped on the next read. To read only part of the
//! events and preserve the cursor, use [`Events::read_partial`].
//!
//! For "peek without advancing" use [`EventReadGuard::peek`].
//!
//! # Thread-safety
//!
//! `Events<T>` is not `Sync`. To send events from parallel
//! systems use [`Events::send_sync`], which is guarded by an internal
//! `Mutex<Vec<T>>`. The contents of `sync_pending` are merged into `pending`
//! on a call to [`Events::update`] or [`Events::flush_sync`].

use rustc_hash::FxHashMap;
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Mutex, OnceLock};

use crate::entity::Entity;

// ── Events ──────────────────────────────────────────────────────

/// Event queue with per-reader tracking of read progress.
///
/// # How it works
///
/// 1. Sender systems write events into `pending` (via `send()`).
/// 2. At the end of the tick `update()` swaps `pending` and `events`.
/// 3. Reader systems call `reader.iter(queue)` — receiving only the
///    unread events from the `events` buffer.
/// 4. After reading, the reader cursor is set to `events.len()`.
/// 5. Garbage collection: if every active reader has read all events,
///    the `events` buffer is cleared.
///
/// # Thread-safety and `send_sync`
///
/// The base methods `send()` / `send_batch()` require `&mut self` and are not `Sync`.
/// For parallel systems — `send_sync(&self, event)` writes into `sync_pending`
/// (an internal `Mutex<Vec<T>>`). The contents are merged into `pending` on
/// [`update()`] or an explicit [`flush_sync()`].
pub struct Events<T> {
    /// Buffer available for reading (previous tick).
    events: Vec<T>,
    /// Buffer new events are written into (current tick).
    pending: Vec<T>,
    /// Thread-safe buffer for parallel systems (4.4).
    /// Lazily initialized via `OnceLock` — zero overhead
    /// for single-threaded users.
    sync_pending: OnceLock<Mutex<Vec<T>>>,
    /// Reader state: `None` = reader removed, `Some(pos)` = current position.
    cursors: Vec<Option<u32>>,
    /// Counter for generating IDs of new readers.
    next_cursor_id: u32,
    /// List of freed EventCursor IDs for O(1) reuse.
    free_list: Vec<EventCursor>,
    /// Number of active readers whose cursor position < events.len().
    /// Invariant: lagging_count == cursors.iter().flatten().filter(|&&p| p < events.len()).count()
    /// Used for the O(1) check in `all_readers_caught_up()`.
    lagging_count: u32,
}

impl<T> Events<T> {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            pending: Vec::with_capacity(256),
            sync_pending: OnceLock::new(),
            cursors: Vec::new(),
            next_cursor_id: 0,
            free_list: Vec::new(),
            lagging_count: 0,
        }
    }

    /// Send an event into the current tick.
    #[inline]
    pub fn send(&mut self, event: T) {
        self.pending.push(event);
    }

    /// Send a batch of events.
    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        self.pending.extend(events);
    }

    /// Send an event from a parallel context (thread-safe).
    ///
    /// Writes into the internal `Mutex<Vec<T>>`. The contents are merged into the
    /// main `pending` buffer on the next call to [`update()`] or [`flush_sync()`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// // In a parallel system:
    /// events.send_sync(DamageEvent { amount: 10 });
    /// ```
    ///
    /// # Performance note
    ///
    /// Each call takes `Mutex::lock`. For bulk sending, prefer to
    /// collect events into a local `Vec` and then call `send_batch_sync`.
    ///
    /// Advanced escape hatch: golden path is the declared `EventWriter<T>`
    /// system parameter (the scheduler serializes writers), not `&Events<T>`.
    #[doc(hidden)]
    pub fn send_sync(&self, event: T) {
        let sync = self.get_or_init_sync();
        sync.lock().unwrap().push(event);
    }

    /// Send a batch of events from a parallel context (thread-safe).
    ///
    /// Takes the lock once for the whole batch — more efficient than repeated `send_sync`.
    #[doc(hidden)] // advanced escape hatch — see `send_sync`
    pub fn send_batch_sync(&self, events: impl IntoIterator<Item = T>) {
        let sync = self.get_or_init_sync();
        sync.lock().unwrap().extend(events);
    }

    /// Merge `sync_pending` into the main `pending`.
    ///
    /// Called automatically from [`update()`]. Can be called manually
    /// if you need to make events available before the next tick.
    #[doc(hidden)] // advanced — pairs with `send_sync`; no-op without it
    pub fn flush_sync(&mut self) {
        if let Some(sync) = self.sync_pending.get() {
            let mut guard = sync.lock().unwrap();
            if !guard.is_empty() {
                self.pending.append(&mut *guard);
            }
        }
    }

    /// Lazily initialize `sync_pending` — zero overhead for
    /// single-threaded users who never call `send_sync`.
    fn get_or_init_sync(&self) -> &Mutex<Vec<T>> {
        self.sync_pending.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Register a new reader.
    ///
    /// Returns an [`EventCursor`] that must be stored and passed
    /// on every call to [`iter()`](Events::iter).
    pub fn add_reader(&mut self) -> EventCursor {
        if let Some(cursor) = self.free_list.pop() {
            let idx = cursor.0 as usize;
            if idx < self.cursors.len() {
                self.cursors[idx] = Some(0);
            }
            if !self.events.is_empty() {
                self.lagging_count += 1;
            }
            self.assert_lagging_invariant();
            return cursor;
        }

        let id = self.next_cursor_id;
        self.next_cursor_id += 1;
        self.cursors.push(Some(0));
        if !self.events.is_empty() {
            self.lagging_count += 1;
        }
        self.assert_lagging_invariant();
        EventCursor(id)
    }

    /// Remove a reader.
    ///
    /// After removal the cursor stops being counted during GC,
    /// which may allow the `events` buffer to be cleared.
    pub fn remove_reader(&mut self, reader_id: EventCursor) {
        let idx = reader_id.0 as usize;
        if idx < self.cursors.len() {
            if let Some(pos) = self.cursors[idx] {
                if (pos as usize) < self.events.len() {
                    self.lagging_count = self.lagging_count.saturating_sub(1);
                }
            }
            self.cursors[idx] = None;
            self.free_list.push(reader_id);
        }
        // Compress tail of None cursors, but only those NOT in the free_list.
        // Cursors in the free_list may be reissued — their slot indices
        // must stay valid even after tail compression.
        while self.cursors.last().copied() == Some(None) {
            let last_idx = self.cursors.len() - 1;
            let would_violate = self.free_list.iter().any(|c| c.0 as usize == last_idx);
            if would_violate {
                break;
            }
            self.cursors.pop();
        }
        self.assert_lagging_invariant();
    }

    /// Number of active readers.
    pub fn reader_count(&self) -> usize {
        self.cursors.iter().filter(|c| c.is_some()).count()
    }

    /// Pre-allocate capacity for the pending buffer.
    ///
    /// Lets you avoid repeated reallocations during bulk event sending.
    /// Call from a system before `event_writer::send()` in a loop.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.pending.reserve(additional);
    }

    /// Update the buffers: move `pending` into `events`.
    ///
    /// Called by the Scheduler after each Stage or manually via [`World::flush_all_events`].
    /// If every reader has read the previous events, the `events` buffer
    /// is cleared before loading the new one. If not all readers caught up —
    /// the old events are appended to the end of the new buffer so that
    /// lagging readers can catch up on them.
    ///
    /// Also merges `sync_pending` (from `send_sync`) into `pending`.
    pub fn update(&mut self) {
        // Merge sync_pending into pending before the swap
        self.flush_sync();

        // Early out: if both buffers are empty — nothing to swap
        if self.events.is_empty() && self.pending.is_empty() {
            self.assert_lagging_invariant();
            return;
        }
        if self.all_readers_caught_up() {
            // Everyone read the current buffer: drop it, reset cursors to 0, and
            // make the pending events the whole new buffer (reusing the old
            // events allocation for `pending`).
            self.events.clear();
            for pos in self.cursors.iter_mut().flatten() {
                *pos = 0;
            }
            std::mem::swap(&mut self.events, &mut self.pending);
            self.pending.clear();
        } else {
            // Some readers are still behind. Append the NEW events AFTER the old
            // ones, so a lagging cursor at position `p` reads `old[p..]` and then
            // flows straight into the new events, and a caught-up cursor sitting
            // at `events.len()` lands exactly on the first new event — no cursor
            // shifting, nothing lost.
            //
            // The old code swapped `[new, old]` into place and shifted every
            // cursor forward by `new_count`, which left the new events *behind*
            // every cursor: no one ever read them and the next all-caught-up
            // `update` cleared them. That silently dropped every event produced
            // while any reader lagged (F1).
            let pending_cap = self.pending.capacity();
            self.events.append(&mut self.pending);
            // Vec::append zeroed pending's capacity — restore some so the next
            // tick does not allocate from scratch.
            self.pending.reserve(pending_cap.min(256));
        }

        // Recompute lagging_count for the new event state
        let new_event_len = self.events.len() as u32;
        self.lagging_count = self
            .cursors
            .iter()
            .flatten()
            .filter(|&&pos| pos < new_event_len)
            .count() as u32;

        self.assert_lagging_invariant();
    }

    /// Iterate over the unread events in the `events` buffer.
    ///
    /// Returns a slice — does not advance the cursor. To advance, call
    /// [`advance_reader_mut`] or use [`read`] (auto-advance on drop).
    #[inline]
    pub fn iter(&self, reader_id: &EventCursor) -> &[T] {
        let idx = reader_id.0 as usize;
        let cursor = self.cursors.get(idx).and_then(|c| c.as_ref());
        match cursor {
            Some(&pos) if (pos as usize) < self.events.len() => &self.events[pos as usize..],
            _ => &[],
        }
    }

    /// Mutable version that advances the cursor to the end of the buffer.
    ///
    /// ⚠ INVARIANT (F5): this must mutate ONLY cursor state (`cursors`,
    /// `lagging_count`) and never the `events` buffer. `EventIterator::Drop`
    /// calls it while `&events[i]` references it handed out may still be live;
    /// soundness relies on the mutated fields being disjoint from `events`
    /// (Tree Borrows is location-precise). Touching `events` here would make
    /// that drop undefined behavior.
    #[inline]
    pub fn advance_reader_mut(&mut self, reader_id: &EventCursor) {
        let idx = reader_id.0 as usize;
        let event_len = self.events.len() as u32;
        if let Some(Some(pos)) = self.cursors.get_mut(idx) {
            let old_pos = *pos;
            if old_pos < event_len {
                self.lagging_count = self.lagging_count.saturating_sub(1);
            }
            *pos = event_len;
        }
    }

    /// Advance the cursor by a given number of events.
    ///
    /// Used inside [`EventReadGuard`] during `read_partial()`.
    /// The cursor will not go past the end of the buffer.
    #[inline]
    pub fn advance_reader_by(&mut self, reader_id: &EventCursor, count: usize) {
        let idx = reader_id.0 as usize;
        let event_len = self.events.len() as u32;
        if let Some(Some(pos)) = self.cursors.get_mut(idx) {
            let old_pos = *pos;
            let new_pos = (old_pos as usize)
                .saturating_add(count)
                .min(self.events.len()) as u32;
            if old_pos < event_len && new_pos >= event_len {
                self.lagging_count = self.lagging_count.saturating_sub(1);
            }
            *pos = new_pos;
        }
    }

    /// Read the unread events with automatic cursor advance
    /// **to the end of the buffer** when the guard is dropped.
    ///
    /// # Semantics
    ///
    /// On dropping [`EventReadGuard`] the cursor **always** jumps to the end of
    /// the buffer, regardless of how many events were actually read via
    /// `iter()` / `as_slice()`. This means: a partial read leads to
    /// skipping the remaining events.
    ///
    /// If you want to read exactly N events and leave the rest —
    /// use [`read_partial`](Events::read_partial).
    ///
    /// If you want to peek at events without advancing the cursor —
    /// use [`EventReadGuard::peek`].
    #[inline]
    pub fn read(&mut self, reader_id: &EventCursor) -> EventReadGuard<'_, T> {
        let idx = reader_id.0 as usize;
        let start = self
            .cursors
            .get(idx)
            .and_then(|c| c.as_ref())
            .copied()
            .unwrap_or(0) as usize;
        let start = start.min(self.events.len());
        EventReadGuard {
            queue: self,
            reader_id: *reader_id,
            start,
        }
    }

    /// Read at most `max_count` events, advancing the cursor
    /// by exactly the number of events actually returned.
    ///
    /// # Difference from `read()`
    ///
    /// `read()` advances the cursor to the end of the buffer on drop.
    /// `read_partial(n)` advances the cursor by exactly `n` events on drop
    /// (or fewer, if there are fewer unread events in the buffer).
    ///
    /// Use it when you need to process events in batches:
    ///
    /// ```ignore
    /// // Process 32 events per tick, without losing the rest
    /// while let guard = events.read_partial(&cursor, 32) {
    ///     if guard.is_empty() { break; }
    ///     for ev in guard.iter() { process(ev); }
    ///     // On drop — the cursor advances by exactly guard.len()
    /// }
    /// ```
    #[inline]
    pub fn read_partial(
        &mut self,
        reader_id: &EventCursor,
        max_count: usize,
    ) -> PartialReadGuard<'_, T> {
        let idx = reader_id.0 as usize;
        let start = self
            .cursors
            .get(idx)
            .and_then(|c| c.as_ref())
            .copied()
            .unwrap_or(0) as usize;
        let start = start.min(self.events.len());
        let end = (start + max_count).min(self.events.len());
        let count = end - start;
        PartialReadGuard {
            queue: self,
            reader_id: *reader_id,
            start,
            count,
        }
    }

    /// Number of events across both buffers.
    #[inline]
    pub fn len(&self) -> usize {
        self.events.len() + self.pending.len()
    }

    /// Number of events in the read buffer (available for the current tick).
    #[inline]
    pub fn len_readable(&self) -> usize {
        self.events.len()
    }

    /// Number of events in the write buffer.
    #[inline]
    pub fn len_pending(&self) -> usize {
        self.pending.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.pending.is_empty()
    }

    /// Clear both buffers and reset all cursors.
    pub fn clear(&mut self) {
        self.events.clear();
        self.pending.clear();
        if let Some(sync) = self.sync_pending.get() {
            sync.lock().unwrap().clear();
        }
        self.free_list.clear();
        for pos in self.cursors.iter_mut().flatten() {
            *pos = 0;
        }
        self.lagging_count = 0;
    }

    /// Debug assertion: checks the lagging_count invariant.
    #[cfg(debug_assertions)]
    fn assert_lagging_invariant(&self) {
        let event_len = self.events.len() as u32;
        let actual = self
            .cursors
            .iter()
            .flatten()
            .filter(|&&pos| pos < event_len)
            .count() as u32;
        assert_eq!(
            self.lagging_count, actual,
            "lagging_count invariant violated: stored={}, actual={}",
            self.lagging_count, actual
        );
    }

    #[cfg(not(debug_assertions))]
    fn assert_lagging_invariant(&self) {}

    fn all_readers_caught_up(&self) -> bool {
        self.lagging_count == 0
    }
}

impl<T> Default for Events<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ── EventReadGuard ──────────────────────────────────────────────

/// RAII wrapper: on drop it automatically advances the cursor **to the end** of the buffer.
///
/// Created via [`Events::read`].
///
/// # Drop semantics
///
/// On drop the reader cursor is set to the end of the `events` buffer,
/// regardless of how many events were actually read via
/// `iter()` / `as_slice()`. If that is undesirable:
///
/// - Use [`EventReadGuard::peek`] to peek without advancing.
/// - Use [`Events::read_partial`] to advance by exactly N.
#[must_use = "EventReadGuard advances cursor to end on drop; \
              bind to variable to read events, \
              or use Events::read_partial() to advance by count"]
pub struct EventReadGuard<'q, T> {
    queue: &'q mut Events<T>,
    reader_id: EventCursor,
    start: usize,
}

impl<'q, T> EventReadGuard<'q, T> {
    /// Slice of the unread events.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.queue.events[self.start..]
    }

    /// Iterate without consuming the guard.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// Opt out of the automatic cursor advance.
    /// The cursor stays at its current position after the guard is dropped.
    pub fn peek(self) -> PeekGuard<'q, T> {
        let start = self.start;
        let queue_ptr: *const Events<T> = &*self.queue;
        // Prevent EventReadGuard::drop from running (which would advance the cursor)
        std::mem::forget(self);
        PeekGuard {
            // SAFETY: queue_ptr is derived from &'q mut Events<T>, valid for 'q.
            // self is forgotten (drop not run), but Events<T> outlives it.
            queue: unsafe { &*queue_ptr },
            start,
        }
    }
}

impl<T> std::ops::Deref for EventReadGuard<'_, T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> Drop for EventReadGuard<'_, T> {
    fn drop(&mut self) {
        // Advance the cursor to the end of the buffer.
        // Semantics: dropping the guard = "I read everything to the end of the buffer".
        // For partial reads use Events::read_partial().
        self.queue.advance_reader_mut(&self.reader_id);
    }
}

/// `for e in reader.read() { ... }` — the main event idiom of Bevy (engine
/// TD-24). The guard converts into an owning [`EventIterator`]; the cursor
/// advance to the end of the buffer happens when the iterator is dropped (i.e.
/// also on `break` — partial iteration skips the remainder, just like the guard
/// itself).
impl<'q, T> IntoIterator for EventReadGuard<'q, T> {
    type Item = &'q T;
    type IntoIter = EventIterator<'q, T>;

    fn into_iter(self) -> EventIterator<'q, T> {
        let reader_id = self.reader_id;
        let start = self.start;
        let queue: *mut Events<T> = &mut *self.queue;
        // Prevent EventReadGuard::drop from running — the advance moves
        // into the iterator's Drop.
        std::mem::forget(self);
        // SAFETY: queue is derived from the guard's &'q mut Events<T> (the guard
        // is forgotten, its Drop will not run — we are the sole owner of the
        // borrow). The `events` buffer is not mutated while the iterator is
        // live: the iterator's Drop only advances the cursor (the
        // cursors/lagging_count fields), never touching the buffer itself — the
        // same technique as in `EventReadGuard::peek`.
        let items = unsafe {
            let events: &[T] = &(*queue).events;
            events[start.min(events.len())..].iter()
        };
        EventIterator {
            items,
            queue,
            reader_id,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Iterate by reference — without consuming the guard (`for e in &guard`).
impl<'a, T> IntoIterator for &'a EventReadGuard<'_, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> std::slice::Iter<'a, T> {
        self.iter()
    }
}

// ── EventIterator ───────────────────────────────────────────────

/// Owning iterator over the unread events (Bevy `EventIterator` style):
/// yields `&T` with the lifetime of the original `Events` borrow; on drop it
/// advances the reader cursor **to the end of the buffer** (the
/// [`EventReadGuard`] semantics are preserved: `break` mid-loop skips the
/// remaining events).
///
/// Created via `IntoIterator` on [`EventReadGuard`]:
/// `for e in reader.read() { ... }`.
pub struct EventIterator<'q, T> {
    items: std::slice::Iter<'q, T>,
    queue: *mut Events<T>,
    reader_id: EventCursor,
    _marker: std::marker::PhantomData<&'q mut Events<T>>,
}

impl<'q, T> Iterator for EventIterator<'q, T> {
    type Item = &'q T;

    #[inline]
    fn next(&mut self) -> Option<&'q T> {
        self.items.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.items.size_hint()
    }
}

impl<T> ExactSizeIterator for EventIterator<'_, T> {}

impl<'q, T> DoubleEndedIterator for EventIterator<'q, T> {
    #[inline]
    fn next_back(&mut self) -> Option<&'q T> {
        self.items.next_back()
    }
}

impl<T> Drop for EventIterator<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the pointer is valid for 'q (see `IntoIterator` above). This
        // reborrows `&mut Events<T>` while `&'q T` references handed out by
        // `next()` may still be live (they carry lifetime 'q, so a caller can
        // `collect()` them and keep them past this drop). That is sound because
        // `advance_reader_mut` mutates ONLY the cursor state (`cursors`,
        // `lagging_count`) and NEVER the `events` buffer the outstanding `&T`
        // point into: Tree Borrows is location-precise, so the `&mut Events`
        // and the live `&events[i]` cover disjoint bytes and never conflict.
        // ⚠ INVARIANT (F5): if `advance_reader_mut` is ever changed to touch the
        // `events` buffer, this becomes undefined behavior — verified sound by
        // `cargo miri test -Zmiri-tree-borrows` (e.g. the
        // `event_iterator_collect_holds_references` test).
        unsafe {
            (*self.queue).advance_reader_mut(&self.reader_id);
        }
    }
}

// ── PartialReadGuard ────────────────────────────────────────────

/// RAII wrapper: on drop it advances the cursor by exactly `count` events.
///
/// Created via [`Events::read_partial`].
///
/// Lets you read events in batches without losing the unread ones:
///
/// ```ignore
/// // Process at most 64 events per tick
/// let guard = events.read_partial(&cursor, 64);
/// for ev in guard.iter() { process(ev); }
/// // On drop the cursor advances only by guard.len() (≤64)
/// ```
#[must_use = "PartialReadGuard advances cursor by count on drop; bind to variable to read events"]
pub struct PartialReadGuard<'q, T> {
    queue: &'q mut Events<T>,
    reader_id: EventCursor,
    start: usize,
    count: usize,
}

impl<'q, T> PartialReadGuard<'q, T> {
    /// Slice of events (at most `max_count`, as passed to `read_partial`).
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.queue.events[self.start..self.start + self.count]
    }

    /// Iterate without consuming the guard.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// Number of events in this guard.
    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether there are no events.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl<T> std::ops::Deref for PartialReadGuard<'_, T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> Drop for PartialReadGuard<'_, T> {
    fn drop(&mut self) {
        // Advance the cursor by exactly count — only the read events.
        self.queue.advance_reader_by(&self.reader_id, self.count);
    }
}

// ── PeekGuard ──────────────────────────────────────────────────

/// Wrapper for "peek without advancing".
///
/// Created via [`EventReadGuard::peek`].
/// On drop the cursor is NOT advanced — unlike [`EventReadGuard`],
/// which on drop always sets the cursor to the end of the buffer.
#[must_use = "PeekGuard prevents cursor advance; bind to variable to prevent accidental advance"]
pub struct PeekGuard<'q, T> {
    queue: &'q Events<T>,
    start: usize,
}

impl<T> PeekGuard<'_, T> {
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.queue.events[self.start..]
    }

    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }
}

impl<T> std::ops::Deref for PeekGuard<'_, T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

// ── EventCursor ─────────────────────────────────────────────────

/// Lightweight event-reader handle.
///
/// Created via [`Events::add_reader`].
/// Stored in [`EventReader`](crate::system_param::EventReader).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventCursor(pub(crate) u32);

// ── EntityEvent ─────────────────────────────────────────────────

/// An event addressed to a specific entity.
///
/// Lets you send an event and have it read only by the systems
/// that request events for a specific entity.
///
/// # Example
///
/// ```ignore
/// writer.send(EntityEvent::new(entity, Damage { amount: 10 }));
/// // ...
/// for ev in reader.iter_for_entity::<Damage>(entity) {
///     health -= ev.data.amount;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct EntityEvent<T> {
    /// Target entity.
    pub target: Entity,
    /// Event data.
    pub data: T,
}

impl<T> EntityEvent<T> {
    pub fn new(target: Entity, data: T) -> Self {
        Self { target, data }
    }
}

// ── DelayedQueue ────────────────────────────────────────────────

/// Deferred-event queue with O(log N) insertion.
///
/// # Algorithm
///
/// Internally — `BinaryHeap<(Reverse<deliver_at>, sequence, T)>`:
/// - `Reverse<deliver_at>` — a min-heap by delivery tick.
/// - `sequence` — a monotonic counter for stable ordering of events
///   with the same `deliver_at` (FIFO within a single tick).
///
/// # Complexity
///
/// | Operation | Old implementation | New implementation |
/// |----------|-------------------|------------------|
/// | `send_delayed` | O(1) (push) | O(log N) (heap push) |
/// | `flush_delayed` | O(N) (full scan) | O(K log N), K = number ready |
/// | Memory | Vec | BinaryHeap (Vec inside) |
///
/// For typical usage (rare deferred events, N < 1000)
/// the difference is negligible. With thousands of deferred events, flush_delayed
/// speeds up by roughly N/K times.
///
/// # How it works
///
/// 1. `send_delayed(event, delay_ticks)` — the event is stored with the label
///    `deliver_at = current_tick + delay_ticks`.
/// 2. Each call to `flush_delayed(current_tick)` moves all events
///    with `deliver_at <= current_tick` into the `pending` buffer of the main queue.
/// 3. Called automatically from `World::tick()` before `update()`.
pub struct DelayedQueue<T> {
    /// Min-heap by (deliver_at, sequence) — ready events at the top.
    heap: BinaryHeap<DelayedEvent<T>>,
    /// Monotonic counter for stable ordering of events with the same deliver_at.
    sequence: u64,
}

/// Internal deferred-event record with BinaryHeap support.
struct DelayedEvent<T> {
    /// Delivery tick (wrapped in Reverse for the min-heap).
    deliver_at: Reverse<u32>,
    /// Sequence number for stable FIFO ordering of events with the same deliver_at.
    sequence: Reverse<u64>,
    event: T,
}

// Manual PartialOrd/Ord implementation, comparing only by (deliver_at, sequence).
// T is not required to implement Ord.
impl<T> PartialEq for DelayedEvent<T> {
    fn eq(&self, other: &Self) -> bool {
        self.deliver_at == other.deliver_at && self.sequence == other.sequence
    }
}
impl<T> Eq for DelayedEvent<T> {}

impl<T> PartialOrd for DelayedEvent<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for DelayedEvent<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap. Reverse<u32> gives min by deliver_at.
        // On equal deliver_at, Reverse<u64> gives FIFO (smaller sequence = older = higher).
        self.deliver_at
            .cmp(&other.deliver_at)
            .then(self.sequence.cmp(&other.sequence))
    }
}

impl<T> DelayedQueue<T> {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            sequence: 0,
        }
    }

    /// Send an event with a delay in ticks.
    ///
    /// `delay` — the number of ticks after which the event becomes available.
    /// `current_tick` — the world's current tick (used to compute `deliver_at`).
    ///
    /// Complexity: O(log N).
    pub fn send_delayed(&mut self, event: T, delay: u32, current_tick: u32) {
        let deliver_at = current_tick.wrapping_add(delay);
        let seq = self.sequence;
        self.sequence += 1;
        self.heap.push(DelayedEvent {
            deliver_at: Reverse(deliver_at),
            sequence: Reverse(seq),
            event,
        });
    }

    /// Move all events ready for delivery into `target_queue`.
    ///
    /// Extracts events with `deliver_at <= current_tick` from the min-heap.
    /// Stops as soon as the heap top is not ready — O(K log N),
    /// where K is the number of ready events.
    ///
    /// Events with the same `deliver_at` are delivered in insertion order (FIFO).
    pub fn flush_delayed(&mut self, current_tick: u32, target_queue: &mut Events<T>) {
        // BinaryHeap is a max-heap with Reverse<u32>, so the top is the min deliver_at.
        // Extract while the top is "ready" (deliver_at <= current_tick).
        while let Some(top) = self.heap.peek() {
            if top.deliver_at.0 <= current_tick {
                // SAFETY: we just checked peek, so pop is always Some
                let ev = self.heap.pop().unwrap();
                target_queue.send(ev.event);
            } else {
                // The top is not ready yet — the rest even less so
                break;
            }
        }
    }

    /// Number of deferred events (not yet delivered).
    #[inline]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub fn clear(&mut self) {
        self.heap.clear();
        self.sequence = 0;
    }

    /// Reserve capacity for future events.
    pub fn reserve(&mut self, additional: usize) {
        self.heap.reserve(additional);
    }
}

impl<T> Default for DelayedQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ── AnyEventQueue (trait object) ─────────────────────────────────

/// Trait object for storing queues of different types in the EventRegistry.
pub trait AnyEventQueue: Any + Send + Sync {
    fn update(&mut self);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Raw mutable pointer for EventWriter in SystemContext.
    fn as_ptr_mut(&mut self) -> *mut u8;
    /// Register a reader — returns an EventCursor (as u32).
    fn add_reader(&mut self) -> u32;
    /// Remove a reader.
    fn remove_reader(&mut self, reader_id: u32);
    /// Pre-allocate capacity for the pending buffer.
    fn reserve(&mut self, additional: usize);
}

impl<T: Send + Sync + 'static> AnyEventQueue for Events<T> {
    fn update(&mut self) {
        Events::update(self);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn len(&self) -> usize {
        Events::len(self)
    }

    fn as_ptr_mut(&mut self) -> *mut u8 {
        self as *mut Events<T> as *mut u8
    }

    fn add_reader(&mut self) -> u32 {
        Events::add_reader(self).0
    }

    fn remove_reader(&mut self, reader_id: u32) {
        Events::remove_reader(self, EventCursor(reader_id));
    }

    fn reserve(&mut self, additional: usize) {
        Events::reserve(self, additional);
    }
}

// ── Removed<T> — component-removal event (W3-1) ────────────

/// The event "entity had its component `T` removed" — analogous to Bevy `RemovedComponents`.
///
/// Emitted by the core for types enabled via
/// [`World::track_removals::<T>()`](crate::World::track_removals), on ALL
/// component-loss paths: `remove`/`remove_raw` and `despawn` (in the latter case
/// the entity is already dead). Read like a regular event — `&[Removed<T>]` in
/// `system!`, `world.event_reader::<Removed<T>>()` — with per-reader cursors
/// (no duplicates and no gaps) and the usual flush discipline (per-stage in the
/// scheduler / `advance_frame()` without it).
pub struct Removed<T: crate::component::Component> {
    pub entity: crate::entity::Entity,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T: crate::component::Component> Removed<T> {
    #[inline]
    pub(crate) fn new(entity: crate::entity::Entity) -> Self {
        Self {
            entity,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T: crate::component::Component> Clone for Removed<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: crate::component::Component> Copy for Removed<T> {}

impl<T: crate::component::Component> std::fmt::Debug for Removed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Removed<{}>({})",
            std::any::type_name::<T>(),
            self.entity
        )
    }
}

// ── EventRegistry ───────────────────────────────────────────────

/// A queue slot behind `UnsafeCell` so `get_raw_ptr` can hand out a
/// `*mut Events<T>` whose provenance is the cell interior — writing through it
/// (the `EventWriter` push path and `EventReader` cursor registration) is legal
/// — instead of laundering a `*mut` out of a shared `downcast_ref`, which is
/// undefined behavior (Tree Borrows: write through a pointer derived from a
/// frozen shared reference). Same class as the resources fix (A3). The scheduler
/// serializes mutable access to any one event queue via `AccessDescriptor`, so
/// the interior mutation never actually races.
#[repr(transparent)]
struct EventQueueCell(UnsafeCell<Box<dyn AnyEventQueue>>);

// SAFETY: the inner queue is `Send + Sync`; concurrent access is serialized by
// the scheduler's AccessDescriptor discipline, so exposing `Sync` is sound.
unsafe impl Sync for EventQueueCell {}

/// Event-queue registry — a `TypeId → Events<T>` map. Internal: the golden path
/// reaches queues through `World::send_event` / `EventWriter` / `EventReader`.
pub(crate) struct EventRegistry {
    queues: FxHashMap<TypeId, EventQueueCell>,
}

impl EventRegistry {
    pub fn new() -> Self {
        Self {
            queues: FxHashMap::default(),
        }
    }

    /// Register an event type.
    pub fn register<T: Send + Sync + 'static>(&mut self) {
        self.queues
            .entry(TypeId::of::<T>())
            .or_insert_with(|| EventQueueCell(UnsafeCell::new(Box::new(Events::<T>::new()))));
    }

    /// Get the event queue by type (panics if not registered).
    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &Events<T> {
        self.queues
            .get(&TypeId::of::<T>())
            // SAFETY: shared read of the cell interior; the scheduler guarantees
            // no concurrent mutable access to this queue.
            .and_then(|c| unsafe { (*c.0.get()).as_any().downcast_ref::<Events<T>>() })
            .unwrap_or_else(|| {
                panic!(
                    "Event `{}` not registered. Call world.add_event::<{0}>()",
                    std::any::type_name::<T>()
                )
            })
    }

    /// Mutable access to the queue (panics if not registered).
    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut Events<T> {
        self.queues
            .get_mut(&TypeId::of::<T>())
            // `&mut self` — exclusive; take the cell interior without unsafe.
            .and_then(|c| c.0.get_mut().as_any_mut().downcast_mut::<Events<T>>())
            .unwrap_or_else(|| {
                panic!(
                    "Event `{}` not registered. Call world.add_event::<{0}>()",
                    std::any::type_name::<T>()
                )
            })
    }

    /// Get mutable access to the queue, automatically registering the type.
    pub fn get_or_register_mut<T: Send + Sync + 'static>(&mut self) -> &mut Events<T> {
        if !self.queues.contains_key(&TypeId::of::<T>()) {
            self.register::<T>();
        }
        self.queues
            .get_mut(&TypeId::of::<T>())
            .and_then(|c| c.0.get_mut().as_any_mut().downcast_mut::<Events<T>>())
            .unwrap()
    }

    /// Raw pointer for EventWriter.
    ///
    /// # Safety
    /// The caller guarantees unique access. The heap data (Box)
    /// is not moved on HashMap reallocation.
    pub fn get_raw_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut Events<T>> {
        let cell = self.queues.get(&TypeId::of::<T>())?;
        // SAFETY: the `*mut` provenance is the `UnsafeCell` interior (via
        // `get()`), so writing through it is legal — unlike laundering a `*mut`
        // out of a shared `downcast_ref` (UB). The scheduler guarantees exclusive
        // access to this queue while the pointer is live.
        let erased: &mut Box<dyn AnyEventQueue> = unsafe { &mut *cell.0.get() };
        let events = erased.as_any_mut().downcast_mut::<Events<T>>()?;
        Some(events as *mut Events<T>)
    }

    /// Pre-allocate capacity for the event queue identified by TypeId.
    pub fn reserve_by_type(&mut self, type_id: TypeId, capacity: usize) {
        if let Some(cell) = self.queues.get_mut(&type_id) {
            cell.0.get_mut().reserve(capacity);
        }
    }

    /// Update all queues (called at the end of the tick).
    pub fn update_all(&mut self) {
        for cell in self.queues.values_mut() {
            cell.0.get_mut().update();
        }
    }

    /// Flush specific event types (by TypeId).
    /// Used by the Scheduler for per-Stage flush.
    pub fn flush_by_type_id(&mut self, type_ids: &[TypeId]) {
        for tid in type_ids {
            if let Some(cell) = self.queues.get_mut(tid) {
                cell.0.get_mut().update();
            }
        }
    }

    /// Flush all event queues.
    /// Used when operating without a Scheduler.
    pub fn flush_all(&mut self) {
        self.update_all();
    }

}

impl Default for EventRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_and_read() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(42);
        queue.send(43);
        queue.update();

        let events = queue.iter(&reader);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], 42);
        assert_eq!(events[1], 43);

        // Advance the cursor
        queue.advance_reader_mut(&reader);
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 0, "after advance the cursor should be at the end");
    }

    // ── EventIterator (engine TD-24): for e in read() ──────────

    #[test]
    fn read_guard_into_iterator_yields_and_advances() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(1);
        queue.send(2);
        queue.send(3);
        queue.update();

        // The main Bevy idiom: direct iteration over read() without .iter().
        let mut got = Vec::new();
        for e in queue.read(&reader) {
            got.push(*e);
        }
        assert_eq!(got, vec![1, 2, 3]);

        // Dropping the iterator advanced the cursor to the end of the buffer.
        assert_eq!(queue.iter(&reader).len(), 0);
    }

    #[test]
    fn event_iterator_break_advances_to_end() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(10);
        queue.send(20);
        queue.update();

        for e in queue.read(&reader) {
            if *e == 10 {
                break; // partial iteration
            }
        }
        // The guard's semantics are preserved: break = skip the remainder.
        assert_eq!(
            queue.iter(&reader).len(),
            0,
            "dropping the iterator (including on break) advances the cursor to the end"
        );
    }

    /// F5 witness: the `&i32` outlive the `EventIterator`, whose `Drop` reborrows
    /// `&mut Events` to advance the cursor. Under `cargo miri test
    /// -Zmiri-tree-borrows` this proves the reborrow does not invalidate the
    /// still-live event references (cursor mutation is disjoint from the `events`
    /// buffer). See the INVARIANT notes on `EventIterator::Drop` /
    /// `advance_reader_mut`.
    #[test]
    fn event_iterator_collect_holds_references() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(7);
        queue.send(8);
        queue.update();

        // The references outlive the iterator itself (lifetime of the original Events borrow).
        let collected: Vec<&i32> = queue.read(&reader).into_iter().collect();
        // Read the still-live references AFTER the iterator has dropped (its
        // Drop ran when `into_iter().collect()` consumed it).
        assert_eq!(collected, vec![&7, &8]);
        drop(collected);
        assert_eq!(queue.iter(&reader).len(), 0);
    }

    #[test]
    fn event_iterator_len_and_guard_slice_api() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(5);
        queue.send(6);
        queue.update();

        {
            // len()/is_empty() are available on the guard via Deref<[T]>.
            let guard = queue.read(&reader);
            assert_eq!(guard.len(), 2);
            assert!(!guard.is_empty());
            // Iterating by reference does not consume the guard.
            let doubled: Vec<i32> = (&guard).into_iter().map(|e| e * 2).collect();
            assert_eq!(doubled, vec![10, 12]);
            // ExactSizeIterator on the owning iterator.
            let it = guard.into_iter();
            assert_eq!(it.len(), 2);
        }
        assert_eq!(queue.iter(&reader).len(), 0);
    }

    #[test]
    fn two_readers_independent() {
        let mut queue = Events::new();
        let reader_a = queue.add_reader();
        let reader_b = queue.add_reader();

        queue.send(1);
        queue.send(2);
        queue.update();

        // Reader A reads one event
        {
            let events = queue.iter(&reader_a);
            assert_eq!(events.len(), 2);
        }
        queue.advance_reader_mut(&reader_a);

        // Reader B read nothing — it can still read both
        {
            let events = queue.iter(&reader_b);
            assert_eq!(events.len(), 2);
        }
        queue.advance_reader_mut(&reader_b);

        // Both have read — the next update may clear
        queue.update();
        queue.send(3);
        queue.update();

        // New events
        let events = queue.iter(&reader_a);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], 3);
    }

    /// F1: when a reader lags across an `update`, neither the old (unread) events
    /// nor the new ones may be lost — for that reader or any other.
    #[test]
    fn lagging_reader_receives_old_and_new_events_none_lost() {
        let mut queue = Events::new();
        let reader_a = queue.add_reader(); // will lag
        let reader_b = queue.add_reader(); // stays caught up

        // Frame 1: 1, 2.
        queue.send(1);
        queue.send(2);
        queue.update();

        // B reads (catches up); A intentionally does not read.
        assert_eq!(queue.iter(&reader_b), &[1, 2]);
        queue.advance_reader_mut(&reader_b);

        // Frame 2: 3, 4, while A still lags.
        queue.send(3);
        queue.send(4);
        queue.update();

        // A must see everything it never read plus the new events.
        assert_eq!(
            queue.iter(&reader_a),
            &[1, 2, 3, 4],
            "lagging reader lost events across update"
        );
        queue.advance_reader_mut(&reader_a);

        // B was caught up; it must still receive the new events (the old code
        // buried them behind every cursor and dropped them).
        assert_eq!(
            queue.iter(&reader_b),
            &[3, 4],
            "caught-up reader missed events produced while another reader lagged"
        );
        queue.advance_reader_mut(&reader_b);

        // Both are now caught up — the next update drains cleanly.
        queue.update();
        assert_eq!(queue.iter(&reader_a), &[] as &[i32]);
        assert_eq!(queue.iter(&reader_b), &[] as &[i32]);
    }

    #[test]
    fn reader_removed_still_works() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(10);
        queue.update();
        queue.remove_reader(reader);

        // After removing a reader, the queue must not panic
        // and must work normally for other readers
        let _reader2 = queue.add_reader();
        queue.send(20);
        queue.update();
    }

    #[test]
    fn entity_event_send_and_read() {
        let mut queue = Events::<EntityEvent<i32>>::new();
        let reader = queue.add_reader();

        let entity = Entity {
            index: 42,
            generation: 1,
        };
        queue.send(EntityEvent::new(entity, 100));
        queue.update();

        let events = queue.iter(&reader);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].target, entity);
        assert_eq!(events[0].data, 100);
    }

    // ── DelayedQueue tests ─────────────────────────────────────

    #[test]
    fn delayed_event_delivery() {
        let mut queue = Events::new();
        let reader = queue.add_reader();
        let mut delayed = DelayedQueue::new();

        // Send an event with a 3-tick delay
        delayed.send_delayed(99, 3, 0);
        assert_eq!(delayed.len(), 1);

        // Tick 1: nothing should be delivered
        delayed.flush_delayed(1, &mut queue);
        assert_eq!(queue.len_pending(), 0);

        // Tick 2: nothing
        delayed.flush_delayed(2, &mut queue);
        assert_eq!(queue.len_pending(), 0);

        // Tick 3: should be delivered
        delayed.flush_delayed(3, &mut queue);
        assert_eq!(queue.len_pending(), 1);
        assert!(delayed.is_empty());

        // After update() the event is available for reading
        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], 99);
    }

    #[test]
    fn delayed_event_fifo_same_tick() {
        // Events with the same deliver_at must be delivered in insertion order (FIFO)
        let mut queue = Events::new();
        let reader = queue.add_reader();
        let mut delayed = DelayedQueue::new();

        delayed.send_delayed(10, 1, 0);
        delayed.send_delayed(20, 1, 0);
        delayed.send_delayed(30, 1, 0);

        delayed.flush_delayed(1, &mut queue);
        assert_eq!(queue.len_pending(), 3);
        assert!(delayed.is_empty());

        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 3);
        // FIFO: the insertion order must be preserved
        assert_eq!(events[0], 10);
        assert_eq!(events[1], 20);
        assert_eq!(events[2], 30);
    }

    #[test]
    fn delayed_event_varying_delays() {
        let mut queue = Events::new();
        let reader = queue.add_reader();
        let mut delayed = DelayedQueue::new();

        delayed.send_delayed(10, 1, 0);
        delayed.send_delayed(20, 2, 0);
        delayed.send_delayed(30, 1, 0);

        // Tick 1: two events with delay 1
        delayed.flush_delayed(1, &mut queue);
        assert_eq!(queue.len_pending(), 2);

        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], 10);
        assert_eq!(events[1], 30);
        queue.advance_reader_mut(&reader);

        // Tick 2: one event with delay 2
        delayed.flush_delayed(2, &mut queue);
        assert_eq!(queue.len_pending(), 1);
        assert!(delayed.is_empty());

        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], 20);
    }

    #[test]
    fn delayed_heap_early_stop() {
        // flush_delayed must stop as soon as the heap top is "not ready"
        let mut queue = Events::new();
        let mut delayed = DelayedQueue::new();

        delayed.send_delayed(1, 10, 0); // deliver_at=10
        delayed.send_delayed(2, 10, 0); // deliver_at=10
        delayed.send_delayed(3, 20, 0); // deliver_at=20

        // flush at tick 10 — events with deliver_at <= 10 should be delivered
        delayed.flush_delayed(10, &mut queue);
        assert_eq!(queue.len_pending(), 2);
        assert_eq!(delayed.len(), 1, "the event with deliver_at=20 should remain");

        // flush at tick 15 — nothing new
        delayed.flush_delayed(15, &mut queue);
        assert_eq!(delayed.len(), 1);

        // flush at tick 20 — the last event
        delayed.flush_delayed(20, &mut queue);
        assert_eq!(delayed.len(), 0);
    }

    // ── read_partial tests ─────────────────────────────────────

    #[test]
    fn read_partial_advances_by_count() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        for i in 0..10 {
            queue.send(i);
        }
        queue.update();

        // Read the first 3 events
        {
            let guard = queue.read_partial(&reader, 3);
            assert_eq!(guard.len(), 3);
            assert_eq!(guard.as_slice(), &[0, 1, 2]);
        } // Drop: the cursor advances by 3

        // Read the next 3
        {
            let guard = queue.read_partial(&reader, 3);
            assert_eq!(guard.len(), 3);
            assert_eq!(guard.as_slice(), &[3, 4, 5]);
        }

        // Read more than remains
        {
            let guard = queue.read_partial(&reader, 100);
            assert_eq!(guard.len(), 4); // 6,7,8,9 remain
        }

        // Nothing remains
        {
            let guard = queue.read_partial(&reader, 10);
            assert_eq!(guard.len(), 0);
        }
    }

    #[test]
    fn read_advances_to_end() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(1);
        queue.send(2);
        queue.send(3);
        queue.update();

        // read() must advance to the end on drop
        {
            let _guard = queue.read(&reader);
            // read only the first one
            // _guard.iter().next(); — does not matter, on drop it goes to the end anyway
        }

        // The cursor must be at the end — there are no new events
        let remaining = queue.iter(&reader);
        assert_eq!(
            remaining.len(),
            0,
            "read() must advance to the end of the buffer"
        );
    }

    // ── send_sync tests ────────────────────────────────────────

    #[test]
    fn send_sync_visible_after_flush() {
        let mut queue = Events::<i32>::new();
        let reader = queue.add_reader();

        // send_sync via a shared reference
        queue.send_sync(42);
        queue.send_sync(43);

        // Before flush_sync — not yet in pending
        assert_eq!(queue.len_pending(), 0);

        // flush_sync moves from sync_pending into pending
        queue.flush_sync();
        assert_eq!(queue.len_pending(), 2);

        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], 42);
        assert_eq!(events[1], 43);
    }

    #[test]
    fn update_auto_flushes_sync() {
        let mut queue = Events::<i32>::new();
        let reader = queue.add_reader();

        queue.send_sync(100);
        queue.send(200); // into the main pending

        // update() must merge sync_pending + pending and perform the swap
        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn clear_resets_everything() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(1);
        queue.send(2);
        queue.update();
        assert_eq!(queue.len(), 2);

        queue.clear();
        assert_eq!(queue.len(), 0);

        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn multiple_updates_cycle() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        // Tick 1
        queue.send(1);
        queue.update();
        assert_eq!(queue.iter(&reader).len(), 1);
        queue.advance_reader_mut(&reader);

        // Tick 2
        queue.send(2);
        queue.update();
        assert_eq!(queue.iter(&reader).len(), 1);
        assert_eq!(queue.iter(&reader)[0], 2);
    }

    #[test]
    fn event_auto_register_via_send() {
        use crate::world::World;

        let mut world = World::new();
        // We do not call add_event::<String>()
        world.send_event("auto-registered".to_string());

        world.tick();
        world.flush_all_events();
        let queue = world.events::<String>();
        assert_eq!(
            queue.len_readable(),
            1,
            "The event should be available after tick()"
        );
    }

    /// C7: `advance_frame()` is self-contained — it flushes events (they become
    /// readable on the next frame) AND advances the change-tick. Without a manual
    /// tick+flush pair.
    #[test]
    fn advance_frame_flushes_events_and_ticks() {
        use crate::world::World;

        let mut world = World::new();
        let t0 = world.current_tick();

        world.send_event(7u32);
        // Until the end of the frame the event is not yet in the "readable" buffer.
        assert_eq!(world.events::<u32>().len_readable(), 0);

        world.advance_frame(); // flush + tick

        assert_eq!(
            world.events::<u32>().len_readable(),
            1,
            "advance_frame must make the event readable"
        );
        assert_ne!(world.current_tick(), t0, "advance_frame must advance the change-tick");
    }
}
