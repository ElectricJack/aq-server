//! Pure validation of the published agent-access v1 boundary.
//! Shape/digest checks are not authentication, live authorization or graph effects.
pub mod result_requests;
use serde_json::Value;
use std::sync::OnceLock;
pub mod operation_receipt;
fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::from_str(include_str!("../artifacts/schema.json"))
            .expect("checked-in API schema")
    })
}
pub fn valid(name: &str, value: &Value) -> bool {
    schema()["$defs"]
        .get(name)
        .is_some_and(|definition| check(schema(), definition, value))
}
mod schema_validation;
pub use schema_validation::check;

/// Reject duplicate keys and nonintegral JSON numbers before canonical hashing.
pub struct StrictValue(pub Value);
impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("canonical JSON")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(StrictValue(v)) = a.next_element()? {
                    values.push(v);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut map = serde_json::Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if map.contains_key(&k) {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                    let StrictValue(v) = a.next_value()?;
                    map.insert(k, v);
                }
                Ok(StrictValue(Value::Object(map)))
            }
        }
        d.deserialize_any(Visitor)
    }
}

/// Maximum admitted agent gateway document, independently of any lower profile limit.
pub const MAX_DOCUMENT_BYTES: usize = 1_048_576;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidContract;
impl std::fmt::Display for InvalidContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid agent access contract")
    }
}
impl std::error::Error for InvalidContract {}
/// Duplicate keys, floating-point values and trailing data are refused before hashing.
pub fn parse(bytes: &[u8]) -> Result<Value, InvalidContract> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(InvalidContract);
    }
    serde_json::from_slice::<StrictValue>(bytes)
        .map(|v| v.0)
        .map_err(|_| InvalidContract)
}
pub fn canonical(value: &Value) -> Result<Vec<u8>, InvalidContract> {
    fn integers(v: &Value) -> bool {
        match v {
            Value::Number(n) => n.is_i64() || n.is_u64(),
            Value::Array(a) => a.iter().all(integers),
            Value::Object(o) => o.values().all(integers),
            _ => true,
        }
    }
    if !integers(value) {
        return Err(InvalidContract);
    }
    let mut value = value.clone();
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| InvalidContract)
}
pub fn digest(value: &Value) -> Result<String, InvalidContract> {
    use sha2::{Digest, Sha256};
    Ok(format!("{:x}", Sha256::digest(canonical(value)?)))
}
/// Immutable scope assertions; only the daemon authenticates current authority.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub task_id: uuid::Uuid,
    pub attempt_id: uuid::Uuid,
    pub session_id: uuid::Uuid,
    pub generation: u64,
    pub node_id: uuid::Uuid,
    pub boot_id: uuid::Uuid,
    pub project_id: uuid::Uuid,
    pub workspace_id: uuid::Uuid,
    pub resolved_spec_digest: String,
    /// The graph access profile, never an execution profile or composed prompt.
    pub profile_digest: String,
    /// The labelled graph snapshot, never the composed startup instructions.
    pub context_digest: String,
}
impl Binding {
    pub fn validate(&self) -> Result<(), InvalidContract> {
        let value = serde_json::to_value(self).map_err(|_| InvalidContract)?;
        if valid("Binding", &value) {
            Ok(())
        } else {
            Err(InvalidContract)
        }
    }
}
/// Digest-bound graph context. It grants no offline authority and holds no token.
#[derive(Clone)]
pub struct Context(Value);
impl Context {
    pub fn new(value: Value, expected: &Binding) -> Result<Self, InvalidContract> {
        expected.validate()?;
        if !valid("ContextResponse", &value) {
            return Err(InvalidContract);
        }
        let binding: Binding =
            serde_json::from_value(value["binding"].clone()).map_err(|_| InvalidContract)?;
        if binding != *expected {
            return Err(InvalidContract);
        }
        if digest(&value["profile"])? != expected.profile_digest
            || digest(&value["context"])? != expected.context_digest
        {
            return Err(InvalidContract);
        }
        let size = canonical(&value)?.len() as u64;
        let limit = value["profile"]["max_response_bytes"]
            .as_u64()
            .ok_or(InvalidContract)?;
        if size > limit {
            return Err(InvalidContract);
        }
        Ok(Self(value))
    }
    pub fn value(&self) -> &Value {
        &self.0
    }
}

#[cfg(test)]
mod reply_cursor_pattern_tests {
    #[test]
    fn reply_and_history_cursor_patterns_are_exact() {
        let reply =
            serde_json::json!({"type":"string","pattern":"^[1-9][0-9]{0,18}\\.[1-9][0-9]{0,18}$"});
        let either = serde_json::json!({"type":"string","pattern":"^[1-9][0-9]{0,18}(\\.[1-9][0-9]{0,18})?$"});
        let ok = |s: &serde_json::Value, t: &str| super::check(s, s, &serde_json::json!(t));
        assert!(ok(&reply, "1.1") && ok(&reply, "42.9999999999999999999"));
        for bad in [
            "1", "0.1", "1.0", "1.", ".1", "1.2.3", "01.1", "1.01", "a.1", "",
        ] {
            assert!(!ok(&reply, bad), "{bad}");
        }
        assert!(ok(&either, "7") && ok(&either, "7.3"));
        for bad in ["0", "07", "7.0", "7.", "7.3.1", "x"] {
            assert!(!ok(&either, bad), "{bad}");
        }
    }
}

#[cfg(test)]
mod human_digest_pattern_tests {
    #[test]
    fn prefixed_sha256_pattern_is_exact_and_existing_hex_remains_exact() {
        let prefixed = serde_json::json!({"type":"string","pattern":"^sha256:[0-9a-f]{64}$"});
        let hex = "a1".repeat(32);
        assert!(super::check(
            &prefixed,
            &prefixed,
            &serde_json::json!(format!("sha256:{hex}"))
        ));
        for value in [
            hex.clone(),
            format!("SHA256:{hex}"),
            format!("sha256:{}", hex.to_uppercase()),
            format!("sha256:{}", "g".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
            format!("sha256:{}", "a".repeat(65)),
        ] {
            assert!(!super::check(
                &prefixed,
                &prefixed,
                &serde_json::json!(value)
            ));
        }
        let original = serde_json::json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
        assert!(super::check(&original, &original, &serde_json::json!(hex)));
        assert!(!super::check(
            &original,
            &original,
            &serde_json::json!(format!("sha256:{hex}"))
        ));
    }
}

pub use operation_receipt as effects;
