//! The text encoding of component and resource payloads: JSON (core ADR-018, ADR-024).
//!
//! **Every text payload the core writes goes through [`to_vec`]**, as every binary one goes
//! through [`crate::binary::to_vec`] — so what a text document may hold is decided once.
//!
//! # A non-finite float is refused, not written as `null`
//!
//! JSON has no spelling for `inf`, `-inf` or `NaN`, and `serde_json` writes each of them as
//! `null`. The write succeeds; the document then fails to open, because `null` is not a float —
//! a save that cannot be loaded, found only when a player loads it. [`to_vec`] makes the WRITE the
//! error, naming the value, while the world that holds it is still alive.
//!
//! A field for which a non-finite value is legitimate ("never" as a time, "unbounded" as a limit)
//! says how it is spelled: `#[serde(with = "apex_core::text::non_finite")]` writes `"Infinity"`,
//! `"-Infinity"` and `"NaN"` (the spelling of the proto3 JSON mapping) into a text document and
//! the plain float into a binary one. A type may also spell it its own way (an `Option`, a
//! sentinel) — the rule is only that the spelling is the type's decision, not an accident.

use std::fmt::Display;

use serde::ser::{
    Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
    SerializeTuple, SerializeTupleStruct, SerializeTupleVariant, Serializer,
};

pub use serde_json::{from_slice, Error};

/// Serialize `value` as JSON text — the bytes `serde_json::to_vec` writes, except that a
/// non-finite float anywhere inside is an error instead of `null` (see the module docs).
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(128);
    let mut ser = serde_json::Serializer::new(&mut out);
    value.serialize(Finite(&mut ser))?;
    Ok(out)
}

fn refuse<E: serde::ser::Error>(value: f64) -> E {
    E::custom(format_args!(
        "a non-finite float ({value}) has no spelling in a text document: it would be written as \
         `null`, which does not read back as a float. Give the field a spelling, e.g. \
         `#[serde(with = \"apex_core::text::non_finite\")]`"
    ))
}

// ── A serializer that passes everything through except a non-finite float ────────────────────

/// Wraps a serializer (and each of its compound serializers) so that every value below it is
/// written through the same wrapper.
struct Finite<S>(S);

/// A value that serializes itself through [`Finite`].
struct Checked<'a, T: ?Sized>(&'a T);

impl<T: Serialize + ?Sized> Serialize for Checked<'_, T> {
    #[inline]
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(Finite(serializer))
    }
}

macro_rules! forward {
    ($($name:ident($ty:ty);)*) => {$(
        #[inline]
        fn $name(self, v: $ty) -> Result<Self::Ok, Self::Error> {
            self.0.$name(v)
        }
    )*};
}

impl<S: Serializer> Serializer for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Finite<S::SerializeSeq>;
    type SerializeTuple = Finite<S::SerializeTuple>;
    type SerializeTupleStruct = Finite<S::SerializeTupleStruct>;
    type SerializeTupleVariant = Finite<S::SerializeTupleVariant>;
    type SerializeMap = Finite<S::SerializeMap>;
    type SerializeStruct = Finite<S::SerializeStruct>;
    type SerializeStructVariant = Finite<S::SerializeStructVariant>;

    forward! {
        serialize_bool(bool);
        serialize_i8(i8);
        serialize_i16(i16);
        serialize_i32(i32);
        serialize_i64(i64);
        serialize_i128(i128);
        serialize_u8(u8);
        serialize_u16(u16);
        serialize_u32(u32);
        serialize_u64(u64);
        serialize_u128(u128);
        serialize_char(char);
        serialize_str(&str);
        serialize_bytes(&[u8]);
    }

    #[inline]
    fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
        if v.is_finite() {
            self.0.serialize_f32(v)
        } else {
            Err(refuse(v as f64))
        }
    }

    #[inline]
    fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
        if v.is_finite() {
            self.0.serialize_f64(v)
        } else {
            Err(refuse(v))
        }
    }

    #[inline]
    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_none()
    }

    #[inline]
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_some(&Checked(value))
    }

    #[inline]
    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit()
    }

    #[inline]
    fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_struct(name)
    }

    #[inline]
    fn serialize_unit_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_variant(name, variant_index, variant)
    }

    #[inline]
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_newtype_struct(name, &Checked(value))
    }

    #[inline]
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_newtype_variant(name, variant_index, variant, &Checked(value))
    }

    #[inline]
    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.0.serialize_seq(len).map(Finite)
    }

    #[inline]
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.0.serialize_tuple(len).map(Finite)
    }

    #[inline]
    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.0.serialize_tuple_struct(name, len).map(Finite)
    }

    #[inline]
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.0.serialize_tuple_variant(name, variant_index, variant, len).map(Finite)
    }

    #[inline]
    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.0.serialize_map(len).map(Finite)
    }

    #[inline]
    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.0.serialize_struct(name, len).map(Finite)
    }

    #[inline]
    fn serialize_struct_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.0.serialize_struct_variant(name, variant_index, variant, len).map(Finite)
    }

    #[inline]
    fn collect_str<T: Display + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.0.collect_str(value)
    }

    #[inline]
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

