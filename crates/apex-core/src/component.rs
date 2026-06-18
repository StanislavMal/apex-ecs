#[cfg(not(target_arch = "wasm32"))]
use linkme::distributed_slice;
use rustc_hash::FxHashMap;
use std::any::TypeId;

/// Тип функции-регистратора: принимает мутабельный реестр, регистрирует компонент.
pub type ComponentRegistrarFn = fn(&mut ComponentRegistry);

/// Глобальный список всех авторегистраторов компонентов.
/// Заполняется линкером из всех крейтов, использующих #[derive(Component)].
#[cfg(not(target_arch = "wasm32"))]
#[distributed_slice]
pub static COMPONENT_REGISTRARS: [ComponentRegistrarFn] = [..];

/// На wasm32 `linkme::distributed_slice` не реализован — авторегистрация
/// на `World::new()` не выполняется, компоненты регистрируются лениво
/// (`get_or_register` на spawn/insert). ⚠ Следствие: `#[require(...)]`
/// на wasm пока НЕ применяется (регистраторы derive не запускаются) —
/// TD-25 в `apex-engine/plans/TECH_DEBT.md`, закрыть до первого
/// wasm-рантайма (план: ленивый require через trait-метод Component).
#[cfg(target_arch = "wasm32")]
pub static COMPONENT_REGISTRARS: [ComponentRegistrarFn; 0] = [];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ComponentId(pub u32);

impl ComponentId {
    /// Сентинел «компонент не зарегистрирован» — используется optional-формами
    /// запросов (`Maybe`/`MaybeWrite`) и ветками `Or<>`, чтобы СОХРАНИТЬ
    /// ВЫРАВНИВАНИЕ списка ids по `component_count()` (иначе компоненты после
    /// незарегистрированного читали бы чужие id — латентный баг до W2).
    /// Ни один реальный компонент этого id не получает (`next_id` растёт от 0).
    pub const INVALID: Self = Self(u32::MAX);
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Tick(pub u32);

impl Tick {
    pub const ZERO: Self = Self(0);

    /// Максимальный «возраст» change-тика относительно текущего тика мира.
    ///
    /// `is_newer_than` — wrapping-сравнение, корректное при разнице < 2³¹.
    /// Строка, не менявшаяся дольше, «перевернулась» бы в ложно-Changed
    /// (~99 дней аптайма @250Hz). Периодический кламп
    /// ([`World::check_change_ticks`](crate::World::check_change_ticks))
    /// подтягивает старые тики к этому возрасту, сохраняя инвариант (W2-3).
    pub const MAX_CHANGE_AGE: u32 = 1 << 30;

    #[inline]
    pub fn is_newer_than(self, last_run: Tick) -> bool {
        self.0.wrapping_sub(last_run.0) as i32 > 0
    }

