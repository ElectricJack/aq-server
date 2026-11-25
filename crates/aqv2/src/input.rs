//! Command input: params from flags, files and stdin. Inputs are bounded
//! (64 KiB), files must be regular non-symlink files, and JSON objects must
//! not repeat a key.
use crate::{Result, error};
use serde_json::{Map, Value, json};
use std::io::Read;

const MAX: usize = 65536;

fn invalid(message: &str) -> Value {
    error("invalid_arguments", message)
}
fn stdin() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("Could not read stdin"))?;
    if bytes.len() > MAX {
        return Err(invalid("stdin is larger than 64 KiB"));
    }
    Ok(bytes)
}
fn unique(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice::<crate::json::UniqueJson>(bytes)
        .map(|v| v.0)
        .map_err(|_| invalid("Expected JSON with unique object keys"))
}
/// A JSON document from a file or `-` (stdin).
pub fn json_source(source: &str) -> Result<Value> {
    if source == "-" {
        return unique(&stdin()?);
    }
    crate::actions::payload(std::path::Path::new(source))
}
/// A JSON object from a file or `-`.
pub fn object_source(source: &str) -> Result<Map<String, Value>> {
    match json_source(source)? {
        Value::Object(m) => Ok(m),
        _ => Err(invalid("Expected a JSON object")),
    }
}
/// A text argument; `-` reads it from stdin.
pub fn text(value: &str) -> Result<String> {
    if value != "-" {
        return Ok(value.to_owned());
    }
    let bytes = stdin()?;
    let text = String::from_utf8(bytes).map_err(|_| invalid("stdin is not UTF-8"))?;
    Ok(text.strip_suffix('\n').unwrap_or(&text).to_owned())
}
/// `--param KEY=VALUE` (a string) and `--param KEY:=JSON` (any JSON value),
/// merged over an optional `--params-file` object.
pub fn params(file: Option<&str>, pairs: &[String]) -> Result<Map<String, Value>> {
    let mut params = match file {
        Some(source) => object_source(source)?,
        None => Map::new(),
    };
    let mut seen = std::collections::BTreeSet::new();
    for pair in pairs {
        let (key, value) = parse_pair(pair)?;
        if !seen.insert(key.to_owned()) {
            return Err(invalid("A --param key is given twice"));
        }
        params.insert(key.to_owned(), value);
    }
    Ok(params)
}
fn parse_pair(pair: &str) -> Result<(&str, Value)> {
    let (key, value) = pair
        .split_once('=')
        .ok_or_else(|| invalid("--param takes KEY=VALUE or KEY:=JSON"))?;
    let (key, value) = match key.strip_suffix(':') {
        Some(key) => (key, unique(value.as_bytes())?),
        None => (key, json!(value)),
    };
    let valid_length = (1..=64).contains(&key.len());
    let valid_chars = key
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid_length || !valid_chars {
        return Err(invalid("A --param key is lowercase letters, digits and _"));
    }
    Ok((key, value))
}
/// `--subject TYPE:ID[:REVISION]`.
pub fn subject(value: &str) -> Result<Value> {
    let mut parts = value.splitn(3, ':');
    let (Some(kind), Some(id)) = (parts.next(), parts.next()) else {
        return Err(invalid("--subject takes TYPE:ID[:REVISION]"));
    };
    if kind.is_empty() || id.is_empty() {
        return Err(invalid("--subject takes TYPE:ID[:REVISION]"));
    }
    let mut subject = json!({"type":kind,"id":id});
    if let Some(revision) = parts.next() {
        subject["revision"] = json!(revision);
    }
    Ok(subject)
}
/// `--expect TYPE=REVISION`, repeatable.
pub fn expected(values: &[String]) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    for v in values {
        let (k, r) = v
            .split_once('=')
            .filter(|(k, r)| !k.is_empty() && !r.is_empty())
            .ok_or_else(|| invalid("--expect takes TYPE=REVISION"))?;
        out.insert(k.to_owned(), json!(r));
    }
    Ok(out)
}
