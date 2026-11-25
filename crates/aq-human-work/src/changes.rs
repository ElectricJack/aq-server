use crate::{MAX_SUMMARY, STATES};
use serde_json::{Value, json};
use uuid::Uuid;
/// Revision fields a history diff names, in display order. `priority` is the
/// `aq:priority` metadata key and `role` the `aq:role` key (spec 0014 §5.5);
/// `metadata` stands for any other metadata key.
const FIELDS: [&str; 17] = [
    "title",
    "body",
    "acceptance_criteria",
    "labels",
    "priority",
    "role",
    "metadata",
    "task_state",
    "provenance",
    "content_type",
    "retired",
    "retirement_reason",
    "successor_id",
    "valid_from",
    "valid_until",
    "review_at",
    "deleted",
];

/// The task's node snapshot that a revision entry carries, with its revision.
fn snapshot(entry: &Value, task: Uuid) -> Option<(&Value, u64)> {
    let d = &entry["data"];
    if entry["category"] != "revision" || d["id"] != json!(task) {
        return None;
    }
    Some((d, d["content_revision"].as_u64().filter(|n| *n > 0)?))
}

/// The content revision the oldest revision entry on a history page needs as
/// its predecessor, when the page does not hold that snapshot.
pub fn predecessor(items: &[Value], task: Uuid) -> Option<u64> {
    let held: Vec<u64> = items
        .iter()
        .filter_map(|e| snapshot(e, task).map(|s| s.1))
        .collect();
    items
        .iter()
        .filter(|e| e["operation"] != "created")
        .filter_map(|e| snapshot(e, task).map(|s| s.1))
        .filter(|r| *r > 1 && !held.contains(&(r - 1)))
        .min()
        .map(|r| r - 1)
}

/// One change summary per history entry, in order, or null when an entry
/// carries nothing AQ can describe. `prior` is the node snapshot at the
/// revision [`predecessor`] named, when it could be read.
///
/// A change is `{summary, fields, edge, complete}`. `fields` lists changed
/// fields as `{field, from, to}`; values are given only for task state,
/// priority, role, content type, retired and deleted, and are null otherwise.
/// `edge` is `{edge_type, direction, node_id}` for a dependency or link
/// change. `complete` is false when the previous revision was unavailable.
pub fn changes(items: &[Value], task: Uuid, prior: Option<&Value>) -> Vec<Value> {
    items
        .iter()
        .map(|e| {
            let op = e["operation"].as_str().unwrap_or("");
            match e["category"].as_str() {
                Some("lifecycle") => lifecycle(op, &e["data"]),
                Some("edge") => linked(op, &e["data"], task),
                Some("revision") => revision(items, e, task, prior),
                _ => None,
            }
            .unwrap_or(Value::Null)
        })
        .collect()
}

fn summary(text: String) -> String {
    text.chars().take(MAX_SUMMARY).collect()
}
fn field(name: &str, from: Value, to: Value) -> Value {
    json!({"field":name,"from":from,"to":to})
}
fn change(text: String, fields: Vec<Value>, edge: Value, complete: bool) -> Option<Value> {
    Some(json!({"summary":summary(text),"fields":fields,"edge":edge,"complete":complete}))
}
/// A task state (or null) as shown in a change; None for anything else.
fn state(v: &Value) -> Option<Value> {
    match v {
        Value::Null => Some(Value::Null),
        _ => STATES[..4].iter().find(|s| v == **s).map(|s| json!(s)),
    }
}
fn shown(v: &Value) -> String {
    v.as_str().unwrap_or("none").to_owned()
}

