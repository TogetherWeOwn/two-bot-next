//! Refuse JSON documents that repeat an object key at any depth.
//!
//! `serde_json` (like `JSON.parse`) silently keeps the last value when an
//! object repeats a key, while other parsers keep the first or refuse, so a
//! body with a key twice can mean one thing to its sender and another here.
//! Every behavior-affecting parse of external bytes calls
//! [`reject_duplicate_keys`] after its normal decode: syntax and nesting depth
//! are already proven, so the only refusal left is a repeated key. Keys
//! compare after unescaping, so `"a"` and `"\u0061"` collide. The refusal is
//! one fixed error that never names the key: a body may carry a token.
//!
//! The fixed vocabulary is [`DUPLICATE_KEY_TOKEN`], shared by every guard
//! site. Internal actions keep their own older refusal class and are out of
//! scope here.

use std::collections::HashSet;
use std::fmt;

use serde::de::{MapAccess, SeqAccess};
use serde::Deserializer;

/// Fixed refusal vocabulary: every duplicate-key guard's `Display` contains
/// this token, and no guard rendering carries key, value, or body bytes.
pub const DUPLICATE_KEY_TOKEN: &str = "duplicate JSON object key";

/// A JSON document repeated an object key. Carries no key, value, or body
/// bytes; see [`reject_duplicate_keys`] for the call contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateKeyError;

impl fmt::Display for DuplicateKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("document contains a duplicate JSON object key")
    }
}

impl std::error::Error for DuplicateKeyError {}

/// Refuse `raw` when it repeats an object key at any depth.
///
/// Call only with bytes that already decoded: the caller's normal parse runs
/// first and keeps its own syntax errors, so any failure of the uniqueness
/// pass is a repeated key. Fails closed on anything it cannot positively
/// clear. The decoded value still comes from the caller's own parse, so
/// unique documents behave byte-identically.
pub fn reject_duplicate_keys(raw: &[u8]) -> Result<(), DuplicateKeyError> {
    if serde_json::from_slice::<UniqueKeys>(raw).is_err() {
        return Err(DuplicateKeyError);
    }
    Ok(())
}

/// Walks one JSON value without building it and fails on the first object
/// that repeats a key. The error text is fixed and never reaches a caller.
struct UniqueKeys;

impl<'de> serde::Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueKeys)
    }
}

impl<'de> serde::de::Visitor<'de> for UniqueKeys {
    type Value = Self;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<UniqueKeys>()?.is_some() {}
        Ok(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key) {
                return Err(serde::de::Error::custom("duplicate object key"));
            }
            map.next_value::<UniqueKeys>()?;
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_documents_pass() {
        for raw in [
            r#"{"a":1,"b":-2,"c":1.5,"d":true,"e":null,"f":"s","g":[1,{"h":2}]}"#,
            "[1,2,3]",
            r#""just a string""#,
            "42",
            "true",
            "null",
            "{}",
            // The same key in sibling objects is not a repeat.
            r#"{"a":{"x":1},"b":{"x":2}}"#,
            r#"[{"x":1},{"x":1}]"#,
        ] {
            assert!(
                reject_duplicate_keys(raw.as_bytes()).is_ok(),
                "unique document must pass: {raw}"
            );
        }
    }

    #[test]
    fn repeated_keys_refuse_with_the_fixed_error() {
        for raw in [
            r#"{"a":1,"a":2}"#,
            r#"{"a":1,"a":1}"#,
            r#"{"a":{"x":1,"x":1}}"#,
            r#"{"a":[{"x":1,"x":2}]}"#,
            "{\"a\":1,\"\\u0061\":2}",
            r#"{"s":7,"t":"x","s":8}"#,
        ] {
            let error = reject_duplicate_keys(raw.as_bytes()).expect_err("must refuse");
            assert_eq!(
                error.to_string(),
                "document contains a duplicate JSON object key"
            );
            assert!(format!("{error:?}").len() < 1024, "refusal stays bounded");
        }
    }
}
