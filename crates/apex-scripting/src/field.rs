//! `ScriptableField` — конвертация примитивных типов между Rust и Rhai Dynamic.
//!
//! Поддерживаемые типы: f32, i32, u32, bool, String, Vec, HashMap.
//! Для вложенных структур достаточно реализовать `ScriptableRegistrar`,
//! который внутри тоже использует Dynamic Map.
//!
//! # Zero-copy для примитивных полей
//!
//! Компоненты, состоящие из одного примитивного поля (например `Health(f32)`),
//! могут читаться напрямую из Column без создания промежуточного Dynamic Map,
//! что сокращает аллокации в `build_item()`.
//!
//! Для этого `ScriptableRegistrar::primitive_info()` должен вернуть `Some(...)`.
//!
//! # Добавление нового типа
//!
//! ```ignore
//! impl ScriptableField for MyEnum {
//!     fn to_dynamic(&self) -> Dynamic {
//!         Dynamic::from(*self as i64)
//!     }
//!     fn from_dynamic(d: &Dynamic) -> Option<Self> {
//!         let n = d.as_int().ok()?;
//!         MyEnum::from_i64(n)
//!     }
//! }
//! ```

use std::collections::HashMap;
use rhai::Dynamic;

/// Конвертация поля компонента в/из Rhai Dynamic.
///
/// Реализован для примитивов: `f32`, `i32`, `u32`, `bool`, `String`.
/// Для кортежей и вложенных структур используй `ScriptableRegistrar::to_dynamic`.
pub trait ScriptableField: Sized + Clone {
    fn to_dynamic(&self) -> Dynamic;
    fn from_dynamic(d: &Dynamic) -> Option<Self>;
}

// ── PrimitiveInfo ────────────────────────────────────────────────

/// Мета-информация о примитивном типе для zero-copy read path.
///
/// Позволяет `build_item()` читать значение напрямую из сырой памяти Column,
/// минуя вызов `binding.read` (который для структур создаёт Dynamic Map).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimitiveInfo {
    F32,
    F64,
    I32,
    I64,
    U32,
    U64,
    Usize,
    Bool,
    String,
}

impl PrimitiveInfo {
    /// Прочитать значение напрямую из сырого указателя, без создания Map.
    ///
    /// # Safety
    /// `ptr` должен указывать на валидные данные соответствующего типа.
    #[inline]
    pub unsafe fn read_raw(&self, ptr: *const u8) -> Dynamic {
        match self {
            Self::F32 => Dynamic::from_float(*(ptr as *const f32) as rhai::FLOAT),
            Self::F64 => Dynamic::from_float(*(ptr as *const f64) as rhai::FLOAT),
            Self::I32 => Dynamic::from_int(*(ptr as *const i32) as rhai::INT),
            Self::I64 => Dynamic::from_int(*(ptr as *const i64) as rhai::INT),
            Self::U32 => Dynamic::from_int(*(ptr as *const u32) as rhai::INT),
            Self::U64 => Dynamic::from_int(*(ptr as *const u64) as rhai::INT),
            Self::Usize => Dynamic::from_int(*(ptr as *const usize) as rhai::INT),
            Self::Bool => Dynamic::from_bool(*(ptr as *const bool)),
            Self::String => {
                let s = &*(ptr as *const String);
                Dynamic::from(rhai::ImmutableString::from(s.as_str()))
            }
        }
    }

    /// Записать значение напрямую в сырой указатель, без создания Map.
    ///
    /// # Safety
    /// `ptr` должен указывать на валидную память соответствующего типа.
    #[inline]
    pub unsafe fn write_raw(&self, ptr: *mut u8, dynamic: &Dynamic) -> bool {
        match self {
            Self::F32 => {
                if let Ok(v) = dynamic.as_float() {
                    *(ptr as *mut f32) = v as f32;
                    true
                } else { false }
            }
            Self::F64 => {
                if let Ok(v) = dynamic.as_float() {
                    *(ptr as *mut f64) = v;
                    true
                } else { false }
            }
            Self::I32 => {
                if let Ok(v) = dynamic.as_int() {
                    *(ptr as *mut i32) = v as i32;
                    true
                } else { false }
            }
            Self::I64 => {
                if let Ok(v) = dynamic.as_int() {
                    *(ptr as *mut i64) = v;
                    true
                } else { false }
            }
            Self::U32 => {
                if let Ok(v) = dynamic.as_int() {
                    *(ptr as *mut u32) = v as u32;
                    true
                } else { false }
            }
            Self::U64 => {
                if let Ok(v) = dynamic.as_int() {
                    *(ptr as *mut u64) = v as u64;
                    true
                } else { false }
            }
            Self::Usize => {
                if let Ok(v) = dynamic.as_int() {
                    *(ptr as *mut usize) = v as usize;
                    true
                } else { false }
            }
            Self::Bool => {
                if let Ok(v) = dynamic.as_bool() {
                    *(ptr as *mut bool) = v;
                    true
                } else { false }
            }
            Self::String => {
                if let Ok(s) = dynamic.clone().into_string() {
                    *(ptr as *mut String) = s;
                    true
                } else { false }
            }
        }
    }
}

// ── f32 ────────────────────────────────────────────────────────

