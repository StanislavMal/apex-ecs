//! Система событий с per-reader курсорами и автоматической очисткой.
//!
//! # Концепция
//!
//! - [`Events<T>`] — основной тип. Содержит два буфера:
//!   `pending` (куда пишут в текущем тике) и `events` (доступно для чтения).
//! - [`EventCursor`] — лёгкий дескриптор читателя. Каждый читатель
//!   регистрирует свой курсор и двигает его по мере чтения.
//! - После вызова [`update()`](Events::update) (в конце тика) буферы
//!   меняются местами: `pending` → `events`, а старые события из `events`
//!   удаляются (если все читатели их прочитали).
//! - [`EntityEvent<T>`] — событие, адресованное конкретной сущности.
//! - [`DelayedQueue<T>`] — отложенная доставка событий через N тиков.

use std::any::{Any, TypeId};
use rustc_hash::FxHashMap;

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
pub struct Events<T> {
    /// Буфер, доступный для чтения (предыдущий тик).
    events: Vec<T>,
    /// Буфер, куда пишутся новые события (текущий тик).
    pending: Vec<T>,
    /// Состояние читателей: `None` = читатель удалён, `Some(pos)` = текущая позиция.
    cursors: Vec<Option<u32>>,
    /// Счётчик для генерации ID новых читателей.
    next_cursor_id: u32,
    /// Список освобождённых EventCursor ID для O(1) переиспользования.
    free_list: Vec<EventCursor>,
}

impl<T> Events<T> {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            pending: Vec::new(),
            cursors: Vec::new(),
            next_cursor_id: 0,
            free_list: Vec::new(),
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

    /// Зарегистрировать нового читателя.
    ///
    /// Возвращает [`EventCursor`], который нужно хранить и передавать
    /// при каждом вызове [`iter()`](Events::iter).
    pub fn add_reader(&mut self) -> EventCursor {
        let id = self.next_cursor_id;
        self.next_cursor_id += 1;

        // O(1): переиспользуем освобождённый слот из free_list
        if let Some(cursor) = self.free_list.pop() {
            let idx = cursor.0 as usize;
            if idx < self.cursors.len() {
                self.cursors[idx] = Some(0);
            }
            return cursor;
        }

        self.cursors.push(Some(0));
        EventCursor(id)
    }

    /// Удалить читателя.
    ///
    /// После удаления курсор перестаёт учитываться при GC,
    /// что может позволить очистить буфер `events`.
    pub fn remove_reader(&mut self, reader_id: EventCursor) {
        let idx = reader_id.0 as usize;
        if idx < self.cursors.len() {
            self.cursors[idx] = None;
            // O(1): сохраняем освобождённый ID для переиспользования
            self.free_list.push(reader_id);
        }
        // Сжимаем хвост из None (только если free_list пуст, иначе слот может понадобиться)
        if self.free_list.is_empty() {
            while self.cursors.last().copied() == Some(None) {
                self.cursors.pop();
            }
        }
    }

    /// Количество активных читателей.
    pub fn reader_count(&self) -> usize {
        self.cursors.iter().filter(|c| c.is_some()).count()
    }

    /// Обновить буферы: переместить `pending` в `events`.
    ///
    /// Вызывается автоматически в `world.tick()`.
    /// Если все читатели прочитали предыдущие события, буфер `events`
    /// очищается перед загрузкой нового. Если не все читатели догнали —
    /// старые события дописываются в конец нового буфера, чтобы
    /// отстающие читатели могли их догнать.
    pub fn update(&mut self) {
        let all_read = self.all_readers_caught_up();

        if all_read {
            // Все читатели прочитали старые события — очищаем буфер и сбрасываем курсоры
            self.events.clear();
            for cursor in &mut self.cursors {
                if let Some(pos) = cursor {
                    *pos = 0;
                }
            }
        }

        // Меняем местами буферы: events ← pending (новые события текущего тика)
        std::mem::swap(&mut self.events, &mut self.pending);

        if all_read {
            // Все читатели догнали — просто очищаем pending, курсоры уже в 0
            self.pending.clear();
        } else {
            // Не все читатели догнали: старые события (теперь в pending) нужно
            // дописать в конец events, чтобы отстающие читатели могли их догнать
            let new_count = self.events.len() as u32;

            // Сдвигаем курсоры на количество новых событий, которые встали перед старыми
            for cursor in &mut self.cursors {
                if let Some(pos) = cursor {
                    *pos += new_count;
                }
            }

            // Переносим старые события в конец буфера чтения
            self.events.append(&mut self.pending);
            // self.pending теперь пуст — append переместил все элементы
        }
    }

