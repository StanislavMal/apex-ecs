//! Система событий с per-reader курсорами и автоматической очисткой.
//!
//! # Концепция
//!
//! - [`Events<T>`] — основной тип. Содержит два буфера:
//!   `pending` (куда пишут в текущем тике) и `events` (доступно для чтения).
//! - [`EventCursor`] — лёгкий дескриптор читателя. Каждый читатель
//!   регистрирует свой курсор и двигает его по мере чтения.
//! - После вызова [`update()`](Events::update) буферы меняются местами:
//!   `pending` → `events`, а старые события из `events`
//!   удаляются (если все читатели их прочитали).
//! - `update()` вызывается Scheduler'ом после каждого Stage
//!   или вручную через [`World::flush_all_events`].
//! - [`EntityEvent<T>`] — событие, адресованное конкретной сущности.
//! - [`DelayedQueue<T>`] — отложенная доставка событий через N тиков.
//!   Реализована через `BinaryHeap` для O(log N) вставки и O(K log N) flush.
//!
//! # Семантика `EventReadGuard`
//!
//! `EventReadGuard` — RAII-обёртка. При **дропе** автоматически продвигает
//! курсор читателя до конца буфера, независимо от того, сколько событий
//! было реально прочитано через `iter()` или `as_slice()`.
//!
//! Это означает: если вы вызвали `guard.iter()` и прочитали только часть
//! событий, остальные будут пропущены при следующем чтении. Чтобы прочитать
//! только часть событий и сохранить курсор, используйте [`Events::read_partial`].
//!
//! Для "посмотреть без продвижения" используйте [`EventReadGuard::peek`].
//!
//! # Thread-safety
//!
//! `Events<T>` не является `Sync`. Для отправки событий из параллельных
//! систем используйте [`Events::send_sync`], который защищён внутренним
//! `Mutex<Vec<T>>`. Содержимое `sync_pending` сливается в `pending`
//! при вызове [`Events::update`] или [`Events::flush_sync`].

use rustc_hash::FxHashMap;
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Mutex, OnceLock};

use crate::entity::Entity;

// ── Events ──────────────────────────────────────────────────────

/// Очередь событий с per-reader отслеживанием прогресса чтения.
///
/// # Как это работает
///
/// 1. Системы-отправители пишут события в `pending` (через `send()`).
/// 2. В конце тика `update()` меняет местами `pending` и `events`.
/// 3. Системы-читатели вызывают `reader.iter(queue)` — получают только
///    непрочитанные события из буфера `events`.
/// 4. После прочтения курсор читателя устанавливается на `events.len()`.
/// 5. Garbage collection: если все активные читатели прочитали все события,
///    буфер `events` очищается.
///
/// # Thread-safety и `send_sync`
///
/// Базовые методы `send()` / `send_batch()` требуют `&mut self` и не `Sync`.
/// Для параллельных систем — `send_sync(&self, event)` пишет в `sync_pending`
/// (внутренний `Mutex<Vec<T>>`). Содержимое сливается в `pending` при
/// [`update()`] или явном [`flush_sync()`].
pub struct Events<T> {
    /// Буфер, доступный для чтения (предыдущий тик).
    events: Vec<T>,
    /// Буфер, куда пишутся новые события (текущий тик).
    pending: Vec<T>,
    /// Thread-safe буфер для параллельных систем (4.4).
    /// Ленивая инициализация через `OnceLock` — нулевой overhead
    /// для однопоточных пользователей.
    sync_pending: OnceLock<Mutex<Vec<T>>>,
    /// Состояние читателей: `None` = читатель удалён, `Some(pos)` = текущая позиция.
    cursors: Vec<Option<u32>>,
    /// Счётчик для генерации ID новых читателей.
    next_cursor_id: u32,
    /// Список освобождённых EventCursor ID для O(1) переиспользования.
    free_list: Vec<EventCursor>,
    /// Число активных читателей, у которых позиция курсора < events.len().
    /// Инвариант: lagging_count == cursors.iter().flatten().filter(|&&p| p < events.len()).count()
    /// Используется для O(1) проверки в `all_readers_caught_up()`.
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

    /// Отправить событие в текущий тик.
    #[inline]
    pub fn send(&mut self, event: T) {
        self.pending.push(event);
    }

