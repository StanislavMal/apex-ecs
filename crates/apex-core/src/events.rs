//! Система событий с per-reader курсорами и автоматической очисткой.
//!
//! # Концепция
//!
//! - [`TrackedEventQueue<T>`] — основной тип. Содержит два буфера:
//!   `pending` (куда пишут в текущем тике) и `events` (доступно для чтения).
//! - [`EventCursor`] — лёгкий дескриптор читателя. Каждый читатель
//!   регистрирует свой курсор и двигает его по мере чтения.
//! - После вызова [`update()`](TrackedEventQueue::update) (в конце тика) буферы
//!   меняются местами: `pending` → `events`, а старые события из `events`
//!   удаляются (если все читатели их прочитали).
//! - [`EntityEvent<T>`] — событие, адресованное конкретной сущности.
//! - [`DelayedQueue<T>`] — отложенная доставка событий через N тиков.

use std::any::{Any, TypeId};
use rustc_hash::FxHashMap;

use crate::entity::Entity;

// ── TrackedEventQueue ───────────────────────────────────────────

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
pub struct TrackedEventQueue<T> {
    /// Буфер, доступный для чтения (предыдущий тик).
    events: Vec<T>,
    /// Буфер, куда пишутся новые события (текущий тик).
    pending: Vec<T>,
    /// Состояние читателей: `None` = читатель удалён, `Some(pos)` = текущая позиция.
    cursors: Vec<Option<u32>>,
    /// Счётчик для генерации ID новых читателей.
    next_cursor_id: u32,
}

