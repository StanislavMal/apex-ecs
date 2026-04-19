use rustc_hash::FxHashMap;
use std::any::{Any, TypeId};

pub struct EventQueue<T> {
    current:  Vec<T>,
    previous: Vec<T>,
}

impl<T> EventQueue<T> {
    pub fn new() -> Self {
        Self { current: Vec::new(), previous: Vec::new() }
    }

    #[inline] pub fn send(&mut self, event: T) { self.current.push(event); }

    pub fn send_batch(&mut self, events: impl IntoIterator<Item = T>) {
        self.current.extend(events);
    }

    #[inline] pub fn iter_current(&self)  -> std::slice::Iter<'_, T> { self.current.iter() }
    #[inline] pub fn iter_previous(&self) -> std::slice::Iter<'_, T> { self.previous.iter() }

    pub fn iter_all(&self) -> impl Iterator<Item = &T> {
        self.previous.iter().chain(self.current.iter())
    }

    pub fn update(&mut self) {
        self.previous.clear();
        std::mem::swap(&mut self.current, &mut self.previous);
    }

    pub fn len_current(&self)  -> usize { self.current.len() }
    pub fn len_previous(&self) -> usize { self.previous.len() }
    pub fn len(&self)          -> usize { self.current.len() + self.previous.len() }
    pub fn is_empty(&self)     -> bool  { self.current.is_empty() && self.previous.is_empty() }

    pub fn clear(&mut self) {
        self.current.clear();
        self.previous.clear();
    }
}

impl<T> Default for EventQueue<T> {
    fn default() -> Self { Self::new() }
}

trait AnyEventQueue: Any + Send + Sync {
    fn update(&mut self);
    fn as_any(&self)     -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn len(&self)        -> usize;
    /// Raw mutable pointer для EventWriter в SystemContext.
    fn as_ptr_mut(&mut self) -> *mut u8;
}

impl<T: Send + Sync + 'static> AnyEventQueue for EventQueue<T> {
    fn update(&mut self)          { EventQueue::update(self); }
    fn as_any(&self)              -> &dyn Any { self }
    fn as_any_mut(&mut self)      -> &mut dyn Any { self }
    fn len(&self)                 -> usize { EventQueue::len(self) }
    fn as_ptr_mut(&mut self)      -> *mut u8 { self as *mut EventQueue<T> as *mut u8 }
}

pub struct EventRegistry {
    queues: FxHashMap<TypeId, Box<dyn AnyEventQueue>>,
}

impl EventRegistry {
    pub fn new() -> Self { Self { queues: FxHashMap::default() } }

    pub fn register<T: Send + Sync + 'static>(&mut self) {
        self.queues
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(EventQueue::<T>::new()));
    }

    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &EventQueue<T> {
        self.try_get::<T>().unwrap_or_else(|| panic!(
            "Event `{}` not registered. Call world.add_event::<{0}>()",
            std::any::type_name::<T>()
        ))
    }

    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut EventQueue<T> {
        self.try_get_mut::<T>().unwrap_or_else(|| panic!(
            "Event `{}` not registered. Call world.add_event::<{0}>()",
            std::any::type_name::<T>()
        ))
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

    /// Raw pointer для EventWriter — используется SystemContext.
    /// # Safety: вызывающий гарантирует уникальный доступ.
    pub fn get_raw_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut EventQueue<T>> {
        let queue = self.try_get::<T>()?;
        Some(queue as *const EventQueue<T> as *mut EventQueue<T>)
    }

    pub fn update_all(&mut self) {
        for queue in self.queues.values_mut() { queue.update(); }
    }

    pub fn is_registered<T: Send + Sync + 'static>(&self) -> bool {
        self.queues.contains_key(&TypeId::of::<T>())
    }

    pub fn queue_count(&self) -> usize { self.queues.len() }
}

impl Default for EventRegistry {
    fn default() -> Self { Self::new() }
}