impl ScriptableField for f32 {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_float(*self as rhai::FLOAT)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        // Rhai хранит float как f64 (FLOAT = f64 по умолчанию)
        d.as_float().ok().map(|v| v as f32)
            .or_else(|| d.as_int().ok().map(|v| v as f32))
    }
}

// ── f64 ────────────────────────────────────────────────────────

impl ScriptableField for f64 {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_float(*self as rhai::FLOAT)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.as_float().ok().map(|v| v as f64)
            .or_else(|| d.as_int().ok().map(|v| v as f64))
    }
}

// ── i32 ────────────────────────────────────────────────────────

impl ScriptableField for i32 {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_int(*self as rhai::INT)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.as_int().ok().map(|v| v as i32)
            .or_else(|| d.as_float().ok().map(|v| v as i32))
    }
}

// ── i64 ────────────────────────────────────────────────────────

impl ScriptableField for i64 {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_int(*self as rhai::INT)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.as_int().ok().map(|v| v as i64)
    }
}

// ── u32 ────────────────────────────────────────────────────────

impl ScriptableField for u32 {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_int(*self as rhai::INT)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.as_int().ok().and_then(|v| u32::try_from(v).ok())
            .or_else(|| d.as_float().ok().map(|v| v as u32))
    }
}

// ── u64 ────────────────────────────────────────────────────────

impl ScriptableField for u64 {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_int(*self as rhai::INT)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.as_int().ok().map(|v| v as u64)
    }
}

// ── usize ──────────────────────────────────────────────────────

impl ScriptableField for usize {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_int(*self as rhai::INT)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.as_int().ok().map(|v| v as usize)
    }
}

// ── bool ───────────────────────────────────────────────────────

impl ScriptableField for bool {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from_bool(*self)
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.as_bool().ok()
    }
}

// ── String ─────────────────────────────────────────────────────

impl ScriptableField for String {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from(rhai::ImmutableString::from(self.as_str()))
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        d.clone().into_string().ok()
    }
}

// ── &'static str ───────────────────────────────────────────────

impl ScriptableField for &'static str {
    #[inline]
    fn to_dynamic(&self) -> Dynamic {
        Dynamic::from(rhai::ImmutableString::from(*self))
    }

    #[inline]
    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        // static str из Dynamic восстановить нельзя — только через String
        // Возвращаем None; используйте String если нужна мутабельность
        let _ = d;
        None
    }
}

// ── Кортежи ────────────────────────────────────────────────────
//
// Представляем кортежи как Rhai-массивы для простоты.

impl<A, B> ScriptableField for (A, B)
where
    A: ScriptableField,
    B: ScriptableField,
{
    fn to_dynamic(&self) -> Dynamic {
        let arr: rhai::Array = vec![self.0.to_dynamic(), self.1.to_dynamic()];
        Dynamic::from_array(arr)
    }

    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        let arr = d.read_lock::<rhai::Array>()?;
        let a = A::from_dynamic(arr.get(0)?)?;
        let b = B::from_dynamic(arr.get(1)?)?;
        Some((a, b))
    }
}

impl<A, B, C> ScriptableField for (A, B, C)
where
    A: ScriptableField,
    B: ScriptableField,
    C: ScriptableField,
{
    fn to_dynamic(&self) -> Dynamic {
        let arr: rhai::Array = vec![
            self.0.to_dynamic(),
            self.1.to_dynamic(),
            self.2.to_dynamic(),
        ];
        Dynamic::from_array(arr)
    }

    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        let arr = d.read_lock::<rhai::Array>()?;
        let a = A::from_dynamic(arr.get(0)?)?;
        let b = B::from_dynamic(arr.get(1)?)?;
        let c = C::from_dynamic(arr.get(2)?)?;
        Some((a, b, c))
    }
}

// ── Option<T> ──────────────────────────────────────────────────

impl<T: ScriptableField> ScriptableField for Option<T> {
    fn to_dynamic(&self) -> Dynamic {
        match self {
            Some(v) => v.to_dynamic(),
            None    => Dynamic::UNIT,
        }
    }

    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        if d.is_unit() {
            Some(None)
        } else {
            Some(Some(T::from_dynamic(d)?))
        }
    }
}

// ── Vec<T> ──────────────────────────────────────────────────────

impl<T: ScriptableField> ScriptableField for Vec<T> {
    fn to_dynamic(&self) -> Dynamic {
        let arr: rhai::Array = self.iter().map(|v| v.to_dynamic()).collect();
        Dynamic::from_array(arr)
    }

    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        let arr = d.read_lock::<rhai::Array>()?;
        arr.iter().map(|v| T::from_dynamic(v)).collect()
    }
}

// ── HashMap<String, V> ──────────────────────────────────────────

impl<V: ScriptableField> ScriptableField for HashMap<String, V> {
    fn to_dynamic(&self) -> Dynamic {
        let mut map = rhai::Map::new();
        for (k, v) in self.iter() {
            map.insert(k.clone().into(), v.to_dynamic());
        }
        Dynamic::from_map(map)
    }

    fn from_dynamic(d: &Dynamic) -> Option<Self> {
        let lock = d.read_lock::<rhai::Map>()?;
        let mut out = HashMap::new();
        for (k, v) in lock.iter() {
            let val = V::from_dynamic(v)?;
            out.insert(k.to_string(), val);
        }
        Some(out)
    }
}
