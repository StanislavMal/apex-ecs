/// Resources — глобальные синглтоны мира.
use std::any::{Any, TypeId};
use rustc_hash::FxHashMap;

trait ResourceStorage: Send + Sync {
    fn as_any(&self)     -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync>;
}

struct ResourceStorageImpl(Box<dyn Any + Send + Sync>);

impl ResourceStorage for ResourceStorageImpl {
    fn as_any(&self)         -> &dyn Any { &*self.0 }
    fn as_any_mut(&mut self) -> &mut dyn Any { &mut *self.0 }
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send + Sync> { self.0 }
}

pub struct ResourceMap {
    data: FxHashMap<TypeId, Box<dyn ResourceStorage>>,
}

impl ResourceMap {
    pub fn new() -> Self {
        Self { data: FxHashMap::default() }
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.data.insert(
            TypeId::of::<T>(),
            Box::new(ResourceStorageImpl(Box::new(value))),
        );
    }

    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &T {
        self.try_get::<T>().unwrap_or_else(|| panic!(
            "Resource `{}` not found. Did you forget insert_resource()?",
            std::any::type_name::<T>()
        ))
    }

    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut T {
        self.try_get_mut::<T>().unwrap_or_else(|| panic!(
            "Resource `{}` not found. Did you forget insert_resource()?",
            std::any::type_name::<T>()
        ))
    }

    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.data
            .get(&TypeId::of::<T>())
            .and_then(|b| b.as_any().downcast_ref::<T>())
    }

    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.data
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.as_any_mut().downcast_mut::<T>())
    }

    /// Получить raw mutable pointer на ресурс.
    ///
    /// Используется `SystemContext::resource_mut` для параллельного доступа.
    /// Метод определён здесь (в своём крейте) — это законно.
    ///
    /// # Safety
    /// Вызывающий код должен гарантировать что только одна система
    /// в данный момент держит мутабельный доступ к T.
    /// Планировщик обеспечивает это через `AccessDescriptor`.
    pub fn get_raw_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut T> {
        // SAFETY: мы берём shared ref и кастуем в *mut.
        // Это тот же паттерн что UnsafeCell<T>::get().
        // Безопасность обеспечивается планировщиком: два ParSystem
        // с Write<T> к одному ресурсу никогда не в одном Stage.
        let r = self.try_get::<T>()?;
        Some(r as *const T as *mut T)
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.data
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.into_any().downcast::<T>().ok().map(|b| *b))
    }

    #[inline]
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.data.contains_key(&TypeId::of::<T>())
    }

    pub fn len(&self)      -> usize { self.data.len() }
    pub fn is_empty(&self) -> bool  { self.data.is_empty() }
}

impl Default for ResourceMap {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gravity(f32);
    struct Score(u32);

    #[test]
    fn insert_get() {
        let mut map = ResourceMap::new();
        map.insert(Gravity(9.8));
        assert_eq!(map.get::<Gravity>().0, 9.8);
    }

    #[test]
    fn get_mut() {
        let mut map = ResourceMap::new();
        map.insert(Score(0));
        map.get_mut::<Score>().0 += 10;
        assert_eq!(map.get::<Score>().0, 10);
    }

    #[test]
    fn try_get_missing() {
        let map = ResourceMap::new();
        assert!(map.try_get::<Gravity>().is_none());
    }

    #[test]
    fn remove() {
        let mut map = ResourceMap::new();
        map.insert(Score(42));
        assert!(map.contains::<Score>());
        map.remove::<Score>();
        assert!(!map.contains::<Score>());
    }

    #[test]
    fn get_raw_ptr() {
        let mut map = ResourceMap::new();
        map.insert(Score(10));
        let ptr = map.get_raw_ptr::<Score>().unwrap();
        // SAFETY: тест — единственный владелец
        unsafe { (*ptr).0 = 99; }
        assert_eq!(map.get::<Score>().0, 99);
    }

    #[test]
    #[should_panic(expected = "not found")]
    fn get_panics_if_missing() {
        let map = ResourceMap::new();
        let _ = map.get::<Gravity>();
    }
}