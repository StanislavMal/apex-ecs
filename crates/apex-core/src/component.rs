use std::any::TypeId;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ComponentId(pub(crate) u32);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Tick(pub u32);

impl Tick {
    pub const ZERO: Self = Self(0);

    #[inline]
    pub fn is_newer_than(self, last_run: Tick) -> bool {
        self.0 > last_run.0
    }
}

pub struct ComponentInfo {
    pub id: ComponentId,
    pub name: &'static str,
    pub type_id: TypeId,
    pub size: usize,
    pub align: usize,
    pub drop_fn: unsafe fn(*mut u8),
}

pub trait Component: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> Component for T {}

pub(crate) unsafe fn drop_ptr<T>(ptr: *mut u8) {
    ptr.cast::<T>().drop_in_place();
}

pub struct ComponentRegistry {
    type_to_id: FxHashMap<TypeId, ComponentId>,
    /// Индексированы по ComponentId.0 — но для relation ID могут быть разреженными.
    /// Используем HashMap вместо Vec для поддержки произвольных ID.
    by_id: FxHashMap<u32, ComponentInfo>,
    next_id: u32,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            type_to_id: FxHashMap::default(),
            by_id: FxHashMap::default(),
            next_id: 0,
        }
    }

    pub fn register<T: Component>(&mut self) -> ComponentId {
        let type_id = TypeId::of::<T>();
        if let Some(&id) = self.type_to_id.get(&type_id) {
            return id;
        }
        let id = ComponentId(self.next_id);
        self.next_id += 1;
        self.by_id.insert(id.0, ComponentInfo {
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

    /// Зарегистрировать компонент с заранее известным ID (для relations).
    /// Если ID уже зарегистрирован — ничего не делает.
    pub fn register_raw(&mut self, id: ComponentId, info: ComponentInfo) {
        self.by_id.entry(id.0).or_insert(info);
    }

    pub fn get_id<T: Component>(&self) -> Option<ComponentId> {
        self.type_to_id.get(&TypeId::of::<T>()).copied()
    }

    pub fn get_or_register<T: Component>(&mut self) -> ComponentId {
        self.register::<T>()
    }

    pub fn get_info(&self, id: ComponentId) -> Option<&ComponentInfo> {
        self.by_id.get(&id.0)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self { Self::new() }
}
