#[cfg(all(not(target_arch = "wasm32"), not(miri)))]
use linkme::distributed_slice;
use rustc_hash::FxHashMap;
use std::any::TypeId;

/// Registrar function type: takes a mutable registry, registers a component.
pub type ComponentRegistrarFn = fn(&mut ComponentRegistry);

/// Global list of all component auto-registrars.
/// Populated by the linker from every crate that uses #[derive(Component)].
#[cfg(all(not(target_arch = "wasm32"), not(miri)))]
#[distributed_slice]
pub static COMPONENT_REGISTRARS: [ComponentRegistrarFn] = [..];

/// On wasm32 (and under Miri) `linkme::distributed_slice` is unavailable —
/// auto-registration on `World::new()` does not run, components are registered
/// lazily (`get_or_register` on spawn/insert). Miri: linkme's distributed-slice
/// address arithmetic overflows under Miri, so the same lazy path is used —
/// components still register on first use (only `#[require]` pre-application is
/// skipped, like wasm). ⚠ Consequence: `#[require(...)]` on wasm is not yet
/// applied (derive registrars do not run) — TD-25 in
/// `apex-engine/plans/TECH_DEBT.md`, to be closed before the first wasm runtime (plan:
/// lazy require via a Component trait method).
#[cfg(any(target_arch = "wasm32", miri))]
pub static COMPONENT_REGISTRARS: [ComponentRegistrarFn; 0] = [];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ComponentId(pub u32);

impl ComponentId {
    /// Sentinel meaning "component not registered" — used by optional query
    /// forms (`Maybe`/`MaybeWrite`) and `Or<>` branches to PRESERVE the
    /// ALIGNMENT of the ids list against `component_count()` (otherwise components after
    /// an unregistered one would read someone else's id — a latent bug until W2).
    /// No real component ever gets this id (`next_id` grows from 0).
    pub const INVALID: Self = Self(u32::MAX);
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Tick(pub u32);

impl Tick {
    pub const ZERO: Self = Self(0);

    /// Maximum "age" of a change tick relative to the world's current tick.
    ///
    /// `is_newer_than` is a wrapping comparison, correct when the difference < 2³¹.
    /// A row unchanged for longer would "wrap around" into a false Changed after
    /// `2³¹ / ticks-per-second` of uptime — the tick advances once per scheduler
    /// STAGE plus the frame boundary, so e.g. a 7-stage schedule @60 FPS runs
    /// ~480 ticks/s ⇒ ≈ 52 days (PE-C7). A periodic clamp
    /// ([`World::check_change_ticks`](crate::World::check_change_ticks))
    /// pulls old ticks up to this age, preserving the invariant (W2-3).
    pub const MAX_CHANGE_AGE: u32 = 1 << 30;

    #[inline]
    pub fn is_newer_than(self, last_run: Tick) -> bool {
        self.0.wrapping_sub(last_run.0) as i32 > 0
    }

    /// Pull the tick up to the `MAX_CHANGE_AGE` window from `current` if it is older.
    /// Returns `true` if the clamp happened.
    #[inline]
    pub fn check_against(&mut self, current: Tick) -> bool {
        if current.0.wrapping_sub(self.0) > Self::MAX_CHANGE_AGE {
            self.0 = current.0.wrapping_sub(Self::MAX_CHANGE_AGE);
            true
        } else {
            false
        }
    }
}

// ── Component serialization ────────────────────────────────────

/// Result of serializing a single component — bytes in the chosen format.
pub type SerializeResult = Result<Vec<u8>, ComponentSerdeError>;
pub type DeserializeResult = Result<Vec<u8>, ComponentSerdeError>;

#[derive(Debug, Clone)]
pub enum ComponentSerdeError {
    SerializationFailed(String),
    DeserializationFailed(String),
    FormatMismatch { expected: &'static str },
}

impl std::fmt::Display for ComponentSerdeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SerializationFailed(s) => write!(f, "serialize failed: {}", s),
            Self::DeserializationFailed(s) => write!(f, "deserialize failed: {}", s),
            Self::FormatMismatch { expected } => {
                write!(f, "format mismatch, expected {}", expected)
            }
        }
    }
}

