//! Bounds for the scoped credential presence schemas.
use serde_json::{Map, Value};
type Object = Map<String, Value>;

fn visit(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                visit(child);
            }
            presence_definitions(object);
            properties(object);
        }
        Value::Array(array) => {
            for child in array {
                visit(child);
            }
        }
        _ => {}
    }
}

fn properties(object: &mut Object) {
    let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for (name, field) in properties {
        if let Some(field) = field.as_object_mut() {
            presence_property(name, field);
        }
    }
}
fn digest(object: &mut Object) {
    object.insert("pattern".into(), "^[0-9a-f]{64}$".into());
    object.insert("minLength".into(), 64.into());
    object.insert("maxLength".into(), 64.into());
    object.insert("x-utf8-byte-limit".into(), 64.into());
}
fn range(object: &mut Object, min: u64, max: u64) {
    object.insert("minimum".into(), min.into());
    object.insert("maximum".into(), max.into());
}
fn presence_definitions(object: &mut Object) {
    let Some(defs) = object.get_mut("$defs").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(text) = defs.get_mut("Text").and_then(Value::as_object_mut) {
        text.insert("minLength".into(), 1.into());
        text.insert("maxLength".into(), 512.into());
        text.insert("x-utf8-byte-limit".into(), 512.into());
    }
    if let Some(label) = defs.get_mut("Label").and_then(Value::as_object_mut) {
        label.insert("x-utf8-byte-limit".into(), 512.into());
    }
    if let Some(hash) = defs.get_mut("Digest").and_then(Value::as_object_mut) {
        digest(hash);
    }
}

pub(super) fn bounded(mut value: Value) -> Value {
    visit(&mut value);
    value["x-max-document-bytes"] = super::MAX_REQUEST_BYTES.into();
    value["x-relational-validation-required"] = true.into();
    value
}
fn presence_property(name: &str, field: &mut Object) {
    match name {
        n if n.ends_with("_ns") || n.ends_with("_at_ms") => range(field, 1, u64::MAX),
        "schema_version" => {
            field.insert("const".into(), 1.into());
        }
        "ttl_ms" | "maximum_ttl_ms" | "maximum_age_ms" => range(field, 1000, super::MAX_TTL_MS),
        "sequence" | "revision" | "grant_revision" => range(field, 1, i64::MAX as u64),
        "observed_material_bytes" => range(field, 1, 65536),
        "qualification" => {
            field.insert("properties".into(),serde_json::json!({"id":{"pattern":"^aq[.]scoped-material-presence-qualification/.+"}}));
        }
        "qualifications" => {
            field.insert("minItems".into(), 1.into());
            field.insert("maxItems".into(), 16.into());
        }
        "provisionable_only" => {
            field.insert("maxItems".into(), 32.into());
        }
        _ => {}
    }
}