impl<S: SerializeSeq> SerializeSeq for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    #[inline]
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_element(&Checked(value))
    }

    #[inline]
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeTuple> SerializeTuple for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    #[inline]
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_element(&Checked(value))
    }

    #[inline]
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeTupleStruct> SerializeTupleStruct for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    #[inline]
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_field(&Checked(value))
    }

    #[inline]
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeTupleVariant> SerializeTupleVariant for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    #[inline]
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_field(&Checked(value))
    }

    #[inline]
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeMap> SerializeMap for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    #[inline]
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Self::Error> {
        self.0.serialize_key(&Checked(key))
    }

    #[inline]
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_value(&Checked(value))
    }

    #[inline]
    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(
        &mut self,
        key: &K,
        value: &V,
    ) -> Result<(), Self::Error> {
        self.0.serialize_entry(&Checked(key), &Checked(value))
    }

    #[inline]
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeStruct> SerializeStruct for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    #[inline]
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.0.serialize_field(key, &Checked(value))
    }

    #[inline]
    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        self.0.skip_field(key)
    }

    #[inline]
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeStructVariant> SerializeStructVariant for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    #[inline]
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.0.serialize_field(key, &Checked(value))
    }

    #[inline]
    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        self.0.skip_field(key)
    }

    #[inline]
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

// ── The spelling of a float that may be non-finite ───────────────────────────────────────────

/// `#[serde(with = "apex_core::text::non_finite")]` for an `f32` or `f64` field that may hold
/// `inf`, `-inf` or `NaN`.
///
/// A human-readable format gets the finite value as a number and the others as the strings
/// `"Infinity"`, `"-Infinity"`, `"NaN"`; a binary format gets the float itself, which it holds
/// natively. The reader is the field's own, so it works wherever the field does — also behind
/// `#[serde(flatten)]` and tagged enums, which buffer a document before the type reads it.
pub mod non_finite {
    use std::fmt;

    use serde::de::{self, Deserializer, Visitor};
    use serde::ser::Serializer;

    const INFINITY: &str = "Infinity";
    const NEG_INFINITY: &str = "-Infinity";
    const NAN: &str = "NaN";

    /// The two float types the spelling covers.
    pub trait Float: Copy + private::Sealed {
        #[doc(hidden)]
        fn to_f64(self) -> f64;
        #[doc(hidden)]
        fn from_f64(v: f64) -> Self;
        #[doc(hidden)]
        fn write<S: Serializer>(self, s: S) -> Result<S::Ok, S::Error>;
        #[doc(hidden)]
        fn read<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error>;
    }

    mod private {
        pub trait Sealed {}
        impl Sealed for f32 {}
        impl Sealed for f64 {}
    }