    /// Отправить пачку событий.
    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        self.pending.extend(events);
    }

    /// Отправить событие из параллельного контекста (thread-safe).
    ///
    /// Пишет в внутренний `Mutex<Vec<T>>`. Содержимое сливается в основной
    /// `pending`-буфер при следующем вызове [`update()`] или [`flush_sync()`].
    ///
    /// # Пример
    ///
    /// ```ignore
    /// // В параллельной системе:
    /// events.send_sync(DamageEvent { amount: 10 });
    /// ```
    ///
    /// # Примечание по производительности
    ///
    /// Каждый вызов берёт `Mutex::lock`. При массовой отправке предпочтите
    /// собирать события в локальный `Vec`, а затем вызывать `send_batch_sync`.
    pub fn send_sync(&self, event: T) {
        let sync = self.get_or_init_sync();
        sync.lock().unwrap().push(event);
    }

    /// Отправить пачку событий из параллельного контекста (thread-safe).
    ///
    /// Берёт lock один раз для всего пакета — эффективнее многократных `send_sync`.
    pub fn send_batch_sync(&self, events: impl IntoIterator<Item = T>) {
        let sync = self.get_or_init_sync();
        sync.lock().unwrap().extend(events);
    }

    /// Слить `sync_pending` в основной `pending`.
    ///
    /// Вызывается автоматически из [`update()`]. Можно вызвать вручную
    /// если нужно сделать события доступными до следующего тика.
    pub fn flush_sync(&mut self) {
        if let Some(sync) = self.sync_pending.get() {
            let mut guard = sync.lock().unwrap();
            if !guard.is_empty() {
                self.pending.append(&mut *guard);
            }
        }
    }

    /// Ленивая инициализация `sync_pending` — нулевой overhead для
    /// однопоточных пользователей, которые никогда не вызывают `send_sync`.
    fn get_or_init_sync(&self) -> &Mutex<Vec<T>> {
        self.sync_pending.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Зарегистрировать нового читателя.
    ///
    /// Возвращает [`EventCursor`], который нужно хранить и передавать
    /// при каждом вызове [`iter()`](Events::iter).
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

    /// Удалить читателя.
    ///
    /// После удаления курсор перестаёт учитываться при GC,
    /// что может позволить очистить буфер `events`.
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

    /// Количество активных читателей.
    pub fn reader_count(&self) -> usize {
        self.cursors.iter().filter(|c| c.is_some()).count()
    }

    /// Предварительно выделить capacity для pending-буфера.
    ///
    /// Позволяет избежать многократных реаллокаций при массовой отправке событий.
    /// Вызывать из системы перед `event_writer::send()` в цикле.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.pending.reserve(additional);
    }

    /// Обновить буферы: переместить `pending` в `events`.
    ///
    /// Вызывается Scheduler'ом после каждого Stage или вручную через [`World::flush_all_events`].
    /// Если все читатели прочитали предыдущие события, буфер `events`
    /// очищается перед загрузкой нового. Если не все читатели догнали —
    /// старые события дописываются в конец нового буфера, чтобы
    /// отстающие читатели могли их догнать.
    ///
    /// Также сливает `sync_pending` (из `send_sync`) в `pending`.
    pub fn update(&mut self) {
        // Сливаем sync_pending в pending перед swap
        self.flush_sync();

        // Ранний выход: если оба буфера пусты — нечего свопать
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

        // Пересчитываем lagging_count для нового состояния событий
        let new_event_len = self.events.len() as u32;
        self.lagging_count = self
            .cursors
            .iter()
            .flatten()
            .filter(|&&pos| pos < new_event_len)
            .count() as u32;

        self.assert_lagging_invariant();
    }

    /// Итерация по непрочитанным событиям из буфера `events`.
    ///
    /// Возвращает срез — не продвигает курсор. Для продвижения вызовите
    /// [`advance_reader_mut`] или используйте [`read`] (с авто-advance при дропе).
    #[inline]
    pub fn iter(&self, reader_id: &EventCursor) -> &[T] {
        let idx = reader_id.0 as usize;
        let cursor = self.cursors.get(idx).and_then(|c| c.as_ref());
        match cursor {
            Some(&pos) if (pos as usize) < self.events.len() => &self.events[pos as usize..],
            _ => &[],
        }
    }

    /// Мутабельная версия продвижения курсора до конца буфера.
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

    /// Продвинуть курсор на заданное число событий.
    ///
    /// Используется внутри [`EventReadGuard`] при `read_partial()`.
    /// Курсор не выйдет за пределы буфера.
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

    /// Прочитать непрочитанные события с автоматическим продвижением курсора
    /// **до конца буфера** при дропе guard.
    ///
    /// # Семантика
    ///
    /// При дропе [`EventReadGuard`] курсор **всегда** прыгает в конец буфера,
    /// независимо от того, сколько событий реально было прочитано через
    /// `iter()` / `as_slice()`. Это означает: частичное чтение приводит
    /// к пропуску оставшихся событий.
    ///
    /// Если вы хотите прочитать ровно N событий и оставить остальные —
    /// используйте [`read_partial`](Events::read_partial).
    ///
    /// Если вы хотите посмотреть события без продвижения курсора —
    /// используйте [`EventReadGuard::peek`].
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

    /// Прочитать не более `max_count` событий с продвижением курсора
    /// ровно на количество реально возвращённых событий.
    ///
    /// # Отличие от `read()`
    ///
    /// `read()` при дропе продвигает курсор до конца буфера.
    /// `read_partial(n)` при дропе продвигает курсор ровно на `n` событий
    /// (или меньше, если в буфере меньше непрочитанных).
    ///
    /// Используйте когда нужно обработать события пакетами:
    ///
    /// ```ignore
    /// // Обрабатываем по 32 события за тик, не теряя остальные
    /// while let guard = events.read_partial(&cursor, 32) {
    ///     if guard.is_empty() { break; }
    ///     for ev in guard.iter() { process(ev); }
    ///     // При дропе — курсор продвинется ровно на guard.len()
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

    /// Количество событий в обоих буферах.
    #[inline]
    pub fn len(&self) -> usize {
        self.events.len() + self.pending.len()
    }

    /// Количество событий в буфере чтения (доступных для текущего тика).
    #[inline]
    pub fn len_readable(&self) -> usize {
        self.events.len()
    }

    /// Количество событий в буфере записи.
    #[inline]
    pub fn len_pending(&self) -> usize {
        self.pending.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.pending.is_empty()
    }

    /// Очистить оба буфера и сбросить все курсоры.
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

    /// Debug-assertion: проверяет инвариант lagging_count.
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

