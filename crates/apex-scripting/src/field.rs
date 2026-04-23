//! `ScriptableField` — конвертация примитивных типов между Rust и Rhai Dynamic.
//!
//! Поддерживаемые типы: f32, i32, u32, bool, String.
//! Для вложенных структур достаточно реализовать `ScriptableRegistrar`,
//! который внутри тоже использует Dynamic Map.
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

use rhai::Dynamic;

/// Конвертация поля компонента в/из Rhai Dynamic.
///
/// Реализован для примитивов: `f32`, `i32`, `u32`, `bool`, `String`.
/// Для кортежей и вложенных структур используй `ScriptableRegistrar::to_dynamic`.
pub trait ScriptableField: Sized + Clone {
    fn to_dynamic(&self) -> Dynamic;
    fn from_dynamic(d: &Dynamic) -> Option<Self>;
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