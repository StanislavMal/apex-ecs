/// Events — типизированная шина событий с двойной буферизацией.
///
/// # Архитектура
///
/// Используется классическая схема двух буферов Bevy-style:
/// - `current`  — буфер текущего тика: сюда записываются события.
/// - `previous` — буфер предыдущего тика: отсюда читают системы.
///
/// На каждый `world.tick()` / `events.update()`:
///   previous ← current, current ← []
///
/// Это означает что события живут **два тика** — именно столько
/// нужно чтобы системы, работающие в том же тике что и отправитель,
/// успели прочитать событие.
///
/// # Пример
/// ```ignore
/// // Регистрация
/// world.add_event::<DamageEvent>();
///
/// // Отправка
/// world.events_mut::<DamageEvent>().send(DamageEvent { amount: 42 });
///
/// // Чтение (в другой системе)
/// for ev in world.events::<DamageEvent>().iter_current() {
///     println!("damage: {}", ev.amount);
/// }
/// ```

use rustc_hash::FxHashMap;
use std::any::{Any, TypeId};

// ── EventQueue<T> ──────────────────────────────────────────────

/// Типизированная очередь событий для конкретного типа T.
pub struct EventQueue<T> {
    /// Текущий тик — туда записываем в этом фрейме
    current: Vec<T>,
    /// Предыдущий тик — оттуда читаем в этом фрейме
    previous: Vec<T>,
}

impl<T> EventQueue<T> {
    pub fn new() -> Self {
        Self {
            current: Vec::new(),
            previous: Vec::new(),
        }
    }

    /// Отправить событие в текущий буфер.
    #[inline]
    pub fn send(&mut self, event: T) {
        self.current.push(event);
    }

    /// Отправить несколько событий.
    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        self.current.extend(events);
    }

    /// Итерация по событиям **текущего** тика.
    /// Используется когда отправитель и читатель — в одной фазе.
    #[inline]
    pub fn iter_current(&self) -> std::slice::Iter<'_, T> {
        self.current.iter()
    }

    /// Итерация по событиям **предыдущего** тика.
    /// Стандартный режим — отправили в тике N, читают в тике N+1.
    #[inline]
    pub fn iter_previous(&self) -> std::slice::Iter<'_, T> {
        self.previous.iter()
    }

    /// Итерация по всем доступным событиям (prev + current).
    pub fn iter_all(&self) -> impl Iterator<Item = &T> {
        self.previous.iter().chain(self.current.iter())
    }

    /// Обновить буферы: current → previous, current = [].
    /// Вызывается раз в тик (обычно через `world.tick()`).
    pub fn update(&mut self) {
        // Переиспользуем выделенную память prev буфера
        self.previous.clear();
        std::mem::swap(&mut self.current, &mut self.previous);
    }

    /// Сколько событий в текущем буфере.
    pub fn len_current(&self) -> usize { self.current.len() }

    /// Сколько событий в предыдущем буфере.
    pub fn len_previous(&self) -> usize { self.previous.len() }

    /// Всего доступных событий.
    pub fn len(&self) -> usize { self.current.len() + self.previous.len() }

    pub fn is_empty(&self) -> bool { self.current.is_empty() && self.previous.is_empty() }

    /// Очистить оба буфера (полный flush).
    pub fn clear(&mut self) {
        self.current.clear();
        self.previous.clear();
    }
}

impl<T> Default for EventQueue<T> {
    fn default() -> Self { Self::new() }
}

// ── Type-erased trait для хранения в HashMap ──────────────────

trait AnyEventQueue: Any + Send + Sync {
    fn update(&mut self);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn len(&self) -> usize;
}

impl<T: Send + Sync + 'static> AnyEventQueue for EventQueue<T> {
    fn update(&mut self) { EventQueue::update(self); }
    fn as_any(&self) -> &dyn Any { self }
    fn as_any_mut(&mut self) -> &mut dyn Any { self }
    fn len(&self) -> usize { EventQueue::len(self) }
}

// ── EventRegistry ──────────────────────────────────────────────

/// Реестр всех EventQueue. Хранится в World.
pub struct EventRegistry {
    queues: FxHashMap<TypeId, Box<dyn AnyEventQueue>>,
}

impl EventRegistry {
    pub fn new() -> Self {
        Self { queues: FxHashMap::default() }
    }