/// Table of serialization functions stored in `ComponentInfo`.
///
/// Split into a separate struct so that `ComponentInfo` stays `Copy`-friendly
/// where needed, while the fn-pointers themselves are available optionally.
///
/// # Safety
/// Both functions operate on the component's raw bytes:
/// - `serialize_fn(src_ptr)` — reads T from `src_ptr`, returns bytes (JSON/bincode/RON)
/// - `deserialize_fn(bytes)`  — takes bytes, returns aligned bytes of T
///   suitable for writing into a Column via `write_component`.
///
/// The caller must guarantee that `src_ptr` points to a live T of the correct type.
/// An opaque **(de)serialization context**, threaded into [`ComponentSerdeFns`] (TD-44). The core does
/// NOT know its contents — the consumer (engine/editor) implements its own type and **downcasts** via
/// [`as_any_mut`](SerdeContext::as_any_mut). This way a component holding an external reference (an asset
/// Handle, an Entity reference during remap) resolves it during (de)serialization, while apex-ecs stays
/// **self-contained** — NOT A SINGLE engine/asset type in the core signatures. Ordinary components ignore the context.
pub trait SerdeContext: std::any::Any {
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// The default empty context — for ordinary (de)serialization without resolving external references. The old
/// `WorldSerializer::snapshot/restore` use it, so existing components are unaffected.
pub struct NoContext;
impl SerdeContext for NoContext {
    #[inline]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    #[inline]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[derive(Clone)]
pub struct ComponentSerdeFns {
    /// Serialize the component from a raw pointer into bytes. `ctx` is an optional external-reference resolver
    /// (ordinary components ignore it; context-dependent ones downcast, see [`SerdeContext`]).
    pub serialize_fn: unsafe fn(*const u8, &mut dyn SerdeContext) -> SerializeResult,
    /// Deserialize bytes back into an aligned buffer holding T's data (with the same `ctx`).
    pub deserialize_fn: fn(&[u8], &mut dyn SerdeContext) -> DeserializeResult,
    /// Human-readable format name: "json", "bincode", "ron".
    pub format: &'static str,
}

/// Remap of `Entity` references INSIDE a component (E6, Bevy's MapEntities analog). After
/// a snapshot `restore` all old `Entity`s are recreated with NEW ids; a component
/// holding an `Entity` (e.g. `Target(Entity)`) must update its references, otherwise
/// they point into the void. Called by the SECOND restore pass, when the
/// old→new map is complete (including forward references).
///
/// # Safety
/// `ptr` is a valid `*mut T` of a live component of this type; `f` is applied to
/// each `Entity` reference and returns its new id.
pub type MapEntitiesFn = unsafe fn(*mut u8, f: &mut dyn FnMut(crate::Entity) -> crate::Entity);

/// A component holding `Entity` references that must be remapped on snapshot
/// restore (E6). Registered via
/// [`World::register_map_entities`](crate::World::register_map_entities).
///
/// ```ignore
/// struct Target(Entity);
/// impl MapEntities for Target {
///     fn map_entities(&mut self, f: &mut dyn FnMut(Entity) -> Entity) {
///         self.0 = f(self.0);
///     }
/// }
/// ```
pub trait MapEntities {
    fn map_entities(&mut self, f: &mut dyn FnMut(crate::Entity) -> crate::Entity);
}

// ── Component hooks (W3-1) ─────────────────────────────────────

/// Composition hook: called AFTER a structural operation completes, when the world
/// is consistent (`on_add` — the component is already on the entity; `on_remove` — the
/// component is already gone; on `despawn` the entity is already dead — `is_alive` returns `false`).
///
/// The hook receives `&mut World` and may perform any operation, including
/// structural ones — nested hooks are enqueued into the same queue and processed
/// by the same dispatcher (no recursion). Only non-capturing functions
/// (fn-pointers): the subscriber's state lives in resources/components.
/// One hook per component per event kind; for multiple subscribers
/// use events ([`World::track_removals`](crate::World::track_removals)).
pub type ComponentHookFn = fn(&mut crate::World, crate::Entity);

/// Emitter of the `Removed<T>` event — a monomorphized fn-pointer,
/// installed by `World::track_removals::<T>()`.
pub(crate) type EmitRemovedFn = fn(&mut crate::events::EventRegistry, crate::Entity);

/// `ComponentRegistry::flags` bits — a fast "are there any subscribers" check without
/// a hashmap lookup on hot structural paths.
pub(crate) const FLAG_ON_ADD: u8 = 1;
pub(crate) const FLAG_ON_REMOVE: u8 = 2;
pub(crate) const FLAG_TRACK_REMOVED: u8 = 4;
/// The component has required components (D2-4, `#[require(...)]`).
pub(crate) const FLAG_REQUIRES: u8 = 8;
/// Mask "the appearance of the component is of interest to someone" (requires + on_add).
pub(crate) const ADDED_NOTIFY_MASK: u8 = FLAG_ON_ADD | FLAG_REQUIRES;

/// Insertion of a missing required component (D2-4): a no-op if `R` is already on
/// the entity (an explicit value from spawn/bundle ALWAYS wins over the default).
pub(crate) type RequiredInsertFn = fn(&mut crate::World, crate::Entity);

#[derive(Default, Clone, Copy)]
pub(crate) struct ComponentHooks {
    pub on_add: Option<ComponentHookFn>,
    pub on_remove: Option<ComponentHookFn>,
    pub emit_removed: Option<EmitRemovedFn>,
}

// ── ComponentInfo ──────────────────────────────────────────────

pub struct ComponentInfo {
    pub id: ComponentId,
    pub name: &'static str,
    pub type_id: TypeId,
    pub size: usize,
    pub align: usize,
    pub drop_fn: unsafe fn(*mut u8),
    /// Serialization functions — `None` if the component is not marked as Serializable.
    /// Populated by a call to `register_component_serde::<T>()`.
    pub serde: Option<ComponentSerdeFns>,
    /// Remap of Entity references on restore (E6) — `None` if the component does not
    /// hold any. Populated by `register_map_entities::<T>()`.
    pub map_entities: Option<MapEntitiesFn>,
}

// ── Component trait ────────────────────────────────────────────

pub trait Component: Send + Sync + 'static {
    /// Registration of **required components** (`#[require(...)]`, D2-4). The default is no requirements;
    /// `#[derive(Component)]` overrides it, calling `register_required::<Self, R>()` for each
    /// `R`. Called from [`ComponentRegistry::register`] EXACTLY ONCE on the type's first registration —
    /// therefore `#[require]` works on **ALL** platforms (including wasm) via lazy registration,
    /// **without** `linkme`/linker magic: natively the requirements are registered by the distributed-slice registrar
    /// (via `register`), on wasm by the component's first use (also via `register`). TD-25.
    fn register_requires(_registry: &mut ComponentRegistry) {}
}

#[cfg(feature = "cgmath")]
impl Component for cgmath::Matrix4<f32> {}

/// Marker: the component can be serialized/deserialized.
///
/// # Example
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// struct Position { x: f32, y: f32 }
///
/// // Registration:
/// world.register_component_serde::<Position>();
/// ```
///
/// Components without this marker (PhysicsHandle, RenderMesh, …) are skipped
/// during a snapshot — that is fine, they are meant to be recreated from the serialized state.
pub trait Serializable: Component + serde::Serialize + for<'de> serde::Deserialize<'de> {}
impl<T> Serializable for T where T: Component + serde::Serialize + for<'de> serde::Deserialize<'de> {}

// ── Drop helper ────────────────────────────────────────────────

pub(crate) unsafe fn drop_ptr<T>(ptr: *mut u8) {
    ptr.cast::<T>().drop_in_place();
}

// ── serde fn implementation for a concrete T ──────────────────

/// Creates `ComponentSerdeFns` for a type T implementing `Serializable`.
///
/// Internally uses `bincode` as a compact binary format.
/// The format can be changed — it is enough to swap the implementation of the two closures.
pub fn make_serde_fns<T: Serializable>() -> ComponentSerdeFns {
    ComponentSerdeFns {
        serialize_fn: |ptr, _ctx| {
            // SAFETY: the caller guarantees the validity of ptr as *const T
            let val = unsafe { &*(ptr as *const T) };
            bincode::serialize(val)
                .map_err(|e| ComponentSerdeError::SerializationFailed(e.to_string()))
        },
        deserialize_fn: |bytes, _ctx| {
            let val: T = bincode::deserialize(bytes)
                .map_err(|e| ComponentSerdeError::DeserializationFailed(e.to_string()))?;
            // Pack T into an aligned byte buffer for writing into a Column.
            let size = std::mem::size_of::<T>();
            let mut buf = vec![0u8; size];
            if size > 0 {
                // SAFETY: buf is large enough, T: Copy-compatible via serde
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &val as *const T as *const u8,
                        buf.as_mut_ptr(),
                        size,
                    );
                }
            }
            std::mem::forget(val);
            Ok(buf)
        },
        format: "bincode",
    }
}

