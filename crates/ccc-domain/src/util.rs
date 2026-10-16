//! Canonical JSON serialization used for stable hashes.
//!
//! `serde_json` maps objects to BTreeMap by default, so object keys come out
//! sorted. Hash inputs must not contain floats (plan T13); callers quantize
//! scores to integers before hashing.

use serde::Serialize;
use serde_json::Value;

/// Serializes a value as canonical JSON: sorted object keys, compact
/// separators. Deterministic across processes and platforms for the same
/// input value.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    let json: Value = serde_json::to_value(value)?;
    serde_json::to_string(&json)
}

/// Hex-encoded BLAKE3 digest of a byte slice.
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_string()
}

/// Current UTC time as an RFC 3339 string (all persisted timestamps use this).
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_object_keys() {
        let value = json!({"zeta": 1, "alpha": {"y": 2, "b": 3}});
        let canonical = canonical_json(&value).unwrap();
        assert_eq!(canonical, r#"{"alpha":{"b":3,"y":2},"zeta":1}"#);
    }

    #[test]
    fn canonical_json_is_stable_across_reloads() {
        let value = json!({"a": [1, 2, 3], "b": "text", "c": null, "d": true});
        let first = canonical_json(&value).unwrap();
        let second = canonical_json(&value).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn blake3_hex_is_stable_and_hex() {
        let digest = blake3_hex(b"traceweave");
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, blake3_hex(b"traceweave"));
        assert_ne!(digest, blake3_hex(b"traceweave "));
    }
}