/// RAII-обёртка: при дропе автоматически продвигает курсор **до конца** буфера.
///
/// Создаётся через [`Events::read`].
///
/// # Семантика дропа
///
/// При дропе курсор читателя устанавливается в конец буфера `events`,
/// независимо от того, сколько событий было реально прочитано через
/// `iter()` / `as_slice()`. Если это нежелательно:
///
/// - Используйте [`EventReadGuard::peek`] чтобы посмотреть без продвижения.
/// - Используйте [`Events::read_partial`] чтобы продвинуть ровно на N.
#[must_use = "EventReadGuard advances cursor to end on drop; \
              bind to variable to read events, \
              or use Events::read_partial() to advance by count"]
pub struct EventReadGuard<'q, T> {
    queue: &'q mut Events<T>,
    reader_id: EventCursor,
    start: usize,
}

impl<'q, T> EventReadGuard<'q, T> {
    /// Срез непрочитанных событий.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.queue.events[self.start..]
    }

    /// Итерация без потребления guard.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// Отказаться от автоматического продвижения курсора.
    /// Курсор останется на текущей позиции после дропа guard.
    pub fn peek(self) -> PeekGuard<'q, T> {
        let start = self.start;
        let queue_ptr: *const Events<T> = &*self.queue;
        // Предотвращаем вызов EventReadGuard::drop (который продвинул бы курсор)
        std::mem::forget(self);
        PeekGuard {
            // SAFETY: queue_ptr получен из &'q mut Events<T>, валиден для 'q.
            // self забыт (drop не вызван), но Events<T> живёт дольше.
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
        // Продвигаем курсор до конца буфера.
        // Семантика: дроп guard = "я прочитал всё до конца буфера".
        // Для частичного чтения используйте Events::read_partial().
        self.queue.advance_reader_mut(&self.reader_id);
    }
}