    /// Подтянуть тик к окну `MAX_CHANGE_AGE` от `current`, если он старше.
    /// Возвращает `true`, если кламп произошёл.
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

// ── Сериализация компонентов ───────────────────────────────────

/// Результат сериализации одного компонента — байты в выбранном формате.
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

/// Таблица функций сериализации, хранящаяся в `ComponentInfo`.
///
/// Разделена на отдельную структуру чтобы `ComponentInfo` оставалась `Copy`-friendly
/// там где это нужно, а сами fn-pointer'ы доступны опционально.
///
/// # Safety
/// Обе функции работают с raw байтами компонента:
/// - `serialize_fn(src_ptr)` — читает T из `src_ptr`, возвращает байты (JSON/bincode/RON)
/// - `deserialize_fn(bytes)`  — принимает байты, возвращает выровненные байты T
///   пригодные для записи в Column через `write_component`.
///
/// Вызывающий обязан гарантировать что `src_ptr` указывает на живой T правильного типа.
#[derive(Clone)]
pub struct ComponentSerdeFns {
    /// Сериализовать компонент по raw-указателю в байты.
    pub serialize_fn: unsafe fn(*const u8) -> SerializeResult,
    /// Десериализовать байты обратно в выровненный буфер с данными T.
    pub deserialize_fn: fn(&[u8]) -> DeserializeResult,
    /// Человекочитаемое имя формата: "json", "bincode", "ron".
    pub format: &'static str,
}

// ── Хуки компонентов (W3-1) ────────────────────────────────────

/// Хук состава: вызывается ПОСЛЕ завершения структурной операции, когда мир
/// консистентен (`on_add` — компонент уже у entity; `on_remove` — компонента
/// уже нет; при `despawn` entity уже мертва — `is_alive` вернёт `false`).
///
/// Хук получает `&mut World` и может делать любые операции, включая
/// структурные — вложенные хуки ставятся в ту же очередь и обрабатываются
/// тем же диспетчером (без рекурсии). Только не-захватывающие функции
/// (fn-pointer): состояние подписчика живёт в ресурсах/компонентах.
/// Один хук на компонент на вид события; для нескольких подписчиков
/// используйте события ([`World::track_removals`](crate::World::track_removals)).
pub type ComponentHookFn = fn(&mut crate::World, crate::Entity);

/// Эмиттер события `Removed<T>` — мономорфизированный fn-pointer,
/// ставится `World::track_removals::<T>()`.
pub(crate) type EmitRemovedFn = fn(&mut crate::events::EventRegistry, crate::Entity);

/// Биты `ComponentRegistry::flags` — быстрый «есть ли подписчики» без
/// hashmap-lookup'а на горячих структурных путях.
pub(crate) const FLAG_ON_ADD: u8 = 1;
pub(crate) const FLAG_ON_REMOVE: u8 = 2;
pub(crate) const FLAG_TRACK_REMOVED: u8 = 4;
/// У компонента есть required-компоненты (D2-4, `#[require(...)]`).
pub(crate) const FLAG_REQUIRES: u8 = 8;
/// Маска «появление компонента кого-то интересует» (requires + on_add).
pub(crate) const ADDED_NOTIFY_MASK: u8 = FLAG_ON_ADD | FLAG_REQUIRES;

/// Вставка недостающего required-компонента (D2-4): no-op, если `R` уже есть
/// у entity (явное значение из спавна/бандла ВСЕГДА выигрывает у дефолта).
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
    /// Функции сериализации — `None` если компонент не помечен как Serializable.
    /// Заполняется при вызове `register_component_serde::<T>()`.
    pub serde: Option<ComponentSerdeFns>,
}

// ── Component trait ────────────────────────────────────────────

pub trait Component: Send + Sync + 'static {
    /// Регистрация **required-компонентов** (`#[require(...)]`, D2-4). Дефолт — требований нет;
    /// `#[derive(Component)]` переопределяет его, вызывая `register_required::<Self, R>()` для каждого
    /// `R`. Вызывается из [`ComponentRegistry::register`] РОВНО ОДИН РАЗ при первой регистрации типа —
    /// поэтому `#[require]` работает на **ВСЕХ** платформах (включая wasm) через ленивую регистрацию,
    /// **без** `linkme`/linker-магии: на нативе требования регистрирует distributed-slice-регистратор
    /// (через `register`), на wasm — первое использование компонента (тоже через `register`). TD-25.
    fn register_requires(_registry: &mut ComponentRegistry) {}
}

#[cfg(feature = "cgmath")]
impl Component for cgmath::Matrix4<f32> {}

/// Маркер: компонент можно сериализовать/десериализовать.
///
/// # Пример
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// struct Position { x: f32, y: f32 }
///
/// // Регистрация:
/// world.register_component_serde::<Position>();
/// ```
///
/// Компоненты без этого маркера (PhysicsHandle, RenderMesh, …) пропускаются
/// при снэпшоте — это нормально, они должны пересоздаваться из сериализованного state.
pub trait Serializable: Component + serde::Serialize + for<'de> serde::Deserialize<'de> {}
impl<T> Serializable for T where T: Component + serde::Serialize + for<'de> serde::Deserialize<'de> {}

