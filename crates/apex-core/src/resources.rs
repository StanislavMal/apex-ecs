use rustc_hash::FxHashMap;
/// Resources — глобальные синглтоны мира.
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;

/// A resource slot behind `UnsafeCell` so `get_raw_ptr` can hand out a `*mut T`
/// whose provenance is the cell's interior — writing through it is legal —
/// rather than laundering a `*mut` out of a shared `&T`, which is UB (A3). The
/// scheduler serializes mutable access per resource via `AccessDescriptor`, so
/// the interior mutation never actually aliases.
#[repr(transparent)]
struct ResourceCell(UnsafeCell<Box<dyn Any + Send + Sync>>);

// SAFETY: the inner value is `Send + Sync`; concurrent access is serialized by
// the scheduler's AccessDescriptor discipline (two systems mutating the same
// resource never share a stage), so exposing `Sync` is sound.
unsafe impl Sync for ResourceCell {}

pub struct Resources {
    data: FxHashMap<TypeId, ResourceCell>,
}

impl Resources {
    pub fn new() -> Self {
        Self {
            data: FxHashMap::default(),
        }
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.data.insert(
            TypeId::of::<T>(),
            ResourceCell(UnsafeCell::new(Box::new(value))),
        );
    }

    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &T {
        self.try_get::<T>().unwrap_or_else(|| {
            panic!(
                "Resource `{}` not found. Did you forget insert_resource()?",
                std::any::type_name::<T>()
            )
        })
    }

    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut T {
        self.try_get_mut::<T>().unwrap_or_else(|| {
            panic!(
                "Resource `{}` not found. Did you forget insert_resource()?",
                std::any::type_name::<T>()
            )
        })
    }

    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        let cell = self.data.get(&TypeId::of::<T>())?;
        // SAFETY: shared read of the cell interior; the scheduler guarantees no
        // concurrent mutable access to this resource.
        unsafe { (*cell.0.get()).downcast_ref::<T>() }
    }

    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        // `&mut self` — exclusive; take the interior mutably without unsafe.
        let cell = self.data.get_mut(&TypeId::of::<T>())?;
        cell.0.get_mut().downcast_mut::<T>()
    }

    /// Получить raw mutable pointer на ресурс.
    ///
    /// Используется `SystemContext::resource_mut` для параллельного доступа.
    ///
    /// # Safety
    /// Вызывающий код должен гарантировать что только одна система
    /// в данный момент держит мутабельный доступ к T.
    /// Планировщик обеспечивает это через `AccessDescriptor`.
    pub fn get_raw_ptr<T: Send + Sync + 'static>(&self) -> Option<*mut T> {
        let cell = self.data.get(&TypeId::of::<T>())?;
        // SAFETY: the `*mut` provenance is the UnsafeCell interior (via `get()`),
        // so writing through it is legal (A3). The scheduler guarantees exclusive
        // access to this resource while the pointer is live.
        let boxed: &mut Box<dyn Any + Send + Sync> = unsafe { &mut *cell.0.get() };
        boxed.downcast_mut::<T>().map(|r| r as *mut T)
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.data
            .remove(&TypeId::of::<T>())
            .and_then(|cell| cell.0.into_inner().downcast::<T>().ok().map(|b| *b))
    }

    #[inline]
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.data.contains_key(&TypeId::of::<T>())
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gravity(f32);
    struct Score(u32);

    #[test]
    fn insert_get() {
        let mut map = Resources::new();
        map.insert(Gravity(9.8));
        assert_eq!(map.get::<Gravity>().0, 9.8);
    }

    #[test]
    fn get_mut() {
        let mut map = Resources::new();
        map.insert(Score(0));
        map.get_mut::<Score>().0 += 10;
        assert_eq!(map.get::<Score>().0, 10);
    }

    #[test]
    fn try_get_missing() {
        let map = Resources::new();
        assert!(map.try_get::<Gravity>().is_none());
    }

    #[test]
    fn remove() {
        let mut map = Resources::new();
        map.insert(Score(42));
        assert!(map.contains::<Score>());
        map.remove::<Score>();
        assert!(!map.contains::<Score>());
    }

    #[test]
    fn get_raw_ptr() {
        let mut map = Resources::new();
        map.insert(Score(10));
        let ptr = map.get_raw_ptr::<Score>().unwrap();
        // SAFETY: тест — единственный владелец
        unsafe {
            (*ptr).0 = 99;
        }
        assert_eq!(map.get::<Score>().0, 99);
    }

    #[test]
    #[should_panic(expected = "not found")]
    fn get_panics_if_missing() {
        let map = Resources::new();
        let _ = map.get::<Gravity>();
    }
}