/// `for e in reader.read() { ... }` — главная событийная идиома Bevy (TD-24
/// движка). Guard конвертируется во владеющий [`EventIterator`]; advance
/// курсора до конца буфера происходит при дропе итератора (т.е. и при `break`
/// — частичная итерация пропускает остаток, как и у самого guard'а).
impl<'q, T> IntoIterator for EventReadGuard<'q, T> {
    type Item = &'q T;
    type IntoIter = EventIterator<'q, T>;

    fn into_iter(self) -> EventIterator<'q, T> {
        let reader_id = self.reader_id;
        let start = self.start;
        let queue: *mut Events<T> = &mut *self.queue;
        // Предотвращаем вызов EventReadGuard::drop — advance переезжает
        // в Drop итератора.
        std::mem::forget(self);
        // SAFETY: queue получен из &'q mut Events<T> guard'а (guard забыт,
        // его Drop не вызовется — мы единственный владелец заёма). Буфер
        // `events` не мутируется, пока жив итератор: Drop итератора лишь
        // продвигает курсор (поля cursors/lagging_count), сам буфер не
        // трогает — тот же приём, что в `EventReadGuard::peek`.
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

/// Итерация по ссылке — без потребления guard'а (`for e in &guard`).
impl<'a, T> IntoIterator for &'a EventReadGuard<'_, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> std::slice::Iter<'a, T> {
        self.iter()
    }
}

// ── EventIterator ───────────────────────────────────────────────

/// Владеющий итератор по непрочитанным событиям (Bevy `EventIterator`-стиль):
/// отдаёт `&T` с лайфтаймом исходного заёма `Events`, при дропе продвигает
/// курсор читателя **до конца буфера** (семантика [`EventReadGuard`]
/// сохранена: `break` посреди цикла пропускает оставшиеся события).
///
/// Создаётся через `IntoIterator` у [`EventReadGuard`]:
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

/// RAII-обёртка: при дропе продвигает курсор ровно на `count` событий.
///
/// Создаётся через [`Events::read_partial`].
///
/// Позволяет читать события пакетами, не теряя непрочитанные:
///
/// ```ignore
/// // Обрабатываем не более 64 событий за тик
/// let guard = events.read_partial(&cursor, 64);
/// for ev in guard.iter() { process(ev); }
/// // При дропе курсор продвинется только на guard.len() (≤64)
/// ```
#[must_use = "PartialReadGuard advances cursor by count on drop; bind to variable to read events"]
pub struct PartialReadGuard<'q, T> {
    queue: &'q mut Events<T>,
    reader_id: EventCursor,
    start: usize,
    count: usize,
}

impl<'q, T> PartialReadGuard<'q, T> {
    /// Срез событий (не более `max_count`, переданного в `read_partial`).
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.queue.events[self.start..self.start + self.count]
    }

    /// Итерация без потребления guard.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// Количество событий в этом guard.
    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Нет ли событий.
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
        // Продвигаем курсор ровно на count — только прочитанные события.
        self.queue.advance_reader_by(&self.reader_id, self.count);
    }
}

// ── PeekGuard ──────────────────────────────────────────────────

/// Обёртка для "посмотреть без продвижения".
///
/// Создаётся через [`EventReadGuard::peek`].
/// При дропе курсор НЕ продвигается — в отличие от [`EventReadGuard`],
/// который при дропе всегда ставит курсор в конец буфера.
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

/// Легковесный дескриптор читателя событий.
///
/// Создаётся через [`Events::add_reader`].
/// Хранится в [`EventReader`](crate::system_param::EventReader).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventCursor(pub u32);

// ── EntityEvent ─────────────────────────────────────────────────

/// Событие, адресованное конкретной сущности.
///
/// Позволяет отправить событие и прочитать его только тем системам,
/// которые запрашивают события для конкретной entity.
///
/// # Пример
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
    /// Целевая сущность.
    pub target: Entity,
    /// Данные события.
    pub data: T,
}

impl<T> EntityEvent<T> {
    pub fn new(target: Entity, data: T) -> Self {
        Self { target, data }
    }
}

// ── DelayedQueue ────────────────────────────────────────────────