// ── Drop helper ────────────────────────────────────────────────

pub(crate) unsafe fn drop_ptr<T>(ptr: *mut u8) {
    ptr.cast::<T>().drop_in_place();
}

// ── Реализация serde fn для конкретного T ─────────────────────

/// Создаёт `ComponentSerdeFns` для типа T реализующего `Serializable`.
///
/// Внутри использует `bincode` как компактный бинарный формат.
/// Формат можно сменить — достаточно поменять реализацию двух замыканий.
pub fn make_serde_fns<T: Serializable>() -> ComponentSerdeFns {
    ComponentSerdeFns {
        serialize_fn: |ptr| {
            // SAFETY: вызывающий гарантирует валидность ptr как *const T
            let val = unsafe { &*(ptr as *const T) };
            bincode::serialize(val)
                .map_err(|e| ComponentSerdeError::SerializationFailed(e.to_string()))
        },
        deserialize_fn: |bytes| {
            let val: T = bincode::deserialize(bytes)
                .map_err(|e| ComponentSerdeError::DeserializationFailed(e.to_string()))?;
            // Упаковываем T в выровненный байтовый буфер для записи в Column.
            let size = std::mem::size_of::<T>();
            let mut buf = vec![0u8; size];
            if size > 0 {
                // SAFETY: buf достаточного размера, T: Copy-compatible через serde
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

/// Создаёт `ComponentSerdeFns` для типа T реализующего `Serializable`
/// с использованием `serde_json` (текстовый формат для отладки/логов).
pub fn make_serde_fns_json<T: Serializable>() -> ComponentSerdeFns {
    ComponentSerdeFns {
        serialize_fn: |ptr| {
            let val = unsafe { &*(ptr as *const T) };
            serde_json::to_vec(val)
                .map_err(|e| ComponentSerdeError::SerializationFailed(e.to_string()))
        },
        deserialize_fn: |bytes| {
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

// ── ComponentRegistry ──────────────────────────────────────────

pub struct ComponentRegistry {
    type_to_id: FxHashMap<TypeId, ComponentId>,
    /// HashMap вместо Vec — историческое решение (произвольные ID relations
    /// до CR-M1); ids сейчас плотные от 0.
    by_id: FxHashMap<u32, ComponentInfo>,
    next_id: u32,
    /// Биты подписок per-ComponentId ([`FLAG_ON_ADD`]/[`FLAG_ON_REMOVE`]/
    /// [`FLAG_TRACK_REMOVED`]) — индекс = `ComponentId.0` (ids плотные).
    /// Горячие структурные пути сначала смотрят [`any_flags`](Self::any_flags)
    /// (один bool), потом бит — hashmap-lookup'ов нет.
    flags: Vec<u8>,
    /// Сами хуки — только для компонентов с ненулевыми flags.
    hooks: FxHashMap<u32, ComponentHooks>,
    /// Required-компоненты per cid (D2-4): вставляются дефолтом, если
    /// отсутствуют, ПОСЛЕ появления компонента-владельца (через очередь
    /// хуков, до пользовательского on_add; транзитивность — естественно
    /// через ту же очередь).
    requires: FxHashMap<u32, Vec<RequiredInsertFn>>,
    any_flags: bool,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            type_to_id: FxHashMap::default(),
            by_id: FxHashMap::default(),
            next_id: 0,
            flags: Vec::new(),
            hooks: FxHashMap::default(),
            requires: FxHashMap::default(),
            any_flags: false,
        }
    }

    /// Объявить: компонент `C` требует `R` (D2-4, аналог Bevy
    /// `#[require(...)]`). При ПОЯВЛЕНИИ `C` у entity недостающий `R`
    /// вставляется `R::default()` (явно заданное значение всегда выигрывает).
    /// Вызывается registrar'ом derive-макроса или вручную
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

    /// Required-вставки компонента (D2-4); `None` — требований нет.
    #[inline]
    pub(crate) fn requires(&self, cid: ComponentId) -> Option<&[RequiredInsertFn]> {
        self.requires.get(&cid.0).map(|v| v.as_slice())
    }

    // ── Хуки (W3-1) ────────────────────────────────────────────

    /// Есть ли в мире хоть один подписчик на состав (fast-path гейт).
    #[inline]
    pub(crate) fn any_flags(&self) -> bool {
        self.any_flags
    }

    /// Биты подписок компонента (0 — подписчиков нет).
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

    /// Регистрирует все компоненты, объявленные через `#[derive(Component)]`.
    /// Вызывается один раз при создании World.
    ///
    /// Работает через `linkme::distributed_slice` — линкер собирает
    /// статические регистраторы из всех крейтов, использующих макрос.
    pub fn register_all_auto(&mut self) {
        for registrar in COMPONENT_REGISTRARS {
            registrar(self);
        }
    }

    /// Зарегистрировать компонент без сериализации.
    pub fn register<T: Component>(&mut self) -> ComponentId {
        let type_id = TypeId::of::<T>();
        if let Some(&id) = self.type_to_id.get(&type_id) {
            return id;
        }
        let id = ComponentId(self.next_id);
        self.next_id += 1;
        self.by_id.insert(
            id.0,
            ComponentInfo {
                id,
                name: std::any::type_name::<T>(),
                type_id,
                size: std::mem::size_of::<T>(),
                align: std::mem::align_of::<T>(),
                drop_fn: drop_ptr::<T>,
                serde: None,
            },
        );
        self.type_to_id.insert(type_id, id);
        // Зарегистрировать required-компоненты (`#[require]`) РОВНО ОДИН РАЗ — здесь, на первой
        // регистрации типа (re-entrant `register::<T>` из `register_required` выйдет рано, т.к. T уже
        // в `type_to_id`). Платформо-независимо: работает и без distributed-slice (wasm — TD-25).
        T::register_requires(self);
        id
    }

    /// Зарегистрировать компонент с поддержкой сериализации.
    ///
    /// Если компонент уже зарегистрирован — только добавляет serde-функции,
    /// ID и layout не меняются.
    pub fn register_serde<T: Serializable>(&mut self) -> ComponentId {
        let id = self.register::<T>();
        if let Some(info) = self.by_id.get_mut(&id.0) {
            if info.serde.is_none() {
                info.serde = Some(make_serde_fns::<T>());
            }
        }
        id
    }

    /// Зарегистрировать компонент с поддержкой сериализации (JSON-формат).
    ///
    /// Если компонент уже зарегистрирован — только добавляет serde-функции,
    /// ID и layout не меняются.
    pub fn register_serde_json<T: Serializable>(&mut self) -> ComponentId {
        let id = self.register::<T>();
        if let Some(info) = self.by_id.get_mut(&id.0) {
            if info.serde.is_none() {
                info.serde = Some(make_serde_fns_json::<T>());
            }
        }
        id
    }

    pub fn get_id<T: Component>(&self) -> Option<ComponentId> {
        self.type_to_id.get(&TypeId::of::<T>()).copied()
    }

    /// Получить ComponentId по TypeId (для динамических запросов).
    pub fn get_id_by_type(&self, type_id: &TypeId) -> Option<ComponentId> {
        self.type_to_id.get(type_id).copied()
    }

    pub fn get_or_register<T: Component>(&mut self) -> ComponentId {
        self.register::<T>()
    }

    pub fn get_info(&self, id: ComponentId) -> Option<&ComponentInfo> {
        self.by_id.get(&id.0)
    }

    /// Итерация по всем зарегистрированным компонентам.
    pub fn iter(&self) -> impl Iterator<Item = &ComponentInfo> {
        self.by_id.values()
    }

    /// Только компоненты у которых есть serde-функции.
    pub fn iter_serializable(&self) -> impl Iterator<Item = &ComponentInfo> {
        self.by_id.values().filter(|info| info.serde.is_some())
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
