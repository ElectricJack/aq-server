//! Pure values the host adapters share (catalogs 1.0.0 to 2.3.0): a command's
//! outcome, subjects and handles, input checks and the completion of a
//! tracked work node. No I/O.
use crate::{Error, Result};
use serde_json::{Value, json};
use uuid::Uuid;

/// A catalog outcome and its schema-validated output.
#[derive(Clone, Debug, PartialEq)]
pub struct HostResult {
    pub outcome: String,
    pub output: Value,
}

/// The handle prefix of a tracked work node.
pub const WORK_REF: &str = "aq.playbook.work/";

pub fn subject(id: impl ToString, revision: i64) -> Value {
    json!({"id": id.to_string(), "revision": revision.max(0)})
}
pub fn text<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input[field].as_str().ok_or(Error::Invalid)
}
pub fn work_output(task: Uuid, revision: i64) -> Value {
    json!({"reference": format!("{WORK_REF}{task}"), "subject": subject(task, revision)})
}
pub fn node_version(node: &Value) -> Result<i64> {
    node["version"]
        .as_i64()
        .filter(|v| *v >= 1)
        .ok_or(Error::Unknown)
}

pub fn source_valid(source: &Value) -> bool {
    let want = match source["format"].as_str() {
        Some("sha1") => 40,
        Some("sha256") => 64,
        _ => return false,
    };
    source["hex"].as_str().is_some_and(|h| {
        h.len() == want
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Choices a Host7 decision may offer: the catalog's human verdicts.
pub const DECISION_OUTCOMES: [&str; 2] = ["approved", "rejected"];

/// `user:<uuid>`: the only responder form a Host7 decision accepts.
pub fn responder(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value.strip_prefix("user:")?).ok()
}

pub fn revision_of(subject: &Value) -> Result<i64> {
    subject["revision"]
        .as_i64()
        .filter(|r| *r >= 0)
        .ok_or(Error::Invalid)
}

/// Terminal completion of one tracked work node, or `None` while it is open.
/// A completed task is `completed`: review work carries its disposition in
/// `aq:review_disposition` (`accepted` / `changes_requested`) and acceptance is
/// never inferred, so a missing or unknown disposition is `failed`; other
/// kinds are `accepted`. A canceled, retired, deleted or missing task is
/// `refused` / `failed`.
pub fn work_completion(request: &Value, node: Option<&Value>) -> Result<Option<HostResult>> {
    let (outcome, disposition, revision) = match node {
        None => ("refused", "failed", revision_of(&request["subject"])?),
        Some(node) => {
            let revision = node_version(node)?;
            if node["deleted"] == json!(true)
                || node["retired"] == json!(true)
                || node["task_state"] == "canceled"
            {
                ("refused", "failed", revision)
            } else if node["task_state"] == "completed" {
                let disposition = match request["kind"].as_str() {
                    Some("review" | "fidelity_review") => node["metadata"]["aq:review_disposition"]
                        .as_str()
                        .filter(|d| matches!(*d, "accepted" | "changes_requested"))
                        .unwrap_or("failed"),
                    _ => "accepted",
                };
                ("completed", disposition, revision)
            } else {
                return Ok(None);
            }
        }
    };
    Ok(Some(HostResult {
        outcome: outcome.into(),
        output: json!({"disposition": disposition, "reference": request["reference"],
            "subject": {"id": request["subject"]["id"], "revision": revision}}),
    }))
}

pub fn handle(id: &str, prefix: &str) -> Option<Uuid> {
    Uuid::parse_str(id.strip_prefix(prefix)?).ok()
}
/// A bounded typed reason from the port; anything else maps to `fallback`.
pub fn token(reason: &str, fallback: &str) -> String {
    if (1..=64).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        reason.into()
    } else {
        fallback.into()
    }
}
pub fn same_subject(a: &Value, id: &str, revision: i64) -> bool {
    a["id"].as_str() == Some(id) && a["revision"].as_i64() == Some(revision)
}
pub fn merged(base: &Value, extra: Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    if let Value::Object(extra) = extra {
        out.extend(extra);
    }
    Value::Object(out)
}