/// Очередь отложенных событий с O(log N) вставкой.
///
/// # Алгоритм
///
/// Внутри — `BinaryHeap<(Reverse<deliver_at>, sequence, T)>`:
/// - `Reverse<deliver_at>` — min-heap по тику доставки.
/// - `sequence` — монотонный счётчик для стабильного порядка событий
///   с одинаковым `deliver_at` (FIFO внутри одного тика).
///
/// # Сложность
///
/// | Операция | Старая реализация | Новая реализация |
/// |----------|-------------------|------------------|
/// | `send_delayed` | O(1) (push) | O(log N) (heap push) |
/// | `flush_delayed` | O(N) (full scan) | O(K log N), K = кол-во готовых |
/// | Память | Vec | BinaryHeap (Vec внутри) |
///
/// При типичном использовании (редкие отложенные события, N < 1000)
/// разница незначительна. При тысячах отложенных событий — flush_delayed
/// ускоряется примерно в N/K раз.
///
/// # Как это работает
///
/// 1. `send_delayed(event, delay_ticks)` — событие сохраняется с меткой
///    `deliver_at = current_tick + delay_ticks`.
/// 2. Каждый вызов `flush_delayed(current_tick)` извлекает все события,
///    у которых `deliver_at <= current_tick`, в `pending`-буфер основной очереди.
/// 3. Вызывается автоматически из `World::tick()` перед `update()`.
pub struct DelayedQueue<T> {
    /// Min-heap по (deliver_at, sequence) — готовые события на вершине.
    heap: BinaryHeap<DelayedEvent<T>>,
    /// Монотонный счётчик для стабильного порядка событий с одинаковым deliver_at.
    sequence: u64,
}

/// Внутренняя запись отложенного события с поддержкой BinaryHeap.
struct DelayedEvent<T> {
    /// Тик доставки (обёрнут в Reverse для min-heap).
    deliver_at: Reverse<u32>,
    /// Порядковый номер для стабильного FIFO-порядка событий с одинаковым deliver_at.
    sequence: Reverse<u64>,
    event: T,
}

// Ручная реализация PartialOrd/Ord для сравнения только по (deliver_at, sequence).
// T не обязан реализовывать Ord.
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
        // BinaryHeap — max-heap. Reverse<u32> даёт min по deliver_at.
        // При равном deliver_at — Reverse<u64> даёт FIFO (меньший sequence = старее = выше).
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

    /// Отправить событие с задержкой в тиках.
    ///
    /// `delay` — количество тиков, через которые событие станет доступно.
    /// `current_tick` — текущий тик мира (для расчёта `deliver_at`).
    ///
    /// Сложность: O(log N).
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

    /// Переместить все события, готовые к доставке, в `target_queue`.
    ///
    /// Извлекает события с `deliver_at <= current_tick` из min-heap.
    /// Останавливается как только вершина heap не готова — O(K log N),
    /// где K — количество готовых событий.
    ///
    /// События с одинаковым `deliver_at` доставляются в порядке вставки (FIFO).
    pub fn flush_delayed(&mut self, current_tick: u32, target_queue: &mut Events<T>) {
        // BinaryHeap — max-heap с Reverse<u32>, поэтому вершина — min deliver_at.
        // Извлекаем пока вершина "готова" (deliver_at <= current_tick).
        while let Some(top) = self.heap.peek() {
            if top.deliver_at.0 <= current_tick {
                // SAFETY: мы только что проверили peak, pop всегда Some
                let ev = self.heap.pop().unwrap();
                target_queue.send(ev.event);
            } else {
                // Вершина ещё не готова — остальные тем более
                break;
            }
        }
    }

    /// Количество отложенных событий (ещё не доставленных).
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

    /// Зарезервировать capacity для будущих событий.
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

