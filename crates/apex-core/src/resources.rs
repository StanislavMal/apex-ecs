use rustc_hash::FxHashMap;
/// Resources — the world's global singletons.
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;

use crate::archetype::TickCell;
use crate::component::Tick;

/// A resource slot: the value behind `UnsafeCell` so `get_raw_parts` can hand
/// out a `*mut T` whose provenance is the cell's interior — writing through it
/// is legal — rather than laundering a `*mut` out of a shared `&T`, which is UB
/// (A3). The scheduler serializes mutable access per resource via
/// `AccessDescriptor`, so the interior mutation never actually aliases.
///
/// `changed` is the resource's change tick (RT-1, same discipline as component
/// columns): stamped on insert, on `&mut` acquisition through the world, and on
/// `ResMut::deref_mut` (lazy, A13-style). Read via
/// [`Resources::changed_tick`] / `World::resource_changed_tick`.
struct ResourceCell {
    value: UnsafeCell<Box<dyn Any + Send + Sync>>,
    changed: TickCell,
}

// SAFETY: the inner value is `Send + Sync`; concurrent access is serialized by
// the scheduler's AccessDescriptor discipline (two systems mutating the same
// resource never share a stage), so exposing `Sync` is sound. The tick cell
// follows the same discipline as component-column ticks.
unsafe impl Sync for ResourceCell {}

/// E7: serialize/deserialize fns for a resource type registered for snapshots.
#[derive(Clone)]
pub struct ResourceSerdeFns {
    pub type_name: &'static str,
    /// Serialize the resource if it is currently present.
    pub serialize: fn(&Resources) -> Option<Vec<u8>>,
    /// Deserialize bytes and insert the resource (stamping `tick` — a restore
    /// IS a change for change-detection consumers, RT-1).
    pub deserialize: fn(&mut Resources, &[u8], Tick) -> Result<(), String>,
}

/// RT-2: name-addressed JSON view of a resource for dynamic consumers (the
/// inspector class of tools, reflective UI bindings, scripting). The same
/// serde-tree idea as `ComponentSerdeFns` in its JSON flavor, read-only:
/// resources are SOURCES for reflective consumers; writes stay typed.
#[derive(Clone)]
pub struct ResourceReflectFns {
    pub type_name: &'static str,
    pub type_id: TypeId,
    /// Serialize the resource to a JSON tree if it is currently present.
    pub to_json: fn(&Resources) -> Option<Result<serde_json::Value, String>>,
}

