use std::any::TypeId;
use rustc_hash::FxHashMap;

/// Уникальный ID компонента
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ComponentId(pub(crate) u32);

/// Тик мира — монотонно растущий счётчик, инкрементируется каждый раз
/// когда World::tick() вызывается (обычно раз в кадр).
/// Используется для change detection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Tick(pub u32);

impl Tick {
    pub const ZERO: Self = Self(0);

    #[inline]
    pub fn is_newer_than(self, last_run: Tick) -> bool {
        self.0 > last_run.0
    }
}

/// Метаданные компонента
pub struct ComponentInfo {
    pub id: ComponentId,
    pub name: &'static str,
    pub type_id: TypeId,
    pub size: usize,
    pub align: usize,
    pub drop_fn: unsafe fn(*mut u8),
}

/// Трейт для всех компонентов
pub trait Component: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> Component for T {}

unsafe fn drop_ptr<T>(ptr: *mut u8) {
    ptr.cast::<T>().drop_in_place();
}

/// Глобальный реестр компонентов
pub struct ComponentRegistry {
    type_to_id: FxHashMap<TypeId, ComponentId>,
    components: Vec<ComponentInfo>,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            type_to_id: FxHashMap::default(),
            components: Vec::new(),
        }
    }

    pub fn register<T: Component>(&mut self) -> ComponentId {
        let type_id = TypeId::of::<T>();
        if let Some(&id) = self.type_to_id.get(&type_id) {
            return id;
        }
        let id = ComponentId(self.components.len() as u32);
        self.components.push(ComponentInfo {
            id,
            name: std::any::type_name::<T>(),
            type_id,
            size: std::mem::size_of::<T>(),
            align: std::mem::align_of::<T>(),
            drop_fn: drop_ptr::<T>,
        });
        self.type_to_id.insert(type_id, id);
        id
    }

    pub fn get_id<T: Component>(&self) -> Option<ComponentId> {
        self.type_to_id.get(&TypeId::of::<T>()).copied()
    }

    pub fn get_or_register<T: Component>(&mut self) -> ComponentId {
        self.register::<T>()
    }

    pub fn get_info(&self, id: ComponentId) -> Option<&ComponentInfo> {
        self.components.get(id.0 as usize)
    }

    pub fn len(&self) -> usize {
        self.components.len()
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self { Self::new() }
}