/// Trait-object для хранения очередей разных типов в EventRegistry.
pub trait AnyEventQueue: Any + Send + Sync {
    fn update(&mut self);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Raw mutable pointer для EventWriter в SystemContext.
    fn as_ptr_mut(&mut self) -> *mut u8;
    /// Зарегистрировать читателя — возвращает EventCursor (как u32).
    fn add_reader(&mut self) -> u32;
    /// Удалить читателя.
    fn remove_reader(&mut self, reader_id: u32);
    /// Предварительно выделить capacity для pending-буфера.
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

// ── Removed<T> — событие удаления компонента (W3-1) ────────────

/// Событие «у entity удалён компонент `T`» — аналог Bevy `RemovedComponents`.
///
/// Эмитится ядром для типов, включённых через
/// [`World::track_removals::<T>()`](crate::World::track_removals), на ВСЕХ
/// путях потери компонента: `remove`/`remove_raw` и `despawn` (в этом случае
/// entity уже мертва). Читается как обычное событие — `&[Removed<T>]` в
/// `system!`, `world.event_reader::<Removed<T>>()` — с per-reader курсорами
/// (без дублей и пропусков) и обычной дисциплиной флаша (per-stage в
/// планировщике / `advance_frame()` без него).
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

/// Реестр очередей событий — карта `TypeId → Events<T>`.
pub struct EventRegistry {
    queues: FxHashMap<TypeId, EventQueueCell>,
}

impl EventRegistry {
    pub fn new() -> Self {
        Self {
            queues: FxHashMap::default(),
        }
    }

    /// Зарегистрировать тип события.
    pub fn register<T: Send + Sync + 'static>(&mut self) {
        self.queues
            .entry(TypeId::of::<T>())
            .or_insert_with(|| EventQueueCell(UnsafeCell::new(Box::new(Events::<T>::new()))));
    }

    /// Получить очередь событий по типу (паникует если не зарегистрирована).
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

    /// Мутабельный доступ к очереди (паникует если не зарегистрирована).
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

    /// Попробовать получить очередь событий по типу.
    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&Events<T>> {
        self.queues
            .get(&TypeId::of::<T>())
            // SAFETY: shared read of the cell interior (see `get`).
            .and_then(|c| unsafe { (*c.0.get()).as_any().downcast_ref::<Events<T>>() })
    }

    /// Попробовать получить мутабельный доступ к очереди.
    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut Events<T>> {
        self.queues
            .get_mut(&TypeId::of::<T>())
            .and_then(|c| c.0.get_mut().as_any_mut().downcast_mut::<Events<T>>())
    }

    /// Получить мутабельный доступ к очереди, автоматически регистрируя тип.
    pub fn get_or_register_mut<T: Send + Sync + 'static>(&mut self) -> &mut Events<T> {
        if !self.queues.contains_key(&TypeId::of::<T>()) {
            self.register::<T>();
        }
        self.queues
            .get_mut(&TypeId::of::<T>())
            .and_then(|c| c.0.get_mut().as_any_mut().downcast_mut::<Events<T>>())
            .unwrap()
    }

    /// Raw pointer для EventWriter.
    ///
    /// # Safety
    /// Вызывающий гарантирует уникальный доступ. Данные на куче (Box)
    /// не перемещаются при реаллокации HashMap.
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

    /// Предварительно выделить capacity для очереди событий по TypeId.
    pub fn reserve_by_type(&mut self, type_id: TypeId, capacity: usize) {
        if let Some(cell) = self.queues.get_mut(&type_id) {
            cell.0.get_mut().reserve(capacity);
        }
    }

    /// Обновить все очереди (вызывается в конце тика).
    pub fn update_all(&mut self) {
        for cell in self.queues.values_mut() {
            cell.0.get_mut().update();
        }
    }

    /// Flush конкретных типов событий (по TypeId).
    /// Используется Scheduler для per-Stage flush.
    pub fn flush_by_type_id(&mut self, type_ids: &[TypeId]) {
        for tid in type_ids {
            if let Some(cell) = self.queues.get_mut(tid) {
                cell.0.get_mut().update();
            }
        }
    }

    /// Flush всех очередей событий.
    /// Используется при работе без Scheduler.
    pub fn flush_all(&mut self) {
        self.update_all();
    }

    /// Получить мутабельный доступ к очереди по TypeId.
    pub fn get_mut_by_typeid(&mut self, type_id: TypeId) -> Option<&mut dyn AnyEventQueue> {
        self.queues
            .get_mut(&type_id)
            .map(|c| &mut **c.0.get_mut())
    }

    /// Проверить, зарегистрирован ли тип события.
    pub fn is_registered<T: Send + Sync + 'static>(&self) -> bool {
        self.queues.contains_key(&TypeId::of::<T>())
    }

    /// Количество зарегистрированных типов событий.
    pub fn queue_count(&self) -> usize {
        self.queues.len()
    }

    /// Общее количество событий во всех очередях.
    pub fn total_event_count(&self) -> usize {
        // SAFETY: shared read of each cell interior (see `get`).
        self.queues
            .values()
            .map(|c| unsafe { (*c.0.get()).len() })
            .sum()
    }
}