fn lifecycle(op: &str, d: &Value) -> Option<Value> {
    match op {
        "created" | "transitioned" => {
            let (from, to) = (state(&d["from"])?, state(&d["to"])?);
            let text = if op == "created" {
                format!("created as {}", shown(&to))
            } else {
                format!("{} → {}", shown(&from), shown(&to))
            };
            change(text, vec![field("task_state", from, to)], Value::Null, true)
        }
        "deleted" if d["deleted"] == true => change(
            "deleted".into(),
            vec![field("deleted", json!("false"), json!("true"))],
            Value::Null,
            true,
        ),
        "restored" => {
            let from = d["from_revision"].as_u64().filter(|n| *n > 0)?;
            let to = state(&d["task_state"])?;
            change(
                format!("restored revision {from}"),
                vec![field("task_state", Value::Null, to)],
                Value::Null,
                true,
            )
        }
        _ => None,
    }
}

fn linked(op: &str, d: &Value, task: Uuid) -> Option<Value> {
    let verb = matches!(op, "added" | "removed").then_some(op)?;
    let kind = d["edge_type"].as_str().filter(|t| {
        !t.is_empty() && t.len() <= 64 && t.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
    })?;
    let id = |k: &str| d[k].as_str()?.parse::<Uuid>().ok();
    let (source, target) = (id("source")?, id("target")?);
    let (direction, other, word) = if target == task {
        ("incoming", source, "from")
    } else if source == task {
        ("outgoing", target, "to")
    } else {
        return None;
    };
    let short = &other.to_string()[..8];
    change(
        format!("{verb} {kind} {word} {short}"),
        vec![],
        json!({"edge_type":kind,"direction":direction,"node_id":other}),
        true,
    )
}

fn revision(items: &[Value], e: &Value, task: Uuid, prior: Option<&Value>) -> Option<Value> {
    let op = e["operation"].as_str().unwrap_or("");
    let (after, r) = snapshot(e, task)?;
    if op == "created" {
        return change("created".into(), vec![], Value::Null, true);
    }
    let previous = items
        .iter()
        .filter_map(|i| snapshot(i, task))
        .find(|(_, n)| *n + 1 == r)
        .map(|(s, _)| s)
        .or(prior
            .filter(|p| p["id"] == json!(task) && p["content_revision"].as_u64() == Some(r - 1)));
    let label = op.replace('_', " ");
    let Some(before) = previous else {
        return change(
            format!("{label}; earlier revision unavailable"),
            vec![],
            Value::Null,
            false,
        );
    };
    let fields = diff(before, after);
    let text = if fields.is_empty() {
        label
    } else {
        fields
            .iter()
            .map(|f| {
                let name = f["field"].as_str().unwrap_or("").replace('_', " ");
                match (&f["from"], &f["to"]) {
                    (Value::Null, Value::Null) => name,
                    (a, b) => format!("{name} {} → {}", shown(a), shown(b)),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    change(text, fields, Value::Null, true)
}

/// Changed fields between two node snapshots of the same task.
fn diff(before: &Value, after: &Value) -> Vec<Value> {
    FIELDS
        .iter()
        .filter_map(|name| {
            let a = field_value(name, before);
            let b = field_value(name, after);
            (a != b).then(|| field(name, visible_value(name, &a), visible_value(name, &b)))
        })
        .collect()
}

fn field_value(name: &str, node: &Value) -> Value {
    match name {
        "priority" => node["metadata"]["aq:priority"].clone(),
        "role" => node["metadata"]["aq:role"].clone(),
        "metadata" => {
            let mut metadata = node["metadata"].as_object().cloned().unwrap_or_default();
            metadata.remove("aq:priority");
            metadata.remove("aq:role");
            Value::Object(metadata)
        }
        _ => node[name].clone(),
    }
}

fn visible_value(name: &str, value: &Value) -> Value {
    match name {
        "role" => value
            .as_str()
            .filter(|s| {
                valid_code(s, |b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
                })
            })
            .map_or(Value::Null, |s| json!(s)),
        "task_state" | "priority" | "content_type" | "retired" | "deleted" => match value {
            Value::Bool(b) => json!(b.to_string()),
            Value::Number(n) if n.is_i64() => json!(n.to_string()),
            Value::String(s) if valid_code(s, |b| b.is_ascii_lowercase() || b == b'_') => json!(s),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}
fn valid_code(s: &str, chars: impl Fn(u8) -> bool) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(chars)
}
