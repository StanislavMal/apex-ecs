//! The binary encoding of the core and the engine: `postcard` 1.x, whose wire format is stable
//! across the 1.x line by its specification (core ADR-018).
//!
//! **Every binary write goes through [`to_vec`]** — components, resources, events between worlds,
//! snapshots, the engine's shader cache — so the way bytes are produced is decided once.

pub use postcard::{from_bytes, take_from_bytes, Error};

/// Serialize `value` into an exactly-sized vector: the size is computed first, then the value is
/// written once into that buffer.
///
/// Not `postcard::to_allocvec`: it grows its vector as bytes arrive, and on the small values this
/// code writes most (an event, a component) the reallocations ARE the cost. Measured on the shapes
/// of `apex-bench --bin serialization_load` (engine TD-608), medians of 9: 200 000 events of 15 bytes
/// 12.0 ms through `to_allocvec`, 7.6 ms sized first (`bincode`, which also sizes first, 5.8 ms);
/// 20 000 string events 1.85 → 0.67 ms; 20 000 components 2.79 → 0.93 ms.
pub fn to_vec<T: serde::Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let size = postcard::serialize_with_flavor::<T, postcard::ser_flavors::Size, usize>(
        value,
        Default::default(),
    )?;
    let mut out = vec![0u8; size];
    let written = postcard::to_slice(value, &mut out)?.len();
    debug_assert_eq!(written, size, "the size pass and the write disagree");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sample {
        id: u64,
        name: String,
        weights: Vec<f32>,
    }

    #[test]
    fn the_sized_write_is_the_postcard_encoding() {
        let v = Sample { id: 300, name: "a name".into(), weights: vec![0.5, 1.5, -2.0] };
        let bytes = super::to_vec(&v).unwrap();
        assert_eq!(bytes, postcard::to_allocvec(&v).unwrap());
        assert_eq!(super::from_bytes::<Sample>(&bytes).unwrap(), v);
    }
}