impl Default for EventRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Тесты ─────────────────────────────────────────────────────

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

        // Продвигаем курсор
        queue.advance_reader_mut(&reader);
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 0, "после advance курсор должен быть в конце");
    }

    // ── EventIterator (TD-24 движка): for e in read() ──────────

    #[test]
    fn read_guard_into_iterator_yields_and_advances() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        queue.send(1);
        queue.send(2);
        queue.send(3);
        queue.update();

        // Главная Bevy-идиома: прямая итерация по read() без .iter().
        let mut got = Vec::new();
        for e in queue.read(&reader) {
            got.push(*e);
        }
        assert_eq!(got, vec![1, 2, 3]);

        // Дроп итератора продвинул курсор до конца буфера.
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
                break; // частичная итерация
            }
        }
        // Семантика guard'а сохранена: break = пропуск остатка.
        assert_eq!(
            queue.iter(&reader).len(),
            0,
            "дроп итератора (в т.ч. на break) продвигает курсор до конца"
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

        // Ссылки переживают сам итератор (лайфтайм исходного заёма Events).
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
            // len()/is_empty() доступны на guard'е через Deref<[T]>.
            let guard = queue.read(&reader);
            assert_eq!(guard.len(), 2);
            assert!(!guard.is_empty());
            // Итерация по ссылке не потребляет guard.
            let doubled: Vec<i32> = (&guard).into_iter().map(|e| e * 2).collect();
            assert_eq!(doubled, vec![10, 12]);
            // ExactSizeIterator у владеющего итератора.
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

        // Reader A читает одно событие
        {
            let events = queue.iter(&reader_a);
            assert_eq!(events.len(), 2);
        }
        queue.advance_reader_mut(&reader_a);

        // Reader B ничего не читал — всё ещё может прочитать оба
        {
            let events = queue.iter(&reader_b);
            assert_eq!(events.len(), 2);
        }
        queue.advance_reader_mut(&reader_b);

        // Оба прочитали — следующее update может очистить
        queue.update();
        queue.send(3);
        queue.update();

        // Новые события
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

        // После удаления читателя, очередь не должна паниковать
        // и должна работать нормально для других читателей
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

    // ── DelayedQueue тесты ─────────────────────────────────────

    #[test]
    fn delayed_event_delivery() {
        let mut queue = Events::new();
        let reader = queue.add_reader();
        let mut delayed = DelayedQueue::new();

        // Отправляем событие с задержкой 3 тика
        delayed.send_delayed(99, 3, 0);
        assert_eq!(delayed.len(), 1);

        // Тик 1: ничего не должно доставиться
        delayed.flush_delayed(1, &mut queue);
        assert_eq!(queue.len_pending(), 0);

        // Тик 2: ничего
        delayed.flush_delayed(2, &mut queue);
        assert_eq!(queue.len_pending(), 0);

        // Тик 3: должно доставиться
        delayed.flush_delayed(3, &mut queue);
        assert_eq!(queue.len_pending(), 1);
        assert!(delayed.is_empty());

        // После update() событие доступно для чтения
        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], 99);
    }

    #[test]
    fn delayed_event_fifo_same_tick() {
        // События с одинаковым deliver_at должны доставляться в порядке вставки (FIFO)
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
        // FIFO: порядок вставки должен сохраняться
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

        // Тик 1: два события с задержкой 1
        delayed.flush_delayed(1, &mut queue);
        assert_eq!(queue.len_pending(), 2);

        queue.update();
        let events = queue.iter(&reader);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], 10);
        assert_eq!(events[1], 30);
        queue.advance_reader_mut(&reader);

        // Тик 2: одно событие с задержкой 2
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
        // flush_delayed должен останавливаться как только вершина heap "не готова"
        let mut queue = Events::new();
        let mut delayed = DelayedQueue::new();

        delayed.send_delayed(1, 10, 0); // deliver_at=10
        delayed.send_delayed(2, 10, 0); // deliver_at=10
        delayed.send_delayed(3, 20, 0); // deliver_at=20

        // flush на тике 10 — должны доставиться события с deliver_at <= 10
        delayed.flush_delayed(10, &mut queue);
        assert_eq!(queue.len_pending(), 2);
        assert_eq!(delayed.len(), 1, "событие с deliver_at=20 должно остаться");

        // flush на тике 15 — ничего нового
        delayed.flush_delayed(15, &mut queue);
        assert_eq!(delayed.len(), 1);

        // flush на тике 20 — последнее событие
        delayed.flush_delayed(20, &mut queue);
        assert_eq!(delayed.len(), 0);
    }

    // ── read_partial тесты ─────────────────────────────────────

    #[test]
    fn read_partial_advances_by_count() {
        let mut queue = Events::new();
        let reader = queue.add_reader();

        for i in 0..10 {
            queue.send(i);
        }
        queue.update();

        // Читаем первые 3 события
        {
            let guard = queue.read_partial(&reader, 3);
            assert_eq!(guard.len(), 3);
            assert_eq!(guard.as_slice(), &[0, 1, 2]);
        } // Drop: курсор продвинется на 3

        // Читаем следующие 3
        {
            let guard = queue.read_partial(&reader, 3);
            assert_eq!(guard.len(), 3);
            assert_eq!(guard.as_slice(), &[3, 4, 5]);
        }

        // Читаем больше чем осталось
        {
            let guard = queue.read_partial(&reader, 100);
            assert_eq!(guard.len(), 4); // остались 6,7,8,9
        }

        // Ничего не осталось
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

        // read() должен продвинуть до конца при дропе
        {
            let _guard = queue.read(&reader);
            // читаем только первый
            // _guard.iter().next(); — не важно, при дропе всё равно до конца
        }

        // Курсор должен быть в конце — новых событий нет
        let remaining = queue.iter(&reader);
        assert_eq!(
            remaining.len(),
            0,
            "read() должен продвинуть до конца буфера"
        );
    }

    // ── send_sync тесты ────────────────────────────────────────

    #[test]
    fn send_sync_visible_after_flush() {
        let mut queue = Events::<i32>::new();
        let reader = queue.add_reader();

        // send_sync через shared reference
        queue.send_sync(42);
        queue.send_sync(43);

        // До flush_sync — в pending ещё нет
        assert_eq!(queue.len_pending(), 0);

        // flush_sync переносит из sync_pending в pending
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
        queue.send(200); // в основной pending

        // update() должен слить sync_pending + pending и сделать swap
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

        // Тик 1
        queue.send(1);
        queue.update();
        assert_eq!(queue.iter(&reader).len(), 1);
        queue.advance_reader_mut(&reader);

        // Тик 2
        queue.send(2);
        queue.update();
        assert_eq!(queue.iter(&reader).len(), 1);
        assert_eq!(queue.iter(&reader)[0], 2);
    }

    #[test]
    fn event_auto_register_via_send() {
        use crate::world::World;

        let mut world = World::new();
        // Не вызываем add_event::<String>()
        world.send_event("auto-registered".to_string());

        world.tick();
        world.flush_all_events();
        let queue = world.events::<String>();
        assert_eq!(
            queue.len_readable(),
            1,
            "Событие должно быть доступно после tick()"
        );
    }

    /// C7: `advance_frame()` самодостаточен — флашит события (становятся читаемы
    /// на следующем кадре) И продвигает change-tick. Без ручной пары tick+flush.
    #[test]
    fn advance_frame_flushes_events_and_ticks() {
        use crate::world::World;

        let mut world = World::new();
        let t0 = world.current_tick();

        world.send_event(7u32);
        // До конца кадра событие ещё не во «readable»-буфере.
        assert_eq!(world.events::<u32>().len_readable(), 0);

        world.advance_frame(); // флаш + tick

        assert_eq!(
            world.events::<u32>().len_readable(),
            1,
            "advance_frame должен сделать событие читаемым"
        );
        assert_ne!(world.current_tick(), t0, "advance_frame должен продвинуть change-tick");
    }
}
