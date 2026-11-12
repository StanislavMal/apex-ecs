//! The self-describing binary encoding of the core: MessagePack with every struct written as a
//! map of its field NAMES (core ADR-025).
//!
//! [`crate::binary`] (`postcard`) writes a struct as its values in declaration order and nothing
//! else. A field added, removed or moved changes what every later byte means, so those bytes are
//! read only by the build that wrote them: the format of a snapshot that lives inside one run.
//! This encoding carries the names, so it is the binary format of a SAVE:
//!
//! - a field the type gained since the write reads as its `#[serde(default)]`, a field it lost
//!   is skipped, the order of fields is free;
//! - an enum variant is written by its name, so variants can be added and reordered;
//! - `skip_serializing_if`, `flatten` and enums without a tag work, as they do in JSON and do
//!   not in `postcard`;
//! - a float is its bits: `inf` and `NaN` need no spelling ([`crate::text::non_finite`] writes
//!   the float itself here, the encoding is not human-readable), and a map may be keyed by
//!   anything, not only by a string.
//!
//! What it shares with JSON: `None` and `()` are both `nil`, so `Option<Option<T>>` does not
//! tell `Some(None)` from `None`.
//!
//! **Every write of this encoding goes through [`to_vec`] and every read through
//! [`from_slice`]**, so the mapping of the serde data model to MessagePack is decided once. The
//! mapping is `rmp-serde` 1.x with `with_struct_map`; the bytes of a fixture are pinned by the
//! test `the_encoding_is_pinned`, so a change of the mapping by the library fails here and not in
//! a save a player already has.

use serde::{Deserialize, Serialize};

/// Why a value was not written or read.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("{0}")]
    Decode(#[from] rmp_serde::decode::Error),
    /// The value was read and bytes are left: these are not the bytes of one value.
    #[error("bytes left after the value")]
    Trailing,
}

/// Serialize `value` as MessagePack, structs as maps of field names.
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(128);
    value.serialize(&mut rmp_serde::Serializer::new(&mut out).with_struct_map())?;
    Ok(out)
}