    /// Итерация по непрочитанным событиям из буфера `events`.
    ///
    /// После завершения итерации курсор читателя перемещается на конец буфера.
    #[inline]
    pub fn iter(&self, reader_id: &EventCursor) -> &[T] {
        let idx = reader_id.0 as usize;
        let cursor = self.cursors.get(idx).and_then(|c| c.as_ref());
        match cursor {
            Some(&pos) if (pos as usize) < self.events.len() => {
                &self.events[pos as usize..]
            }
            _ => &[],
        }
    }

    /// Мутабельная версия продвижения курсора.
    #[inline]
    pub fn advance_reader_mut(&mut self, reader_id: &EventCursor) {
        let idx = reader_id.0 as usize;
        if let Some(Some(pos)) = self.cursors.get_mut(idx) {
            *pos = self.events.len() as u32;
        }
    }

    /// Прочитать непрочитанные события с автоматическим продвижением курсора.
    ///
    /// Курсор продвигается при дропе возвращённого [`EventReadGuard`].
    #[inline]
    pub fn read(&mut self, reader_id: &EventCursor) -> EventReadGuard<'_, T> {
        let idx   = reader_id.0 as usize;
        let start = self.cursors.get(idx)
            .and_then(|c| c.as_ref())
            .copied()
            .unwrap_or(0) as usize;
        let start = start.min(self.events.len());
        EventReadGuard { queue: self, reader_id: *reader_id, start }
    }

    /// Количество событий в буфере чтения.
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
        self.free_list.clear();
        for cursor in &mut self.cursors {
            if let Some(pos) = cursor {
                *pos = 0;
            }
        }
    }

    fn all_readers_caught_up(&self) -> bool {
        let total = self.events.len() as u32;
        self.cursors.iter().all(|c| match c {
            Some(pos) => *pos >= total,
            None => true, // удалённые читатели не учитываем
        })
    }
}

impl<T> Default for Events<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII-обёртка: при дропе автоматически продвигает курсор до конца буфера.
///
/// Создаётся через [`Events::read`].
pub struct EventReadGuard<'q, T> {
    queue:     &'q mut Events<T>,
    reader_id: EventCursor,
    start:     usize,
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
    /// Курсор останется на месте после дропа guard.
    pub fn peek(self) -> PeekGuard<'q, T> {
        PeekGuard(self)
    }
}

impl<T> std::ops::Deref for EventReadGuard<'_, T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] { self.as_slice() }
}

impl<T> Drop for EventReadGuard<'_, T> {
    fn drop(&mut self) {
        // Автоматически продвигаем курсор при дропе
        self.queue.advance_reader_mut(&self.reader_id);
    }
}

/// Обёртка для "посмотреть без продвижения".
pub struct PeekGuard<'q, T>(EventReadGuard<'q, T>);

impl<T> std::ops::Deref for PeekGuard<'_, T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] { self.0.as_slice() }
}

impl<T> Drop for PeekGuard<'_, T> {
    fn drop(&mut self) {
        // Намеренно НЕ продвигаем курсор
    }
}

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

