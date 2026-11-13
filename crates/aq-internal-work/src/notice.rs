//! What a supervisor is told about a failed run (spec 0012 §8 step 2).
use crate::{FAILURE_BURST_RUNS, MAX_DIAGNOSIS_BYTES, SUPERVISOR_ATTEMPTS, store::Row};
use serde::Serialize;
use std::fmt::Write as _;

/// The largest message the supervisor conversation accepts.
const MAX_NOTICE_BYTES: usize = 16 * 1024;
/// Longest attempt detail repeated in a notice.
const MAX_DETAIL_BYTES: usize = 500;
/// Where a supervisor reaches the runs routes.
const RUNS: &str = "/v1/supervisor/admin/api/internal-work/runs";

/// The purpose, the affected item, the reason, what the supervisor may do,
/// and each attempt's provider, model and detail. The run's instructions and
/// input are not repeated: the supervisor reads them with `GET {RUNS}/{id}`.
pub fn supervisor(row: &Row) -> String {
    let id = row.id;
    let mut text = String::new();
    let _ = writeln!(text, "Internal work failed and needs a supervisor.");
    let _ = writeln!(text, "Run: {id}");
    let _ = writeln!(text, "Purpose: {}", name(&row.purpose));
    let project = row
        .project_id
        .map(|p| format!(" in project {p}"))
        .unwrap_or_default();
    let _ = writeln!(
        text,
        "Affected item: {} {}{project}",
        row.affected_kind, row.affected_id
    );
    let reason = row.reason.as_ref().map_or_else(|| "unknown".into(), name);
    let detail = row.detail.as_deref().unwrap_or_default();
    let _ = writeln!(text, "Reason: {reason}: {}", clip(detail));
    if let Some(earlier) = row.retry_of {
        let _ = writeln!(text, "It retried run {earlier}.");
    } else if !row.escalate {
        let _ = writeln!(
            text,
            "This purpose failed at least {FAILURE_BURST_RUNS} times in ten minutes."
        );
    }
    let _ = writeln!(
        text,
        "Supervisor retries so far: {} of {SUPERVISOR_ATTEMPTS}.",
        row.supervisor_attempts
    );
    let _ = writeln!(text, "You may:");
    let _ = writeln!(text, "- Retry it: POST {RUNS}/{id}/retry");
    let _ = writeln!(
        text,
        "- Fix the cause in Settings first (a profile with no working model, a source that will not compile), then retry it."
    );
    let _ = writeln!(
        text,
        "- Escalate it to the owner: POST {RUNS}/{id}/escalate with {{\"diagnosis\": \"...\"}} (at most {MAX_DIAGNOSIS_BYTES} bytes)."
    );
    let _ = writeln!(
        text,
        "If nobody retries or escalates it within 30 minutes, it goes to the owner."
    );
    let _ = writeln!(text, "Attempts:");
    if row.attempts.is_empty() {
        let _ = writeln!(text, "- none: no candidate ran");
    }
    for attempt in &row.attempts {
        let reason = attempt
            .reason
            .as_ref()
            .map_or_else(|| "answered".into(), name);
        let line = format!(
            "- {} {}: {reason}: {}\n",
            attempt.provider,
            attempt.model,
            clip(&attempt.detail)
        );
        if text.len() + line.len() > MAX_NOTICE_BYTES {
            break;
        }
        text.push_str(&line);
    }
    text
}

/// The snake_case name serde gives a unit variant.
fn name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => "unknown".into(),
    }
}

fn clip(text: &str) -> &str {
    let mut end = text.len().min(MAX_DETAIL_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
