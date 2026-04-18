/// Resources — глобальные синглтоны мира.
///
/// Хранятся как type-erased Box<dyn Any + Send + Sync> в FxHashMap по TypeId.
/// Доступ O(1), нет аллокаций на горячем пути (кроме первичной вставки).
///
/// # Пример
/// ```ignore
/// world.insert_resource(GameConfig { gravity: 9.8 });
/// let cfg = world.resource::<GameConfig>();
/// world.resource_mut::<GameConfig>().gravity = 0.0;
/// world.remove_resource::<GameConfig>();
/// ```

use std::any::{Any, TypeId};
use rustc_hash::FxHashMap;

// ── Внутренний trait объект ────────────────────────────────────

trait ResourceStorage: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any + Send + Sync> ResourceStorage for T {
    fn as_any(&self) -> &dyn Any { self }
    fn as_any_mut(&mut self) -> &mut dyn Any { self }
}

// ── ResourceMap ────────────────────────────────────────────────

pub struct ResourceMap {
    data: FxHashMap<TypeId, Box<dyn ResourceStorage>>,
}

impl ResourceMap {
    pub fn new() -> Self {
        Self { data: FxHashMap::default() }
    }

    /// Вставить ресурс (перезаписывает если уже есть).
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.data.insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Получить ссылку на ресурс. Паникует если ресурс не зарегистрирован.
    #[track_caller]
    pub fn get<T: Send + Sync + 'static>(&self) -> &T {
        self.try_get::<T>().unwrap_or_else(|| {
            panic!(
                "Resource `{}` not found. Did you forget to insert_resource()?",
                std::any::type_name::<T>()
            )
        })
    }

    /// Получить мутабельную ссылку. Паникует если ресурс не зарегистрирован.
    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> &mut T {
        self.try_get_mut::<T>().unwrap_or_else(|| {
            panic!(
                "Resource `{}` not found. Did you forget to insert_resource()?",
                std::any::type_name::<T>()
            )
        })
    }

    /// Попытаться получить ресурс — None если не существует.
    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.data
            .get(&TypeId::of::<T>())
            .and_then(|b| b.as_any().downcast_ref::<T>())
    }

    /// Попытаться получить мутабельную ссылку — None если не существует.
    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.data
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.as_any_mut().downcast_mut::<T>())
    }

    /// Удалить ресурс. Возвращает Some(T) если ресурс существовал.
    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.data
            .remove(&TypeId::of::<T>())
            .and_then(|b| {
                // Downcasting из Box<dyn Trait> требует промежуточного шага
                let raw: Box<dyn Any + Send + Sync> = unsafe {
                    // SAFETY: ResourceStorage реализован для T: Any + Send + Sync,
                    // и мы знаем что TypeId совпадает.
                    std::mem::transmute(b)
                };
                raw.downcast::<T>().ok().map(|b| *b)
            })
    }

    /// Проверить наличие ресурса.
    #[inline]
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.data.contains_key(&TypeId::of::<T>())
    }

    pub fn len(&self) -> usize { self.data.len() }
    pub fn is_empty(&self) -> bool { self.data.is_empty() }
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
    #[should_panic(expected = "Resource `apex_core::resources::tests::Gravity` not found")]
    fn get_panics_if_missing() {
        let map = ResourceMap::new();
        let _ = map.get::<Gravity>();
    }
}