/// Очередь отложенных событий.
///
/// События, отправленные через `send_delayed`, не попадают сразу
/// в основной буфер, а хранятся до наступления указанного тика.
///
/// # Как это работает
///
/// 1. `send_delayed(event, delay_ticks)` — событие сохраняется с меткой
///    `deliver_at = current_tick + delay_ticks`.
/// 2. Каждый вызов `flush_delayed(current_tick)` перемещает все события,
///    у которых `deliver_at <= current_tick`, в `pending`-буфер основной очереди.
/// 3. Вызывается автоматически из `World::tick()` перед `update()`.
pub struct DelayedQueue<T> {
    /// Отложенные события, ожидающие доставки.
    pending_delayed: Vec<DelayedEvent<T>>,
}

struct DelayedEvent<T> {
    deliver_at: u32,
    event: T,
}

impl<T> DelayedQueue<T> {
    pub fn new() -> Self {
        Self {
            pending_delayed: Vec::new(),
        }
    }

    /// Отправить событие с задержкой в тиках.
    ///
    /// `delay` — количество тиков, через которые событие станет доступно.
    /// `current_tick` — текущий тик мира (для расчёта `deliver_at`).
    pub fn send_delayed(&mut self, event: T, delay: u32, current_tick: u32) {
        self.pending_delayed.push(DelayedEvent {
            deliver_at: current_tick.wrapping_add(delay),
            event,
        });
    }

    /// Переместить все события, готовые к доставке, в `target_queue`.
    ///
    /// Возвращает количество доставленных событий.
    pub fn flush_delayed(&mut self, current_tick: u32, target_queue: &mut Events<T>) {
        if self.pending_delayed.is_empty() {
            return;
        }

        // Используем swap_remove для эффективного удаления: забираем элемент
        // и на его место ставим последний. Не инкрементируем i, если забрали.
        let mut i = 0;
        while i < self.pending_delayed.len() {
            if self.pending_delayed[i].deliver_at <= current_tick {
                let ev = self.pending_delayed.swap_remove(i);
                target_queue.send(ev.event);
                // Не инкрементируем i — на место i пришёл последний элемент,
                // его тоже нужно проверить
            } else {
                i += 1;
            }
        }
    }

    /// Количество отложенных событий (ещё не доставленных).
    #[inline]
    pub fn len(&self) -> usize {
        self.pending_delayed.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pending_delayed.is_empty()
    }

    pub fn clear(&mut self) {
        self.pending_delayed.clear();
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
    /// Raw mutable pointer для EventWriter в SystemContext.
    fn as_ptr_mut(&mut self) -> *mut u8;
    /// Зарегистрировать читателя — возвращает EventCursor (как u32).
    fn add_reader(&mut self) -> u32;
    /// Удалить читателя.
    fn remove_reader(&mut self, reader_id: u32);
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
}

// ── EventRegistry ───────────────────────────────────────────────


/// Wrapper around `*mut u8` that implements `Send + Sync`.
///
/// # Safety
/// Указатель указывает на `Events<T>`, который живёт в `Box<dyn AnyEventQueue>`
/// внутри `self.queues`. Box гарантирует, что данные на куче не перемещаются.
/// `FxHashMap` может перемещать `Box` (указатель на кучу), но не данные внутри Box.
/// Таким образом, указатель остаётся валидным на всё время жизни `EventRegistry`.
#[derive(Clone, Copy)]
struct SyncPtr(*mut u8);
unsafe impl Send for SyncPtr {}
unsafe impl Sync for SyncPtr {}

/// Реестр очередей событий — карта `TypeId → Events<T>`.
///
/// Оптимизация: `raw_ptrs` хранит сырые указатели на `Events<T>`,
/// что позволяет `get::<T>()` и `get_mut::<T>()` работать без `downcast_ref`
/// (vtable call). Указатели валидны, т.к. `Box<dyn AnyEventQueue>` в `queues`
/// не перемещает данные на куче при реаллокации HashMap.
pub struct EventRegistry {
    queues:   FxHashMap<TypeId, Box<dyn AnyEventQueue>>,
    /// Zero-cost кеш для typed доступа: `TypeId → *mut Events<T>`.
    /// Позволяет избежать `downcast_ref` в get::<T>().
    /// SyncPtr — newtype с `unsafe impl Sync` для совместимости с `&World` в par_iter().
    raw_ptrs: FxHashMap<TypeId, SyncPtr>,
}

impl EventRegistry {
    pub fn new() -> Self {
        Self {
            queues:   FxHashMap::default(),
            raw_ptrs: FxHashMap::default(),
        }
    }

