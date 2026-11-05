//! Pure validation of the published agent-access v1 boundary.
//! Shape/digest checks are not authentication, live authorization or graph effects.
use serde_json::Value;
use std::sync::OnceLock;
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
/// Validator for trusted embedded schemas only; not a general JSON Schema engine.
/// Supports the vocabulary in the versioned AQ and retained upstream contracts.
pub fn check(root: &Value, s: &Value, v: &Value) -> bool {
    if let Some(r) = s["$ref"].as_str() {
        return r
            .strip_prefix('#')
            .and_then(|p| root.pointer(p))
            .is_some_and(|s| check(root, s, v));
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        return false;
    }
    if let Some(es) = s["enum"].as_array()
        && !es.contains(v)
    {
        return false;
    }
    if let Some(n) = s.get("not")
        && check(root, n, v)
    {
        return false;
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = s[key].as_array() {
            let count = branches.iter().filter(|b| check(root, b, v)).count();
            if match key {
                "anyOf" => count == 0,
                "oneOf" => count != 1,
                _ => count != branches.len(),
            } {
                return false;
            }
        }
    }
    if let Some(cond) = s.get("if") {
        let branch = if check(root, cond, v) { "then" } else { "else" };
        if let Some(b) = s.get(branch)
            && !check(root, b, v)
        {
            return false;
        }
    }
    if let Some(t) = s["type"].as_str()
        && !match t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "integer" => v.as_i64().is_some() || v.as_u64().is_some(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            _ => false,
        }
    {
        return false;
    }
    if let Some(obj) = v.as_object() {
        if s["minProperties"]
            .as_u64()
            .is_some_and(|n| obj.len() < n as usize)
        {
            return false;
        }
        if let Some(required) = s["required"].as_array()
            && required
                .iter()
                .any(|k| !obj.contains_key(k.as_str().unwrap_or("")))
        {
            return false;
        }
        for (k, val) in obj {
            if let Some(prop) = s["properties"].get(k) {
                if !check(root, prop, val) {
                    return false;
                }
            } else if s["additionalProperties"] == false {
                return false;
            }
        }
    }
    if let Some(a) = v.as_array() {
        if let Some(item) = s.get("contains")
            && !a.iter().any(|v| check(root, item, v))
        {
            return false;
        }
        if s["minItems"].as_u64().is_some_and(|n| a.len() < n as usize)
            || s["maxItems"].as_u64().is_some_and(|n| a.len() > n as usize)
        {
            return false;
        }
        if s["uniqueItems"] == true && a.iter().enumerate().any(|(i, x)| a[..i].contains(x)) {
            return false;
        }
        if let Some(item) = s.get("items")
            && a.iter().any(|v| !check(root, item, v))
        {
            return false;
        }
    }
    if let Some(t) = v.as_str() {
        let n = t.chars().count();
        if s["minLength"].as_u64().is_some_and(|m| n < m as usize)
            || s["maxLength"].as_u64().is_some_and(|m| n > m as usize)
            || s["x-utf8-maxBytes"]
                .as_u64()
                .is_some_and(|m| t.len() > m as usize)
        {
            return false;
        }
        if s["x-int64-token"] == true
            && !(t.parse::<i64>().is_ok_and(|i| i > 0)
                && !t.starts_with('0')
                && t.bytes().all(|b| b.is_ascii_digit()))
        {
            return false;
        }
        if let Some(p) = s["pattern"].as_str()
            && !match p {
                "^https?://" => t.starts_with("https://") || t.starts_with("http://"),
                "^([0-9a-f]{40}|[0-9a-f]{64})$" => {
                    matches!(t.len(), 40 | 64)
                        && t.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }
                "\\S" => t.chars().any(|c| !c.is_whitespace()),
                "^sha256:[0-9a-f]{64}$" => t.strip_prefix("sha256:").is_some_and(|hex| {
                    hex.len() == 64
                        && hex
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                }),
                "^[0-9a-f]{64}$" => {
                    t.len() == 64
                        && t.bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                }
                "^[0-9a-f-]{36}$" => {
                    t.len() == 36
                        && t.bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || c == b'-')
                }
                "^[1-9][0-9]{0,18}$" => {
                    !t.starts_with('0')
                        && (1..=19).contains(&t.len())
                        && t.bytes().all(|c| c.is_ascii_digit())
                }
                // Conversation reply cursor `A.R`, and a history cursor that is
                // either an owner cursor `N` or a reply cursor `A.R`.
                "^[1-9][0-9]{0,18}\\.[1-9][0-9]{0,18}$" => t
                    .split_once('.')
                    .is_some_and(|(a, r)| positive_token(a) && positive_token(r)),
                "^[1-9][0-9]{0,18}(\\.[1-9][0-9]{0,18})?$" => match t.split_once('.') {
                    Some((a, r)) => positive_token(a) && positive_token(r),
                    None => positive_token(t),
                },
                "^(0|[1-9][0-9]{0,18})$" => {
                    t == "0"
                        || (!t.starts_with('0')
                            && (1..=19).contains(&t.len())
                            && t.bytes().all(|c| c.is_ascii_digit()))
                }
                _ => false,
            }
        {
            return false;
        }
        if s["format"] == "uuid" && !uuid::Uuid::parse_str(t).is_ok_and(|u| u.to_string() == t) {
            return false;
        }
        if s["format"] == "date-time" && chrono::DateTime::parse_from_rfc3339(t).is_err() {
            return false;
        }
    }
    if let Some(n) = v.as_u64() {
        if s["minimum"].as_u64().is_some_and(|m| n < m)
            || s["maximum"].as_u64().is_some_and(|m| n > m)
        {
            return false;
        }
    } else if let Some(n) = v.as_i64()
        && s["minimum"].as_i64().is_some_and(|m| n < m)
    {
        return false;
    }
    true
}

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
        if !valid("ContextResponse", &value)
            || serde_json::from_value::<Binding>(value["binding"].clone())
                .map_err(|_| InvalidContract)?
                != *expected
            || digest(&value["profile"])? != expected.profile_digest
            || digest(&value["context"])? != expected.context_digest
            || canonical(&value)?.len() as u64
                > value["profile"]["max_response_bytes"]
                    .as_u64()
                    .ok_or(InvalidContract)?
        {
            return Err(InvalidContract);
        }
        Ok(Self(value))
    }
    pub fn value(&self) -> &Value {
        &self.0
    }
}

/// `^[1-9][0-9]{0,18}$`.
fn positive_token(t: &str) -> bool {
    !t.starts_with('0') && (1..=19).contains(&t.len()) && t.bytes().all(|c| c.is_ascii_digit())
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