/// Creates `ComponentSerdeFns` for a type T implementing `Serializable`
/// using `serde_json` (a text format for debugging/logs).
pub fn make_serde_fns_json<T: Serializable>() -> ComponentSerdeFns {
    ComponentSerdeFns {
        serialize_fn: |ptr, _ctx| {
            let val = unsafe { &*(ptr as *const T) };
            serde_json::to_vec(val)
                .map_err(|e| ComponentSerdeError::SerializationFailed(e.to_string()))
        },
        deserialize_fn: |bytes, _ctx| {
            let val: T = serde_json::from_slice(bytes)
                .map_err(|e| ComponentSerdeError::DeserializationFailed(e.to_string()))?;
            let size = std::mem::size_of::<T>();
            let mut buf = vec![0u8; size];
            if size > 0 {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &val as *const T as *const u8,
                        buf.as_mut_ptr(),
                        size,
                    );
                }
            }
            std::mem::forget(val);
            Ok(buf)
        },
        format: "json",
    }
}

// ── JSON deep merge (RT-2) ─────────────────────────────────────

/// Deep-merge a PARTIAL JSON value tree into `base`: objects merge key-by-key
/// recursively; any non-object (numbers, strings, arrays, enum-variant
/// switches) overwrites the base value at that spot.
///
/// The canonical "partial component edit" semantics shared by every dynamic
/// writer of the serde tree (the editor's `edit_setComponent`, script
/// `set_component`, reflective UI bindings): read the whole component to JSON,
/// merge the partial, write the whole component back.
pub fn json_merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(over_map)) => {
            for (k, v) in over_map {
                match base_map.get_mut(k) {
                    Some(slot) => json_merge(slot, v),
                    None => {
                        base_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, over) => *slot = over.clone(),
    }
}

// ── ComponentRegistry ──────────────────────────────────────────

pub struct ComponentRegistry {
    type_to_id: FxHashMap<TypeId, ComponentId>,
    /// Component metadata indexed by `ComponentId.0`. Ids are dense from 0 (allocated
    /// by `register`, one contiguous `push` per new type, never removed), so a `Vec`
    /// is the correct structure — O(1) indexed lookup, no hashing (§1.4: was an
    /// `FxHashMap<u32, _>`, a leftover from pre-CR-M1 arbitrary relation ids).
    by_id: Vec<ComponentInfo>,
    next_id: u32,
    /// Subscription bits per-ComponentId ([`FLAG_ON_ADD`]/[`FLAG_ON_REMOVE`]/
    /// [`FLAG_TRACK_REMOVED`]) — index = `ComponentId.0` (ids are dense).
    /// Hot structural paths first check [`any_flags`](Self::any_flags)
    /// (a single bool), then the bit — no hashmap lookups.
    flags: Vec<u8>,
    /// The hooks themselves — only for components with non-zero flags.
    hooks: FxHashMap<u32, ComponentHooks>,
    /// Required components per cid (D2-4): inserted by default if
    /// absent, AFTER the owning component appears (via the hook
    /// queue, before the user's on_add; transitivity comes naturally
    /// through the same queue).
    requires: FxHashMap<u32, Vec<RequiredInsertFn>>,
    any_flags: bool,
    /// Any component carries a [`MapEntitiesFn`] (E6) — the cheap gate of
    /// [`any_map_entities`](Self::any_map_entities).
    any_map_entities: bool,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            type_to_id: FxHashMap::default(),
            by_id: Vec::new(),
            next_id: 0,
            flags: Vec::new(),
            hooks: FxHashMap::default(),
            requires: FxHashMap::default(),
            any_flags: false,
            any_map_entities: false,
        }
    }

    /// Declare: component `C` requires `R` (D2-4, Bevy's
    /// `#[require(...)]` analog). When `C` APPEARS on an entity, a missing `R`
    /// is inserted as `R::default()` (an explicitly set value always wins).
    /// Called by the derive-macro registrar or manually
    /// ([`World::require_component`](crate::World::require_component)).
    pub fn register_required<C: Component, R: Component + Default>(&mut self) {
        let cid = self.register::<C>();
        self.register::<R>();
        self.requires
            .entry(cid.0)
            .or_default()
            .push(|world, entity| {
                if !world.has_component::<R>(entity) {
                    world.insert(entity, R::default());
                }
            });
        self.set_flag(cid, FLAG_REQUIRES);
    }

    /// The component's required inserts (D2-4); `None` — no requirements.
    #[inline]
    pub(crate) fn requires(&self, cid: ComponentId) -> Option<&[RequiredInsertFn]> {
        self.requires.get(&cid.0).map(|v| v.as_slice())
    }

    // ── Hooks (W3-1) ───────────────────────────────────────────

    /// Whether the world has at least one composition subscriber (fast-path gate).
    #[inline]
    pub(crate) fn any_flags(&self) -> bool {
        self.any_flags
    }

    /// The component's subscription bits (0 — no subscribers).
    #[inline]
    pub(crate) fn flags(&self, cid: ComponentId) -> u8 {
        self.flags.get(cid.0 as usize).copied().unwrap_or(0)
    }

    pub(crate) fn set_flag(&mut self, cid: ComponentId, flag: u8) {
        let idx = cid.0 as usize;
        if self.flags.len() <= idx {
            self.flags.resize(idx + 1, 0);
        }
        self.flags[idx] |= flag;
        self.any_flags = true;
    }

    #[inline]
    pub(crate) fn hooks(&self, cid: ComponentId) -> Option<&ComponentHooks> {
        self.hooks.get(&cid.0)
    }

    pub(crate) fn hooks_mut(&mut self, cid: ComponentId) -> &mut ComponentHooks {
        self.hooks.entry(cid.0).or_default()
    }

    /// Registers all components declared via `#[derive(Component)]`.
    /// Called once at World creation.
    ///
    /// Works via `linkme::distributed_slice` — the linker collects
    /// the static registrars from every crate that uses the macro.
    pub fn register_all_auto(&mut self) {
        for registrar in COMPONENT_REGISTRARS {
            registrar(self);
        }
    }

    /// Register a component without serialization.
    pub fn register<T: Component>(&mut self) -> ComponentId {
        let type_id = TypeId::of::<T>();
        if let Some(&id) = self.type_to_id.get(&type_id) {
            return id;
        }
        let id = ComponentId(self.next_id);
        self.next_id += 1;
        // Dense push: id.0 == the current length, so `by_id[id.0]` is this entry.
        debug_assert_eq!(self.by_id.len(), id.0 as usize, "component ids must stay dense");
        self.by_id.push(ComponentInfo {
            id,
            name: std::any::type_name::<T>(),
            type_id,
            size: std::mem::size_of::<T>(),
            align: std::mem::align_of::<T>(),
            drop_fn: drop_ptr::<T>,
            serde: None,
            map_entities: None,
        });
        self.type_to_id.insert(type_id, id);
        // Register the required components (`#[require]`) EXACTLY ONCE — here, on the type's first
        // registration (a re-entrant `register::<T>` from `register_required` returns early, since T is already
        // in `type_to_id`). Platform-independent: works even without a distributed-slice (wasm — TD-25).
        T::register_requires(self);
        id
    }

    /// Register a component with serialization support.
    ///
    /// If the component is already registered — only adds the serde functions,
    /// the ID and layout do not change.
    pub fn register_serde<T: Serializable>(&mut self) -> ComponentId {
        let id = self.register::<T>();
        if let Some(info) = self.by_id.get_mut(id.0 as usize) {
            if info.serde.is_none() {
                info.serde = Some(make_serde_fns::<T>());
            }
        }
        id
    }

    /// Register a component with **context-dependent** serde functions (TD-44): a component with
    /// an external reference (an asset Handle, an Entity reference) is (de)serialized via [`SerdeContext`],
    /// which is provided by `WorldSerializer::*_with` / `PrefabLoader`. Unlike [`register_serde`], it **always
    /// replaces** the serde functions (context-dependent ones take priority over the default bincode/json). The resolver itself lives in
    /// the engine/editor — the core stays asset-agnostic.
    pub fn register_serde_with<T: Component>(&mut self, fns: ComponentSerdeFns) -> ComponentId {
        let id = self.register::<T>();
        if let Some(info) = self.by_id.get_mut(id.0 as usize) {
            info.serde = Some(fns);
        }
        id
    }

    /// Set the component's [`MapEntitiesFn`] (E6). The component must already be
    /// registered (`id` from `get_or_register`).
    pub(crate) fn set_map_entities(&mut self, id: ComponentId, f: MapEntitiesFn) {
        if let Some(info) = self.by_id.get_mut(id.0 as usize) {
            info.map_entities = Some(f);
            self.any_map_entities = true;
        }
    }

    /// Does ANY registered component hold remappable `Entity` references (E6)?
    ///
    /// The cheap gate for a caller whose only alternative is sweeping every entity of the world to
    /// repair references — the editor's structural undo, which brings a deleted object back under a
    /// NEW handle and must point everything that referenced it at the new one. Most worlds register
    /// nothing of the kind, and this lets them skip the sweep instead of walking the whole world to
    /// discover there was nothing to do. The same shape as [`any_flags`](Self::any_flags): one bool
    /// raised at registration.
    #[inline]
    pub fn any_map_entities(&self) -> bool {
        self.any_map_entities
    }

    /// Register a component with serialization support (JSON format).
    ///
    /// If the component is already registered — only adds the serde functions,
    /// the ID and layout do not change.
    pub fn register_serde_json<T: Serializable>(&mut self) -> ComponentId {
        let id = self.register::<T>();
        if let Some(info) = self.by_id.get_mut(id.0 as usize) {
            if info.serde.is_none() {
                info.serde = Some(make_serde_fns_json::<T>());
            }
        }
        id
    }

    pub fn get_id<T: Component>(&self) -> Option<ComponentId> {
        self.type_to_id.get(&TypeId::of::<T>()).copied()
    }

    /// Get a ComponentId by TypeId (for dynamic queries).
    pub fn get_id_by_type(&self, type_id: &TypeId) -> Option<ComponentId> {
        self.type_to_id.get(type_id).copied()
    }

    pub fn get_or_register<T: Component>(&mut self) -> ComponentId {
        self.register::<T>()
    }

    pub fn get_info(&self, id: ComponentId) -> Option<&ComponentInfo> {
        self.by_id.get(id.0 as usize)
    }

    /// RT-2: resolve a component by name — the full `type_name`, or its
    /// unambiguous last `::`-segment (`"UiText"` for `"apex_ui::text::UiText"`).
    /// The shared resolver of every dynamic consumer (scripting
    /// `get_component`/`set_component`, reflective bindings). Errors are
    /// human-readable strings (§0.2a: unknown and AMBIGUOUS names refuse
    /// loudly instead of guessing).
    pub fn find_by_name(&self, name: &str) -> Result<&ComponentInfo, String> {
        if let Some(info) = self.by_id.iter().find(|i| i.name == name) {
            return Ok(info);
        }
        let mut found: Option<&ComponentInfo> = None;
        for info in &self.by_id {
            if info.name.rsplit("::").next() == Some(name) {
                if let Some(prev) = found {
                    return Err(format!(
                        "component name '{}' is ambiguous ('{}' vs '{}') — use the full type name",
                        name, prev.name, info.name
                    ));
                }
                found = Some(info);
            }
        }
        found.ok_or_else(|| format!("component '{}' is not registered", name))
    }

    /// Iterate over all registered components.
    pub fn iter(&self) -> impl Iterator<Item = &ComponentInfo> {
        self.by_id.iter()
    }

    /// Only components that have serde functions.
    pub fn iter_serializable(&self) -> impl Iterator<Item = &ComponentInfo> {
        self.by_id.iter().filter(|info| info.serde.is_some())
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TD-25: `#[require]` works **without** the `linkme` distributed-slice (the wasm path). Registering
    /// a component **lazily** (a bare `register::<T>()`, as a first spawn would on wasm where no startup
    /// registrar runs) must register its required components too — driven by `Component::register_requires`
    /// from inside `register`, not by the registrar. Proven here at registry level (platform-independent).
    #[test]
    fn lazy_register_applies_required_components_without_linkme() {
        #[derive(Default)]
        struct Req;
        impl Component for Req {}

        struct Host; // emulates a `#[derive(Component)] #[require(Req)]` type
        impl Component for Host {
            fn register_requires(reg: &mut ComponentRegistry) {
                reg.register_required::<Host, Req>();
            }
        }

        // Bare registry — NO `register_all_auto` / distributed-slice (exactly the wasm situation).
        let mut reg = ComponentRegistry::new();
        let host_id = reg.register::<Host>(); // lazy first-use registration

        // The require fired from inside `register`: Host has a required-insert closure, and Req is registered.
        assert!(
            reg.requires(host_id).is_some_and(|r| r.len() == 1),
            "register::<Host> must register its #[require] (Req) via Component::register_requires"
        );
        assert!(
            reg.type_to_id.contains_key(&TypeId::of::<Req>()),
            "the required component Req must be registered transitively"
        );

        // Idempotent: re-registering Host does NOT duplicate the require closure.
        reg.register::<Host>();
        assert_eq!(reg.requires(host_id).map(|r| r.len()), Some(1), "no duplicate require on re-register");
    }
}