    /// Зарегистрировать тип события.
    ///
    /// Создаёт новую `Events<T>`, если ещё не зарегистрирован.
    /// Одновременно сохраняет raw pointer в `raw_ptrs` для zero-cost доступа.
    pub fn register<T: Send + Sync + 'static>(&mut self) {
        if !self.raw_ptrs.contains_key(&TypeId::of::<T>()) {
            let boxed = Box::new(Events::<T>::new());
            // Сохраняем указатель на данные Box (куча — не перемещается).
            // Box::as_ref() даёт ссылку на данные внутри Box, которые находятся
            // на куче и не изменят адрес при реаллокации HashMap.
            let ptr = Box::as_ref(&boxed) as *const Events<T> as *mut u8;
            self.raw_ptrs.insert(TypeId::of::<T>(), SyncPtr(ptr));
            self.queues.insert(TypeId::of::<T>(), boxed);
        }
    }

    /// Получить очередь событий по типу (паникует если не зарегистрирована).
    /// O(1) через raw_ptrs — без vtable вызовов.
    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &Events<T> {
        unsafe {
            let ptr = self.raw_ptrs.get(&TypeId::of::<T>())
                .unwrap_or_else(|| {
                    panic!(
                        "Event `{}` not registered. Call world.add_event::<{0}>()",
                        std::any::type_name::<T>()
                    )
                });
            &*(ptr.0 as *const Events<T>)
        }
    }

    /// Мутабельный доступ к очереди (паникует если не зарегистрирована).
    /// O(1) через raw_ptrs — без vtable вызовов.
    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut Events<T> {
        unsafe {
            let ptr = self.raw_ptrs.get(&TypeId::of::<T>())
                .unwrap_or_else(|| {
                    panic!(
                        "Event `{}` not registered. Call world.add_event::<{0}>()",
                        std::any::type_name::<T>()
                    )
                });
            &mut *(ptr.0 as *mut Events<T>)
        }
    }

    /// Попробовать получить очередь событий по типу.
    /// O(1) через raw_ptrs — без vtable вызовов.
    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&Events<T>> {
        unsafe {
            self.raw_ptrs.get(&TypeId::of::<T>())
                .map(|ptr| &*(ptr.0 as *const Events<T>))
        }
    }

    /// Попробовать получить мутабельный доступ к очереди.
    /// O(1) через raw_ptrs — без vtable вызовов.
    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut Events<T>> {
        unsafe {
            self.raw_ptrs.get(&TypeId::of::<T>())
                .map(|ptr| &mut *(ptr.0 as *mut Events<T>))
        }
    }

    /// Raw pointer для EventWriter (zero-cost, без vtable).
    ///
    /// # Safety
    /// Вызывающий гарантирует уникальный доступ.
    pub fn get_raw_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut Events<T>> {
        self.raw_ptrs.get(&TypeId::of::<T>())
            .map(|ptr| ptr.0 as *mut Events<T>)
    }

    /// Обновить все очереди (вызывается в конце тика).
    pub fn update_all(&mut self) {
        for queue in self.queues.values_mut() {
            queue.update();
        }
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
        self.queues.values().map(|q| q.len()).sum()
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
            // A продвигает курсор до конца
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

        let entity = Entity { index: 42, generation: 1 };
        queue.send(EntityEvent::new(entity, 100));
        queue.update();

        let events = queue.iter(&reader);
        assert_eq!(events.len(), 1);
        // Проверка по entity должна производиться в EventReader::iter_for_entity
        assert_eq!(events[0].target, entity);
        assert_eq!(events[0].data, 100);
    }

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

        queue.clear();
        // Тик 2: одно событие с задержкой 2
        delayed.flush_delayed(2, &mut queue);
        assert_eq!(queue.len_pending(), 1);
        assert!(delayed.is_empty());
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
}