fn ser_resource<R: serde::Serialize + Send + Sync + 'static>(res: &Resources) -> Option<Vec<u8>> {
    res.try_get::<R>().and_then(|r| bincode::serialize(r).ok())
}
fn de_resource<R: serde::de::DeserializeOwned + Send + Sync + 'static>(
    res: &mut Resources,
    bytes: &[u8],
    tick: Tick,
) -> Result<(), String> {
    let r: R = bincode::deserialize(bytes).map_err(|e| e.to_string())?;
    res.insert(r, tick);
    Ok(())
}
fn reflect_resource<R: serde::Serialize + Send + Sync + 'static>(
    res: &Resources,
) -> Option<Result<serde_json::Value, String>> {
    res.try_get::<R>()
        .map(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
}

pub struct Resources {
    data: FxHashMap<TypeId, ResourceCell>,
    /// E7: resource types opted into snapshots (`register_serde`).
    serde: FxHashMap<TypeId, ResourceSerdeFns>,
    /// RT-2: resource types opted into the name-addressed JSON view.
    reflect: FxHashMap<TypeId, ResourceReflectFns>,
}

impl Resources {
    pub fn new() -> Self {
        Self {
            data: FxHashMap::default(),
            serde: FxHashMap::default(),
            reflect: FxHashMap::default(),
        }
    }

    /// E7: opt a resource type into snapshots (bincode). Present resources of
    /// this type are then included by [`snapshot_serde`](Self::snapshot_serde)
    /// and restored by [`restore_serde`](Self::restore_serde).
    pub fn register_serde<R: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static>(
        &mut self,
    ) {
        self.serde.insert(
            TypeId::of::<R>(),
            ResourceSerdeFns {
                type_name: std::any::type_name::<R>(),
                serialize: ser_resource::<R>,
                deserialize: de_resource::<R>,
            },
        );
    }

    /// RT-2: opt a resource type into the name-addressed JSON view (read-only
    /// reflection for dynamic consumers). Independent of the snapshot registry:
    /// a resource may be reflectable without being snapshot-persisted.
    pub fn register_reflect<R: serde::Serialize + Send + Sync + 'static>(&mut self) {
        self.reflect.insert(
            TypeId::of::<R>(),
            ResourceReflectFns {
                type_name: std::any::type_name::<R>(),
                type_id: TypeId::of::<R>(),
                to_json: reflect_resource::<R>,
            },
        );
    }

    /// RT-2: resolve a registered reflectable resource by name — the full
    /// `type_name`, or its unambiguous last `::`-segment (`"Inventory"` for
    /// `"game::items::Inventory"`). `None` when unknown OR ambiguous (two
    /// registered resources sharing a short name must be addressed fully).
    pub fn reflect_by_name(&self, name: &str) -> Option<&ResourceReflectFns> {
        if let Some(fns) = self.reflect.values().find(|f| f.type_name == name) {
            return Some(fns);
        }
        let mut found: Option<&ResourceReflectFns> = None;
        for fns in self.reflect.values() {
            if fns.type_name.rsplit("::").next() == Some(name) {
                if found.is_some() {
                    return None; // ambiguous short name
                }
                found = Some(fns);
            }
        }
        found
    }

    /// E7: `(type_name, bytes)` for every registered resource that is present.
    pub fn snapshot_serde(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for fns in self.serde.values() {
            if let Some(bytes) = (fns.serialize)(self) {
                out.push((fns.type_name.to_string(), bytes));
            }
        }
        out
    }

    /// E7: deserialize+insert a resource by `type_name`, stamping `tick` as its
    /// change tick (a restore is a change, RT-1). `Ok(false)` if that type_name
    /// was never registered (unknown resource — caller may warn).
    pub fn restore_serde(&mut self, type_name: &str, bytes: &[u8], tick: Tick) -> Result<bool, String> {
        let fns = self.serde.values().find(|f| f.type_name == type_name).cloned();
        match fns {
            Some(f) => {
                (f.deserialize)(self, bytes, tick)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Insert a resource, stamping `tick` as its change tick (RT-1).
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T, tick: Tick) {
        self.data.insert(
            TypeId::of::<T>(),
            ResourceCell {
                value: UnsafeCell::new(Box::new(value)),
                changed: TickCell::new(tick),
            },
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

    /// Exclusive access, stamping `tick` (acquisition through `&mut self` is
    /// presumed a write — the precise lazy path is `ResMut::deref_mut`).
    #[track_caller]
    pub fn get_mut<T: Send + Sync + 'static>(&mut self, tick: Tick) -> &mut T {
        self.try_get_mut::<T>(tick).unwrap_or_else(|| {
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
        unsafe { (*cell.value.get()).downcast_ref::<T>() }
    }

    /// See [`get_mut`](Self::get_mut) for the stamping contract.
    pub fn try_get_mut<T: Send + Sync + 'static>(&mut self, tick: Tick) -> Option<&mut T> {
        // `&mut self` — exclusive; take the interior mutably without unsafe.
        let cell = self.data.get_mut(&TypeId::of::<T>())?;
        let r = cell.value.get_mut().downcast_mut::<T>();
        if r.is_some() {
            *cell.changed.get_mut() = tick;
        }
        r
    }

    /// The resource's change tick (RT-1); `None` when absent.
    pub fn changed_tick<T: Send + Sync + 'static>(&self) -> Option<Tick> {
        self.changed_tick_by_type(&TypeId::of::<T>())
    }

    /// [`changed_tick`](Self::changed_tick) by `TypeId` (dynamic consumers).
    pub fn changed_tick_by_type(&self, type_id: &TypeId) -> Option<Tick> {
        self.data.get(type_id).map(|cell| cell.changed.get())
    }

    /// Get raw parts of a resource: value pointer + change-tick cell.
    ///
    /// Used by `SystemContext::resource_mut` for parallel access; the tick cell
    /// lets `ResMut` stamp lazily on `deref_mut` (A13 discipline).
    ///
    /// # Safety
    /// The calling code must guarantee that only one system holds mutable access
    /// to T at any given time. The scheduler ensures this via `AccessDescriptor`.
    pub(crate) fn get_raw_parts<T: Send + Sync + 'static>(
        &self,
    ) -> Option<(*mut T, *const TickCell)> {
        let cell = self.data.get(&TypeId::of::<T>())?;
        // SAFETY: the `*mut` provenance is the UnsafeCell interior (via `get()`),
        // so writing through it is legal (A3). The scheduler guarantees exclusive
        // access to this resource while the pointer is live.
        let boxed: &mut Box<dyn Any + Send + Sync> = unsafe { &mut *cell.value.get() };
        boxed
            .downcast_mut::<T>()
            .map(|r| (r as *mut T, &cell.changed as *const TickCell))
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.data
            .remove(&TypeId::of::<T>())
            .and_then(|cell| cell.value.into_inner().downcast::<T>().ok().map(|b| *b))
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

    /// Clamp stale resource change ticks against the `MAX_CHANGE_AGE` window
    /// (mirrors the column-tick clamp in `World::check_change_ticks`).
    pub(crate) fn check_change_ticks(&mut self, current: Tick) {
        for cell in self.data.values_mut() {
            cell.changed.get_mut().check_against(current);
        }
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
        map.insert(Gravity(9.8), Tick(1));
        assert_eq!(map.get::<Gravity>().0, 9.8);
    }

    #[test]
    fn get_mut() {
        let mut map = Resources::new();
        map.insert(Score(0), Tick(1));
        map.get_mut::<Score>(Tick(2)).0 += 10;
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
        map.insert(Score(42), Tick(1));
        assert!(map.contains::<Score>());
        map.remove::<Score>();
        assert!(!map.contains::<Score>());
    }

    #[test]
    fn get_raw_parts() {
        let mut map = Resources::new();
        map.insert(Score(10), Tick(1));
        let (ptr, tick) = map.get_raw_parts::<Score>().unwrap();
        // SAFETY: the test is the sole owner
        unsafe {
            (*ptr).0 = 99;
            (*tick).set(Tick(7));
        }
        assert_eq!(map.get::<Score>().0, 99);
        assert_eq!(map.changed_tick::<Score>(), Some(Tick(7)));
    }

    #[test]
    #[should_panic(expected = "not found")]
    fn get_panics_if_missing() {
        let map = Resources::new();
        let _ = map.get::<Gravity>();
    }

    // ── RT-1: change ticks ────────────────────────────────────

    #[test]
    fn insert_stamps_tick() {
        let mut map = Resources::new();
        map.insert(Score(1), Tick(5));
        assert_eq!(map.changed_tick::<Score>(), Some(Tick(5)));
        assert_eq!(map.changed_tick::<Gravity>(), None);
    }

    #[test]
    fn get_mut_stamps_tick_only_when_present() {
        let mut map = Resources::new();
        map.insert(Score(1), Tick(1));
        let _ = map.try_get_mut::<Score>(Tick(9));
        assert_eq!(map.changed_tick::<Score>(), Some(Tick(9)));
        // A missing resource must not materialize a phantom tick.
        assert!(map.try_get_mut::<Gravity>(Tick(9)).is_none());
        assert_eq!(map.changed_tick::<Gravity>(), None);
    }

    #[test]
    fn shared_read_does_not_stamp() {
        let mut map = Resources::new();
        map.insert(Score(1), Tick(3));
        let _ = map.get::<Score>();
        let _ = map.try_get::<Score>();
        assert_eq!(map.changed_tick::<Score>(), Some(Tick(3)));
    }

    // ── RT-2: JSON reflection ─────────────────────────────────

    #[derive(serde::Serialize)]
    struct Inventory {
        items: Vec<String>,
    }

    #[test]
    fn reflect_by_full_and_short_name() {
        let mut map = Resources::new();
        map.register_reflect::<Inventory>();
        map.insert(
            Inventory { items: vec!["sword".into()] },
            Tick(1),
        );
        let full = std::any::type_name::<Inventory>();
        for name in [full, "Inventory"] {
            let fns = map.reflect_by_name(name).expect("resolved");
            let value = (fns.to_json)(&map).expect("present").expect("serialized");
            assert_eq!(value.pointer("/items/0").and_then(|v| v.as_str()), Some("sword"));
        }
        assert!(map.reflect_by_name("NoSuch").is_none());
    }

    #[test]
    fn reflect_absent_resource_is_none() {
        let mut map = Resources::new();
        map.register_reflect::<Inventory>();
        let fns = map.reflect_by_name("Inventory").expect("registered");
        assert!((fns.to_json)(&map).is_none(), "registered but not inserted");
    }
}