/// Read one value from `bytes`: all of them. Bytes left after the value are an error
/// ([`Error::Trailing`]) — a payload that is longer than its value is a damaged payload, and the
/// reader of a save says so instead of returning the part it understood.
pub fn from_slice<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    let mut de = rmp_serde::Deserializer::from_read_ref(bytes);
    let value = T::deserialize(&mut de)?;
    // The reader does not tell how much it left, so it is asked for one more value: the end of
    // the input at the very first byte of it is the only answer that means "nothing is left".
    match serde::de::IgnoredAny::deserialize(&mut de) {
        Err(rmp_serde::decode::Error::InvalidMarkerRead(e))
            if e.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            Ok(value)
        }
        _ => Err(Error::Trailing),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use super::{from_slice, to_vec};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Order {
        Idle,
        Follow(u32),
        Move { x: f64, y: f64 },
        Pair(u8, u8),
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Unit;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Wrapped(u16);

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sample {
        id: u64,
        name: String,
        speed: f32,
        at: f64,
        alive: bool,
        target: Option<u32>,
        nothing: Option<u32>,
        orders: Vec<Order>,
        cargo: BTreeMap<u32, i64>,
        pair: (i8, char),
        unit: Unit,
        wrapped: Wrapped,
        #[serde(with = "serde_bytes_like")]
        blob: Vec<u8>,
    }

    /// A byte block written as bytes, the way the snapshot container writes a payload.
    mod serde_bytes_like {
        pub fn serialize<S: serde::Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(v)
        }
        pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
            struct V;
            impl serde::de::Visitor<'_> for V {
                type Value = Vec<u8>;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("bytes")
                }
                fn visit_bytes<E>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                    Ok(v.to_vec())
                }
            }
            d.deserialize_bytes(V)
        }
    }

    fn sample() -> Sample {
        Sample {
            id: 300,
            name: "a name".into(),
            speed: 1.5,
            at: -0.25,
            alive: true,
            target: Some(7),
            nothing: None,
            orders: vec![Order::Idle, Order::Follow(9), Order::Move { x: 1.0, y: 2.0 }, Order::Pair(1, 2)],
            cargo: BTreeMap::from([(1, -5), (70_000, 1 << 40)]),
            pair: (-3, 'я'),
            unit: Unit,
            wrapped: Wrapped(65_535),
            blob: vec![0, 255, 7],
        }
    }

    /// The bytes of every shape of the serde data model, fixed. A save is read years after it was
    /// written: if a new version of the library spelled a variant by number or a struct by
    /// position, old saves would stop opening — this test stops that version at the door.
    #[test]
    fn the_encoding_is_pinned() {
        let bytes = to_vec(&sample()).unwrap();
        let pinned: &[u8] = PINNED;
        assert_eq!(bytes, pinned, "the encoding moved; as Rust: {bytes:?}");
        assert_eq!(from_slice::<Sample>(pinned).unwrap(), sample());
        // What the pin says in words: names are in the bytes, positions are not what is read.
        for word in ["id", "orders", "Idle", "Follow", "Move", "wrapped"] {
            assert!(bytes.windows(word.len()).any(|w| w == word.as_bytes()), "`{word}` is not in the bytes");
        }
    }

    #[rustfmt::skip]
    const PINNED: &[u8] = &[
        0x8d, // a map of 13 fields
        // id: 300
        0xa2, 0x69, 0x64, 0xcd, 0x01, 0x2c,
        // name
        0xa4, 0x6e, 0x61, 0x6d, 0x65, 0xa6, 0x61, 0x20, 0x6e, 0x61, 0x6d, 0x65,
        // speed: f32
        0xa5, 0x73, 0x70, 0x65, 0x65, 0x64, 0xca, 0x3f, 0xc0, 0x00, 0x00,
        // at: f64
        0xa2, 0x61, 0x74, 0xcb, 0xbf, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // alive
        0xa5, 0x61, 0x6c, 0x69, 0x76, 0x65, 0xc3,
        // target: Some(7)
        0xa6, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x07,
        // nothing: None is nil
        0xa7, 0x6e, 0x6f, 0x74, 0x68, 0x69, 0x6e, 0x67, 0xc0,
        // orders: a unit variant is its name, the others a map of one name
        0xa6, 0x6f, 0x72, 0x64, 0x65, 0x72, 0x73, 0x94, 0xa4, 0x49, 0x64, 0x6c, 0x65, 0x81, 0xa6, 0x46,
        0x6f, 0x6c, 0x6c, 0x6f, 0x77, 0x09, 0x81, 0xa4, 0x4d, 0x6f, 0x76, 0x65, 0x82, 0xa1, 0x78, 0xcb,
        0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa1, 0x79, 0xcb, 0x40, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x81, 0xa4, 0x50, 0x61, 0x69, 0x72, 0x92, 0x01, 0x02,
        // cargo: a map keyed by numbers
        0xa5, 0x63, 0x61, 0x72, 0x67, 0x6f, 0x82, 0x01, 0xfb, 0xce, 0x00, 0x01, 0x11, 0x70, 0xcf, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        // pair: a tuple is an array
        0xa4, 0x70, 0x61, 0x69, 0x72, 0x92, 0xfd, 0xa2, 0xd1, 0x8f,
        // unit: a unit struct is an empty array
        0xa4, 0x75, 0x6e, 0x69, 0x74, 0x90,
        // wrapped: a newtype is its value
        0xa7, 0x77, 0x72, 0x61, 0x70, 0x70, 0x65, 0x64, 0xcd, 0xff, 0xff,
        // blob: bytes are a block
        0xa4, 0x62, 0x6c, 0x6f, 0x62, 0xc4, 0x03, 0x00, 0xff, 0x07,
    ];

    // ── What a save needs: an older payload under a newer type ───

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum KindV1 {
        Idle,
        Go(u32),
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct FleetV1 {
        id: u32,
        gone: String,
        kind: KindV1,
        t: f64,
    }

    /// The same type some releases later: a variant put in FRONT (every index moved), a field
    /// added first, a field removed, the rest reordered.
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum KindV2 {
        Wait { s: f64 },
        Idle,
        Go(u32),
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct FleetV2 {
        #[serde(default)]
        added: Vec<u8>,
        t: f64,
        kind: KindV2,
        id: u32,
    }

    #[test]
    fn an_older_payload_is_read_by_the_newer_type() {
        for (kind, expected) in [(KindV1::Go(3), KindV2::Go(3)), (KindV1::Idle, KindV2::Idle)] {
            let old = to_vec(&FleetV1 { id: 7, gone: "x".into(), kind, t: 2.5 }).unwrap();
            assert_eq!(
                from_slice::<FleetV2>(&old).unwrap(),
                FleetV2 { added: Vec::new(), t: 2.5, kind: expected, id: 7 }
            );
        }
        // The compact encoding holds positions, and the same change is not readable there: this
        // is the difference the module exists for.
        let old = crate::binary::to_vec(&FleetV1 { id: 7, gone: "x".into(), kind: KindV1::Go(3), t: 2.5 }).unwrap();
        assert!(crate::binary::from_bytes::<FleetV2>(&old).is_err());
    }

    /// A field without a default that the payload does not have is an error naming the field,
    /// not a zero.
    #[test]
    fn a_missing_field_without_a_default_is_named() {
        #[derive(Serialize)]
        struct Old {
            id: u32,
        }
        #[derive(Deserialize, Debug)]
        struct New {
            #[allow(dead_code)]
            id: u32,
            #[allow(dead_code)]
            must: u32,
        }
        let err = from_slice::<New>(&to_vec(&Old { id: 1 }).unwrap()).unwrap_err().to_string();
        assert!(err.contains("must"), "{err}");
    }

    // ── Shapes the compact encoding cannot read ──────────────────

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Inner {
        x: f64,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Shapes {
        n: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<f64>,
        #[serde(flatten)]
        inner: Inner,
        free: Free,
        tagged: Tagged,
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[serde(untagged)]
    enum Free {
        Number(u32),
        Text(String),
    }
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[serde(tag = "kind")]
    enum Tagged {
        Dock { port: u32 },
        Free,
    }

    #[test]
    fn skipped_flattened_and_untagged_shapes_round_trip() {
        for (at, free, tagged) in [
            (None, Free::Number(5), Tagged::Dock { port: 2 }),
            (Some(0.5), Free::Text("a".into()), Tagged::Free),
        ] {
            let v = Shapes { n: 2, at, inner: Inner { x: 1.5 }, free, tagged };
            assert_eq!(from_slice::<Shapes>(&to_vec(&v).unwrap()).unwrap(), v);
        }
    }

    // ── Numbers ──────────────────────────────────────────────────

    #[derive(Serialize, Deserialize, Debug)]
    struct Floats {
        a: f64,
        b: f32,
        #[serde(with = "crate::text::non_finite")]
        spelled: f64,
    }

    /// A float is its bits — the infinities, a NaN with its payload, the negative zero, a
    /// subnormal — with or without the text spelling of non-finite values on the field.
    #[test]
    fn a_float_is_its_bits() {
        let nan = f64::from_bits(0x7ff8_0000_0000_beef);
        for a in [f64::INFINITY, f64::NEG_INFINITY, nan, -0.0, f64::MIN_POSITIVE / 8.0, 0.1] {
            for b in [f32::INFINITY, f32::NAN, -0.0f32, 0.1f32] {
                let back: Floats = from_slice(&to_vec(&Floats { a, b, spelled: a }).unwrap()).unwrap();
                assert_eq!(back.a.to_bits(), a.to_bits());
                assert_eq!(back.b.to_bits(), b.to_bits());
                assert_eq!(back.spelled.to_bits(), a.to_bits());
            }
        }
    }

    #[test]
    fn integers_keep_their_range() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Ints {
            a: u64,
            b: i64,
            c: u128,
            d: i8,
            keys: BTreeMap<(u32, i32), u8>,
        }
        let v = Ints {
            a: u64::MAX,
            b: i64::MIN,
            c: u128::MAX - 1,
            d: -128,
            keys: BTreeMap::from([((1, -1), 2), ((u32::MAX, i32::MIN), 3)]),
        };
        assert_eq!(from_slice::<Ints>(&to_vec(&v).unwrap()).unwrap(), v);
    }

    // ── A damaged payload is an error ────────────────────────────

    #[test]
    fn bytes_after_the_value_are_an_error() {
        // A whole value after the value, a cut one, and a byte that starts no value at all.
        for tail in [&[0u8][..], &[0xd9, 5, b'a'], &[0xc1]] {
            let mut bytes = to_vec(&sample()).unwrap();
            bytes.extend_from_slice(tail);
            let err = from_slice::<Sample>(&bytes).unwrap_err().to_string();
            assert!(err.contains("bytes left after the value"), "{tail:?}: {err}");
        }
    }

    #[test]
    fn a_cut_payload_is_an_error() {
        let bytes = to_vec(&sample()).unwrap();
        for cut in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(from_slice::<Sample>(&bytes[..cut]).is_err(), "cut at {cut} read as a value");
        }
    }
}