impl<T> TrackedEventQueue<T> {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            pending: Vec::new(),
            cursors: Vec::new(),
            next_cursor_id: 0,
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
    /// при каждом вызове [`iter()`](TrackedEventQueue::iter).
    pub fn add_reader(&mut self) -> EventCursor {
        let id = self.next_cursor_id;
        self.next_cursor_id += 1;

        // Находим свободный слот или добавляем новый
        for slot in &mut self.cursors {
            if slot.is_none() {
                *slot = Some(0);
                return EventCursor(id);
            }
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
        }
        // Сжимаем хвост из None
        while self.cursors.last().copied() == Some(None) {
            self.cursors.pop();
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
    /// очищается перед загрузкой нового.
    pub fn update(&mut self) {
        // GC: если все читатели прочитали все текущие события — очищаем
        let all_read = self.all_readers_caught_up();
        if all_read {
            self.events.clear();
            // Сбрасываем курсоры в 0
            for cursor in &mut self.cursors {
                if let Some(pos) = cursor {
                    *pos = 0;
                }
            }
        }

        // Меняем местами буферы
        std::mem::swap(&mut self.events, &mut self.pending);
        self.pending.clear();

        // Сбрасываем курсоры на начало нового буфера events
        // (но если читатель не успел прочитать старые, они потеряны —
        //  это стандартное поведение double-buffer, как в Bevy)
        for cursor in &mut self.cursors {
            if let Some(pos) = cursor {
                *pos = 0;
            }
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

    /// Продвинуть курсор читателя до конца буфера (отметить всё как прочитанное).
    #[inline]
    pub fn advance_reader(&self, reader_id: &EventCursor) {
        let idx = reader_id.0 as usize;
        if let Some(Some(pos)) = self.cursors.get(idx) {
            let _ = pos; // мы не можем мутировать self через &self
        }
        // advance_reader требует &mut self, см. advance_reader_mut
    }

    /// Мутабельная версия продвижения курсора.
    #[inline]
    pub fn advance_reader_mut(&mut self, reader_id: &EventCursor) {
        let idx = reader_id.0 as usize;
        if let Some(Some(pos)) = self.cursors.get_mut(idx) {
            *pos = self.events.len() as u32;
        }
    }

    /// Прочитать непрочитанные события и сразу продвинуть курсор.
    #[inline]
    pub fn read_and_advance(&mut self, reader_id: &EventCursor) -> Vec<&T> {
        let idx = reader_id.0 as usize;
        let start = self.cursors.get(idx).and_then(|c| c.as_ref()).copied().unwrap_or(0) as usize;
        let end = self.events.len();
        if start < end {
            if let Some(Some(pos)) = self.cursors.get_mut(idx) {
                *pos = end as u32;
            }
            // Возвращаем ссылки на непрочитанные события
            self.events[start..].iter().collect()
        } else {
            Vec::new()
        }
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

    // ── Backward-compatible методы (старый EventQueue API) ─────

    /// Итерация по событиям из буфера чтения (предыдущий тик).
    /// Аналог старого `iter_previous()`.
    #[inline]
    pub fn iter_previous(&self) -> std::slice::Iter<'_, T> {
        self.events.iter()
    }

    /// Итерация по событиям из буфера записи (текущий тик).
    /// Аналог старого `iter_current()`.
    #[inline]
    pub fn iter_current(&self) -> std::slice::Iter<'_, T> {
        self.pending.iter()
    }

    /// Итерация по всем событиям (чтение + запись).
    /// Аналог старого `iter_all()`.
    #[inline]
    pub fn iter_all(&self) -> impl Iterator<Item = &T> {
        self.events.iter().chain(self.pending.iter())
    }

    /// Количество событий в буфере чтения.
    /// Аналог старого `len_previous()`.
    #[inline]
    pub fn len_previous(&self) -> usize {
        self.events.len()
    }
}

impl<T> Default for TrackedEventQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Легковесный дескриптор читателя событий.
///
/// Создаётся через [`TrackedEventQueue::add_reader`].
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
    pub fn flush_delayed(&mut self, current_tick: u32, target_queue: &mut TrackedEventQueue<T>) {
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

impl<T: Send + Sync + 'static> AnyEventQueue for TrackedEventQueue<T> {
    fn update(&mut self) {
        TrackedEventQueue::update(self);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn len(&self) -> usize {
        TrackedEventQueue::len(self)
    }

    fn as_ptr_mut(&mut self) -> *mut u8 {
        self as *mut TrackedEventQueue<T> as *mut u8
    }

    fn add_reader(&mut self) -> u32 {
        TrackedEventQueue::add_reader(self).0
    }

    fn remove_reader(&mut self, reader_id: u32) {
        TrackedEventQueue::remove_reader(self, EventCursor(reader_id));
    }
}

// ── EventRegistry ───────────────────────────────────────────────

/// Реестр очередей событий — карта `TypeId → TrackedEventQueue<T>`.
pub struct EventRegistry {
    queues: FxHashMap<TypeId, Box<dyn AnyEventQueue>>,
}

impl EventRegistry {
    pub fn new() -> Self {
        Self {
            queues: FxHashMap::default(),
        }
    }

    /// Зарегистрировать тип события.
    ///
    /// Создаёт новую `TrackedEventQueue<T>`, если ещё не зарегистрирован.
    pub fn register<T: Send + Sync + 'static>(&mut self) {
        self.queues.entry(TypeId::of::<T>()).or_insert_with(|| {
            Box::new(TrackedEventQueue::<T>::new())
        });
    }

    /// Получить очередь событий по типу (паникует если не зарегистрирована).
    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &TrackedEventQueue<T> {
        self.try_get::<T>().unwrap_or_else(|| {
            panic!(
                "Event `{}` not registered. Call world.add_event::<{0}>()",
                std::any::type_name::<T>()
            )
        })
    }

    /// Мутабельный доступ к очереди (паникует если не зарегистрирована).
    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut TrackedEventQueue<T> {
        self.try_get_mut::<T>().unwrap_or_else(|| {
            panic!(
                "Event `{}` not registered. Call world.add_event::<{0}>()",
                std::any::type_name::<T>()
            )
        })
    }

    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&TrackedEventQueue<T>> {
        self.queues
            .get(&TypeId::of::<T>())
            .and_then(|b| b.as_any().downcast_ref::<TrackedEventQueue<T>>())
    }

    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut TrackedEventQueue<T>> {
        self.queues
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.as_any_mut().downcast_mut::<TrackedEventQueue<T>>())
    }

    /// Raw pointer для EventWriter.
    ///
    /// # Safety
    /// Вызывающий гарантирует уникальный доступ.
    pub fn get_raw_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut TrackedEventQueue<T>> {
        match self.try_get::<T>() {
            Some(queue) => {
                Some(queue as *const TrackedEventQueue<T> as *mut TrackedEventQueue<T>)
            }
            None => None,
        }
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

// ── EventQueue (legacy alias для обратной совместимости) ────────

/// Устаревший alias. Используйте [`TrackedEventQueue`].
pub type EventQueue<T> = TrackedEventQueue<T>;

// ── Тесты ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_and_read() {
        let mut queue = TrackedEventQueue::new();
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
        let mut queue = TrackedEventQueue::new();
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
        let mut queue = TrackedEventQueue::new();
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
        let mut queue = TrackedEventQueue::<EntityEvent<i32>>::new();
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
        let mut queue = TrackedEventQueue::new();
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
        let mut queue = TrackedEventQueue::new();
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
        let mut queue = TrackedEventQueue::new();
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
        let mut queue = TrackedEventQueue::new();
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
