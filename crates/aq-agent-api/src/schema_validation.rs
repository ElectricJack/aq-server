mod collections;
mod text;
use serde_json::Value;

/// Validator for trusted embedded schemas only; not a general JSON Schema engine.
/// Supports the vocabulary in the versioned AQ and retained upstream contracts.
pub fn check(root: &Value, schema: &Value, value: &Value) -> bool {
    if let Some(reference) = schema["$ref"].as_str() {
        return reference
            .strip_prefix('#')
            .and_then(|p| root.pointer(p))
            .is_some_and(|target| check(root, target, value));
    }
    let allowed = schema.get("const").is_none_or(|c| c == value)
        && schema["enum"]
            .as_array()
            .is_none_or(|choices| choices.contains(value));
    allowed
        && check_branches(root, schema, value)
        && schema["type"]
            .as_str()
            .is_none_or(|kind| matches_type(kind, value))
        && match value {
            Value::Object(object) => collections::check_object(root, schema, object),
            Value::Array(array) => collections::check_array(root, schema, array),
            Value::String(text) => text::check_text(schema, text),
            Value::Number(_) => check_number(schema, value),
            _ => true,
        }
}

fn check_branches(root: &Value, schema: &Value, value: &Value) -> bool {
    if schema
        .get("not")
        .is_some_and(|branch| check(root, branch, value))
    {
        return false;
    }
    let matches = |branch: &Value| check(root, branch, value);
    schema["anyOf"]
        .as_array()
        .is_none_or(|branches| branches.iter().any(matches))
        && schema["oneOf"].as_array().is_none_or(|branches| {
            branches
                .iter()
                .filter(|branch| matches(branch))
                .take(2)
                .count()
                == 1
        })
        && schema["allOf"]
            .as_array()
            .is_none_or(|branches| branches.iter().all(matches))
        && schema.get("if").is_none_or(|condition| {
            let branch = if check(root, condition, value) {
                "then"
            } else {
                "else"
            };
            schema
                .get(branch)
                .is_none_or(|subschema| check(root, subschema, value))
        })
}

fn matches_type(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn within_length(length: usize, minimum: &Value, maximum: &Value) -> bool {
    minimum.as_u64().is_none_or(|n| length >= n as usize)
        && maximum.as_u64().is_none_or(|n| length <= n as usize)
}

fn check_number(schema: &Value, value: &Value) -> bool {
    if let Some(n) = value.as_u64() {
        return schema["minimum"].as_u64().is_none_or(|min| n >= min)
            && schema["maximum"].as_u64().is_none_or(|max| n <= max);
    }
    value
        .as_i64()
        .is_none_or(|n| schema["minimum"].as_i64().is_none_or(|min| n >= min))
}
