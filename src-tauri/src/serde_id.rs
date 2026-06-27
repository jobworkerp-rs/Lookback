//! Serialize `i64` IDs as JSON strings.
//!
//! memories' Thread/Memory ids are 64-bit snowflakes (e.g. 7_462_752_159_340_220_411)
//! that overflow JavaScript's `Number.MAX_SAFE_INTEGER` (≈9.0e15). Passing
//! them through the Tauri IPC as numbers silently truncates the low bits,
//! so any subsequent `find_memories_by_thread_id(thread.id)` round-trip
//! lands on a non-existent thread. Routing IDs through strings on the wire
//! and converting on both sides keeps the values intact.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<S: Serializer>(value: &i64, s: S) -> Result<S::Ok, S::Error> {
    value.to_string().serialize(s)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    String::deserialize(d)?
        .parse::<i64>()
        .map_err(serde::de::Error::custom)
}

pub mod option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => v.to_string().serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        opt.map(|s| s.parse::<i64>().map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip_max_i64() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
        struct Wrap(#[serde(with = "super")] i64);
        let v = Wrap(7_462_752_159_340_220_411);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"7462752159340220411\"");
        let back: Wrap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn option_handles_none() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
        struct Wrap(#[serde(with = "super::option")] Option<i64>);
        let v = Wrap(None);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "null");
        let back: Wrap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }
}
