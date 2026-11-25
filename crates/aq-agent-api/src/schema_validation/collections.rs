use super::{check, within_length};
use serde_json::{Map, Value};

pub(super) fn check_object(root: &Value, schema: &Value, object: &Map<String, Value>) -> bool {
    if schema["minProperties"]
        .as_u64()
        .is_some_and(|n| object.len() < n as usize)
    {
        return false;
    }
    let has_required = schema["required"].as_array().is_none_or(|required| {
        required
            .iter()
            .all(|name| object.contains_key(name.as_str().unwrap_or("")))
    });
    has_required
        && object
            .iter()
            .all(|(key, value)| match schema["properties"].get(key) {
                Some(property) => check(root, property, value),
                None => schema["additionalProperties"] != false,
            })
}

pub(super) fn check_array(root: &Value, schema: &Value, array: &[Value]) -> bool {
    let contains = schema
        .get("contains")
        .is_none_or(|item| array.iter().any(|value| check(root, item, value)));
    if !contains {
        return false;
    }
    if !within_length(array.len(), &schema["minItems"], &schema["maxItems"]) {
        return false;
    }
    if schema["uniqueItems"] == true
        && array
            .iter()
            .enumerate()
            .any(|(i, item)| array[..i].contains(item))
    {
        return false;
    }
    schema
        .get("items")
        .is_none_or(|item| array.iter().all(|value| check(root, item, value)))
}