    impl Float for f64 {
        #[inline]
        fn to_f64(self) -> f64 {
            self
        }
        #[inline]
        fn from_f64(v: f64) -> Self {
            v
        }
        #[inline]
        fn write<S: Serializer>(self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_f64(self)
        }
        #[inline]
        fn read<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            serde::Deserialize::deserialize(d)
        }
    }

    impl Float for f32 {
        #[inline]
        fn to_f64(self) -> f64 {
            self as f64
        }
        // The cast a float visitor of `serde` itself applies to the `f64` a text parser hands it.
        #[inline]
        fn from_f64(v: f64) -> Self {
            v as f32
        }
        #[inline]
        fn write<S: Serializer>(self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_f32(self)
        }
        #[inline]
        fn read<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            serde::Deserialize::deserialize(d)
        }
    }

    pub fn serialize<F: Float, S: Serializer>(value: &F, s: S) -> Result<S::Ok, S::Error> {
        let v = value.to_f64();
        if v.is_finite() || !s.is_human_readable() {
            value.write(s)
        } else if v.is_nan() {
            s.serialize_str(NAN)
        } else if v > 0.0 {
            s.serialize_str(INFINITY)
        } else {
            s.serialize_str(NEG_INFINITY)
        }
    }

    pub fn deserialize<'de, F: Float, D: Deserializer<'de>>(d: D) -> Result<F, D::Error> {
        if !d.is_human_readable() {
            // A binary format is not self-describing: ask for exactly what `serialize` wrote.
            return F::read(d);
        }
        d.deserialize_any(Spelled(std::marker::PhantomData))
    }

    struct Spelled<F>(std::marker::PhantomData<F>);

    impl<F: Float> Visitor<'_> for Spelled<F> {
        type Value = F;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a number, or \"{INFINITY}\", \"{NEG_INFINITY}\" or \"{NAN}\"")
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<F, E> {
            Ok(F::from_f64(v))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<F, E> {
            Ok(F::from_f64(v as f64))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<F, E> {
            Ok(F::from_f64(v as f64))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<F, E> {
            match v {
                INFINITY => Ok(F::from_f64(f64::INFINITY)),
                NEG_INFINITY => Ok(F::from_f64(f64::NEG_INFINITY)),
                NAN => Ok(F::from_f64(f64::NAN)),
                _ => Err(E::invalid_value(de::Unexpected::Str(v), &self)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    enum Shape {
        Unit,
        Newtype(f64),
        Tuple(f32, i8),
        Struct { r: f64, name: String },
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    #[serde(tag = "kind")]
    enum Tagged {
        Idle,
        Move { speed: f64 },
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    struct Inner {
        a: f32,
        b: Option<f64>,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    struct Pair(f64, u8);

    #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
    struct Wrapper(f64);

    /// Every shape `serde` can hand a serializer, floats at every depth.
    #[derive(Serialize, Deserialize, Debug, Clone)]
    struct Everything {
        flag: bool,
        small: i8,
        big: u64,
        huge: u128,
        letter: char,
        text: String,
        unit: (),
        maybe: Option<Inner>,
        nothing: Option<f64>,
        list: Vec<f64>,
        tuple: (f32, f64, i32),
        pair: Pair,
        wrapper: Wrapper,
        shapes: Vec<Shape>,
        tagged: Vec<Tagged>,
        by_name: BTreeMap<String, f64>,
        by_number: BTreeMap<u32, Inner>,
        #[serde(flatten)]
        flat: Inner,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<f64>,
        raw: Box<serde_json::value::RawValue>,
    }

    fn everything() -> Everything {
        Everything {
            flag: true,
            small: -7,
            big: u64::MAX,
            huge: u128::MAX,
            letter: 'я',
            text: "a \"quoted\" line\n".into(),
            unit: (),
            maybe: Some(Inner { a: 0.1, b: Some(-0.0) }),
            nothing: None,
            list: vec![0.1 + 0.2, 1e-300, 1e300, f64::MIN_POSITIVE, -1.5],
            tuple: (f32::MAX, f64::EPSILON, -3),
            pair: Pair(2.5, 9),
            wrapper: Wrapper(1.0 / 3.0),
            shapes: vec![
                Shape::Unit,
                Shape::Newtype(6.02e23),
                Shape::Tuple(0.3, -1),
                Shape::Struct { r: 1.0, name: "disc".into() },
            ],
            tagged: vec![Tagged::Idle, Tagged::Move { speed: 4000.0 }],
            by_name: BTreeMap::from([("x".to_string(), 1.25), ("y".to_string(), -2.0)]),
            by_number: BTreeMap::from([(3, Inner { a: 3.0, b: None })]),
            flat: Inner { a: 7.5, b: Some(8.5) },
            skipped: None,
            raw: serde_json::value::RawValue::from_string("{\"kept\":  [1, 2.50]}".into()).unwrap(),
        }
    }

    /// The wrapper changes nothing but the refusal: for finite data the bytes are the bytes of
    /// `serde_json::to_vec`, through every compound serializer, and they read back as the value.
    #[test]
    fn a_finite_value_is_written_byte_for_byte_as_serde_json_writes_it() {
        let v = everything();
        let bytes = super::to_vec(&v).unwrap();
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            serde_json::to_string(&v).unwrap()
        );
        // `RawValue` has no `PartialEq`: the value that came back is compared by what it writes
        // (floats are printed shortest-round-trip, so equal text is equal bits) and by a few
        // fields read directly.
        let back: Everything = super::from_slice(&bytes).unwrap();
        assert_eq!(super::to_vec(&back).unwrap(), bytes);
        assert_eq!(back.raw.get(), v.raw.get());
        assert_eq!((back.list, back.shapes, back.flat), (v.list, v.shapes, v.flat));
    }

    /// The reason the module exists: `serde_json` writes `null`, and the document it wrote does
    /// not open.
    #[test]
    fn serde_json_alone_writes_a_document_that_does_not_read_back() {
        let text = serde_json::to_string(&Inner { a: 1.0, b: Some(f64::INFINITY) }).unwrap();
        assert_eq!(text, r#"{"a":1.0,"b":null}"#);
        // Here `null` happens to read as `None` — the value silently changed.
        assert_eq!(serde_json::from_str::<Inner>(&text).unwrap().b, None);
        // And a plain float does not read at all.
        let text = serde_json::to_string(&Wrapper(f64::NAN)).unwrap();
        assert!(serde_json::from_str::<Wrapper>(&text).is_err());
    }

    /// A non-finite float is refused wherever it sits: each case puts one into a different
    /// compound serializer of the wrapper.
    #[test]
    fn a_non_finite_float_is_refused_at_every_depth() {
        let base = everything();
        let cases: Vec<(&str, Box<dyn Fn(&mut Everything)>)> = vec![
            ("struct field inside an option", Box::new(|v| v.maybe = Some(Inner { a: f32::INFINITY, b: None }))),
            ("option", Box::new(|v| v.nothing = Some(f64::NAN))),
            ("sequence", Box::new(|v| v.list.push(f64::NEG_INFINITY))),
            ("tuple", Box::new(|v| v.tuple.1 = f64::INFINITY)),
            ("tuple struct", Box::new(|v| v.pair.0 = f64::INFINITY)),
            ("newtype struct", Box::new(|v| v.wrapper = Wrapper(f64::NAN))),
            ("newtype variant", Box::new(|v| v.shapes.push(Shape::Newtype(f64::INFINITY)))),
            ("tuple variant", Box::new(|v| v.shapes.push(Shape::Tuple(f32::NAN, 0)))),
            ("struct variant", Box::new(|v| v.shapes.push(Shape::Struct { r: f64::INFINITY, name: String::new() }))),
            ("internally tagged variant", Box::new(|v| v.tagged.push(Tagged::Move { speed: f64::INFINITY }))),
            ("map value", Box::new(|v| { v.by_name.insert("z".into(), f64::INFINITY); })),
            ("struct as a map value", Box::new(|v| { v.by_number.insert(4, Inner { a: 0.0, b: Some(f64::NAN) }); })),
            ("flattened field", Box::new(|v| v.flat.a = f32::NEG_INFINITY)),
            ("field behind skip_serializing_if", Box::new(|v| v.skipped = Some(f64::INFINITY))),
        ];
        for (what, spoil) in cases {
            let mut v = base.clone();
            spoil(&mut v);
            let err = super::to_vec(&v).expect_err(what).to_string();
            assert!(err.contains("non-finite float"), "{what}: {err}");
            assert!(err.contains("apex_core::text::non_finite"), "{what}: {err}");
            // The control: `serde_json` alone writes it without a word.
            assert!(serde_json::to_string(&v).is_ok(), "{what}");
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    struct Deadline {
        #[serde(with = "super::non_finite")]
        at: f64,
        #[serde(with = "super::non_finite")]
        limit: f32,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    #[serde(tag = "kind")]
    enum Order {
        Wait {
            #[serde(with = "super::non_finite")]
            until: f64,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    struct Holder {
        id: u32,
        #[serde(flatten)]
        deadline: Deadline,
    }

    fn same_bits(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
    }

    /// The spelling carries every float, finite to the last bit, through the text document.
    #[test]
    fn a_spelled_float_survives_the_text_document() {
        let values = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            0.0,
            -0.0,
            3.0,
            -7.0,
            0.1 + 0.2,
            1e-320,
            f64::MAX,
            f64::MIN_POSITIVE,
        ];
        for at in values {
            let v = Deadline { at, limit: at as f32 };
            let text = String::from_utf8(super::to_vec(&v).unwrap()).unwrap();
            let back: Deadline = super::from_slice(text.as_bytes()).unwrap();
            assert!(same_bits(back.at, v.at), "{at}: {text}");
            assert!(same_bits(back.limit as f64, v.limit as f64), "{at}: {text}");
        }
        let text = String::from_utf8(super::to_vec(&Deadline { at: f64::INFINITY, limit: f32::NEG_INFINITY }).unwrap()).unwrap();
        assert_eq!(text, r#"{"at":"Infinity","limit":"-Infinity"}"#);
        // A finite value is the plain number, not a string.
        let text = String::from_utf8(super::to_vec(&Deadline { at: 1.5, limit: 2.0 }).unwrap()).unwrap();
        assert_eq!(text, r#"{"at":1.5,"limit":2.0}"#);
        // Any other string is an error that names the three spellings.
        let err = super::from_slice::<Deadline>(br#"{"at":"inf","limit":0}"#).unwrap_err().to_string();
        assert!(err.contains("Infinity"), "{err}");
    }

    /// `flatten` and tagged enums buffer the document before the type reads it; the spelling is
    /// read by the field itself, so it survives the buffer.
    #[test]
    fn a_spelled_float_survives_a_buffered_document() {
        let order = Order::Wait { until: f64::INFINITY };
        let text = super::to_vec(&order).unwrap();
        assert_eq!(String::from_utf8(text.clone()).unwrap(), r#"{"kind":"Wait","until":"Infinity"}"#);
        let Order::Wait { until } = super::from_slice(&text).unwrap();
        assert_eq!(until, f64::INFINITY);

        let holder = Holder { id: 1, deadline: Deadline { at: f64::NEG_INFINITY, limit: 0.5 } };
        let back: Holder = super::from_slice(&super::to_vec(&holder).unwrap()).unwrap();
        assert_eq!(back.deadline.at, f64::NEG_INFINITY);
        assert_eq!(back.deadline.limit, 0.5);
    }

    /// The same field in the binary form is the float itself: same bytes as an unannotated one.
    #[test]
    fn a_spelled_float_is_a_plain_float_in_the_binary_form() {
        #[derive(Serialize)]
        struct Plain {
            at: f64,
            limit: f32,
        }
        for at in [f64::INFINITY, f64::NEG_INFINITY, 1.5, -0.0] {
            let v = Deadline { at, limit: at as f32 };
            let bytes = crate::binary::to_vec(&v).unwrap();
            assert_eq!(bytes, crate::binary::to_vec(&Plain { at, limit: at as f32 }).unwrap());
            let back: Deadline = crate::binary::from_bytes(&bytes).unwrap();
            assert!(same_bits(back.at, v.at) && same_bits(back.limit as f64, v.limit as f64));
        }
        let nan: Deadline = crate::binary::from_bytes(&crate::binary::to_vec(&Deadline { at: f64::NAN, limit: f32::NAN }).unwrap()).unwrap();
        assert!(nan.at.is_nan() && nan.limit.is_nan());
    }
}
