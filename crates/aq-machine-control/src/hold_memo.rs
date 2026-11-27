//! A held task's scheduling assessment, remembered between polls.
//!
//! Every slot polls about every ten seconds, and each poll used to run the whole
//! replacement/delivery assessment (some twenty statements) for every held ready task, so an
//! idle fleet's cost grew with held tasks x slots. A task whose assessment is `Blocked` stays
//! blocked until something is written about it, so the blocked reason is remembered together
//! with a fingerprint of the rows the assessment reads ([`fingerprint`]). Each poll still
//! reads the fingerprint (one statement); the remembered reason is reused only while the
//! fingerprint, the project binding revision and [`REASSESS_MS`] all still hold. Any change
//! to the evidence therefore sends the very next poll through the full assessment, and
//! evidence outside the fingerprint (a running delivery operation, a deadline that passes) is
//! picked up when the remembered reason expires.
use sqlx::PgPool;
use std::{collections::BTreeMap, sync::Mutex};
use uuid::Uuid;

/// How long a remembered reason is trusted: the slot poll interval (`IDLE_POLL_INTERVAL`).
pub const REASSESS_MS: u64 = 10_000;
/// Held tasks remembered at once; past this the memory starts over.
const MAX_REMEMBERED: usize = 4096;

#[derive(Clone)]
struct Entry {
    reason: &'static str,
    revision: u64,
    fingerprint: String,
    assessed_at_ms: u64,
}
static MEMORY: Mutex<BTreeMap<Uuid, Entry>> = Mutex::new(BTreeMap::new());

/// The rows a replacement/delivery assessment of `task` reads, as the transaction ids that
/// wrote them: any insert, update or delete of one of them changes the text. None when the
/// store cannot answer (an older schema), which means "assess in full".
pub async fn fingerprint(pool: &PgPool, task: Uuid, project: Uuid) -> Option<String> {
    const SQL: &str = "WITH a AS (SELECT attempt_id FROM execution_attempts WHERE task_id=$1) SELECT concat_ws('|',\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM execution_attempts WHERE task_id=$1),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_authority_hot WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_launches WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_replacement_releases WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_settlement_evaluations WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_cancelled_settlements WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_task_completions WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_result_heads WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_auto_settlements WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM attempt_handoffs WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM lost_attempt_releases WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM task_fresh_starts WHERE attempt_id IN (SELECT attempt_id FROM a)),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM task_limit_overrides WHERE task_id=$1),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM delivery_decisions WHERE task_id=$1),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM stall_quarantines WHERE lineage_id=$1),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM profiled_stopped_dispositions WHERE task_id=$1),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM project_execution_bindings WHERE project_id=$2),\
 (SELECT string_agg(xmin::text,',' ORDER BY xmin::text) FROM role_settings))";
    sqlx::query_scalar(SQL)
        .bind(task)
        .bind(project)
        .fetch_one(pool)
        .await
        .inspect_err(|e| tracing::debug!(%task, error = ?e, "hold evidence fingerprint unreadable"))
        .ok()
}

/// The reason `task` was found blocked, if the evidence is unchanged since and the reason is
/// younger than [`REASSESS_MS`].
pub fn recall(task: Uuid, revision: u64, fingerprint: &str, now_ms: u64) -> Option<&'static str> {
    let memory = MEMORY.lock().ok()?;
    let entry = memory.get(&task)?;
    (entry.revision == revision
        && entry.fingerprint == fingerprint
        && now_ms.saturating_sub(entry.assessed_at_ms) < REASSESS_MS
        && now_ms >= entry.assessed_at_ms)
        .then_some(entry.reason)
}

/// Remember that `task` was assessed blocked for `reason` with this evidence.
pub fn remember(task: Uuid, revision: u64, fingerprint: String, reason: &'static str, now_ms: u64) {
    if let Ok(mut memory) = MEMORY.lock() {
        if memory.len() >= MAX_REMEMBERED && !memory.contains_key(&task) {
            memory.clear();
        }
        memory.insert(
            task,
            Entry {
                reason,
                revision,
                fingerprint,
                assessed_at_ms: now_ms,
            },
        );
    }
}

/// `task` is no longer blocked: drop what was remembered.
pub fn forget(task: Uuid) {
    if let Ok(mut memory) = MEMORY.lock() {
        memory.remove(&task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_is_reused_only_for_unchanged_evidence_and_revision() {
        let task = Uuid::new_v4();
        assert_eq!(recall(task, 3, "a", 1_000), None);
        remember(task, 3, "a".into(), "stall_quarantined", 1_000);
        assert_eq!(recall(task, 3, "a", 1_000), Some("stall_quarantined"));
        assert_eq!(
            recall(task, 3, "a", 1_000 + REASSESS_MS - 1),
            Some("stall_quarantined")
        );
        // New evidence, a new binding revision, an expired reason or a clock that went back:
        // the next poll assesses in full.
        assert_eq!(recall(task, 3, "b", 1_000), None);
        assert_eq!(recall(task, 4, "a", 1_000), None);
        assert_eq!(recall(task, 3, "a", 1_000 + REASSESS_MS), None);
        assert_eq!(recall(task, 3, "a", 999), None);
        forget(task);
        assert_eq!(recall(task, 3, "a", 1_000), None);
    }
}
