use std::any::TypeId;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ComponentId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Tick(pub u32);

impl Tick {
    pub const ZERO: Self = Self(0);

    #[inline]
    pub fn is_newer_than(self, last_run: Tick) -> bool {
        self.0 > last_run.0
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
            Self::SerializationFailed(s)   => write!(f, "serialize failed: {}", s),
            Self::DeserializationFailed(s) => write!(f, "deserialize failed: {}", s),
            Self::FormatMismatch { expected } => write!(f, "format mismatch, expected {}", expected),
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
    pub serialize_fn:   unsafe fn(*const u8) -> SerializeResult,
    /// Десериализовать байты обратно в выровненный буфер с данными T.
    pub deserialize_fn: fn(&[u8])            -> DeserializeResult,
    /// Человекочитаемое имя формата: "json", "bincode", "ron".
    pub format:         &'static str,
}

// ── ComponentInfo ──────────────────────────────────────────────

pub struct ComponentInfo {
    pub id:       ComponentId,
    pub name:     &'static str,
    pub type_id:  TypeId,
    pub size:     usize,
    pub align:    usize,
    pub drop_fn:  unsafe fn(*mut u8),
    /// Функции сериализации — `None` если компонент не помечен как Serializable.
    /// Заполняется при вызове `register_component_serde::<T>()`.
    pub serde:    Option<ComponentSerdeFns>,
}

// ── Component trait ────────────────────────────────────────────

pub trait Component: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> Component for T {}

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
impl<T> Serializable for T
where
    T: Component + serde::Serialize + for<'de> serde::Deserialize<'de>,
{}

// ── Drop helper ────────────────────────────────────────────────

pub(crate) unsafe fn drop_ptr<T>(ptr: *mut u8) {
    ptr.cast::<T>().drop_in_place();
}

// ── Реализация serde fn для конкретного T ─────────────────────

/// Создаёт `ComponentSerdeFns` для типа T реализующего `Serializable`.
///
/// Внутри использует `serde_json` как дефолтный формат.
/// Формат можно сменить — достаточно поменять реализацию двух замыканий.
pub fn make_serde_fns<T: Serializable>() -> ComponentSerdeFns {
    ComponentSerdeFns {
        serialize_fn: |ptr| {
            // SAFETY: вызывающий гарантирует валидность ptr как *const T
            let val = unsafe { &*(ptr as *const T) };
            serde_json::to_vec(val)
                .map_err(|e| ComponentSerdeError::SerializationFailed(e.to_string()))
        },
        deserialize_fn: |bytes| {
            let val: T = serde_json::from_slice(bytes)
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
        format: "json",
    }
}

// ── ComponentRegistry ──────────────────────────────────────────

pub struct ComponentRegistry {
    type_to_id: FxHashMap<TypeId, ComponentId>,
    /// HashMap вместо Vec — поддержка произвольных ID (relations).
    by_id:      FxHashMap<u32, ComponentInfo>,
    next_id:    u32,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            type_to_id: FxHashMap::default(),
            by_id:      FxHashMap::default(),
            next_id:    0,
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
        self.by_id.insert(id.0, ComponentInfo {
            id,
            name:    std::any::type_name::<T>(),
            type_id,
            size:    std::mem::size_of::<T>(),
            align:   std::mem::align_of::<T>(),
            drop_fn: drop_ptr::<T>,
            serde:   None,
        });
        self.type_to_id.insert(type_id, id);
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

    /// Зарегистрировать компонент с заранее известным ID (для relations).
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
}

impl Default for ComponentRegistry {
    fn default() -> Self { Self::new() }
}