    /// Зарегистрировать тип события (создать пустую очередь).
    /// Идемпотентно — повторная регистрация ничего не делает.
    pub fn register<T: Send + Sync + 'static>(&mut self) {
        self.queues
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(EventQueue::<T>::new()));
    }

    /// Получить очередь для типа T. Паникует если не зарегистрирована.
    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &EventQueue<T> {
        self.try_get::<T>().unwrap_or_else(|| {
            panic!(
                "Event `{}` not registered. Call world.add_event::<{0}>()",
                std::any::type_name::<T>()
            )
        })
    }

    /// Получить мутабельную очередь для типа T. Паникует если не зарегистрирована.
    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut EventQueue<T> {
        self.try_get_mut::<T>().unwrap_or_else(|| {
            panic!(
                "Event `{}` not registered. Call world.add_event::<{0}>()",
                std::any::type_name::<T>()
            )
        })
    }

    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&EventQueue<T>> {
        self.queues
            .get(&TypeId::of::<T>())
            .and_then(|b| b.as_any().downcast_ref::<EventQueue<T>>())
    }

    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut EventQueue<T>> {
        self.queues
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.as_any_mut().downcast_mut::<EventQueue<T>>())
    }

    /// Обновить все очереди (double-buffer swap). Вызывается в `world.tick()`.
    pub fn update_all(&mut self) {
        for queue in self.queues.values_mut() {
            queue.update();
        }
    }

    pub fn is_registered<T: Send + Sync + 'static>(&self) -> bool {
        self.queues.contains_key(&TypeId::of::<T>())
    }

    pub fn queue_count(&self) -> usize { self.queues.len() }
}

impl Default for EventRegistry {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct DamageEvent { amount: u32, target_id: u32 }

    #[derive(Debug, Clone)]
    struct SpawnEvent { kind: String }

    #[test]
    fn send_and_read_current() {
        let mut q: EventQueue<DamageEvent> = EventQueue::new();
        q.send(DamageEvent { amount: 10, target_id: 1 });
        q.send(DamageEvent { amount: 20, target_id: 2 });

        let events: Vec<_> = q.iter_current().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].amount, 10);
    }

    #[test]
    fn double_buffer_swap() {
        let mut q: EventQueue<DamageEvent> = EventQueue::new();
        q.send(DamageEvent { amount: 5, target_id: 0 });

        // После update — событие уходит в previous
        q.update();
        assert_eq!(q.len_current(), 0);
        assert_eq!(q.len_previous(), 1);
        assert_eq!(q.iter_previous().next().unwrap().amount, 5);

        // После второго update — previous тоже очищается
        q.update();
        assert_eq!(q.len_previous(), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn registry_register_get() {
        let mut reg = EventRegistry::new();
        reg.register::<DamageEvent>();
        reg.get_mut::<DamageEvent>().send(DamageEvent { amount: 99, target_id: 7 });

        let ev = reg.get::<DamageEvent>().iter_current().next().unwrap();
        assert_eq!(ev.amount, 99);
    }

    #[test]
    fn registry_update_all() {
        let mut reg = EventRegistry::new();
        reg.register::<DamageEvent>();
        reg.register::<SpawnEvent>();

        reg.get_mut::<DamageEvent>().send(DamageEvent { amount: 1, target_id: 0 });
        reg.get_mut::<SpawnEvent>().send(SpawnEvent { kind: "orc".into() });

        reg.update_all();

        assert_eq!(reg.get::<DamageEvent>().len_previous(), 1);
        assert_eq!(reg.get::<SpawnEvent>().len_previous(), 1);
        assert_eq!(reg.get::<DamageEvent>().len_current(), 0);
    }

    #[test]
    #[should_panic(expected = "not registered")]
    fn get_unregistered_panics() {
        let reg = EventRegistry::new();
        let _ = reg.get::<DamageEvent>();
    }

    #[test]
    fn send_batch() {
        let mut q: EventQueue<DamageEvent> = EventQueue::new();
        q.send_batch([
            DamageEvent { amount: 1, target_id: 0 },
            DamageEvent { amount: 2, target_id: 1 },
            DamageEvent { amount: 3, target_id: 2 },
        ]);
        assert_eq!(q.len_current(), 3);
    }
}