//! An owner's fresh start of a task the offer path holds because the project binding changed
//! since its last attempt (`replacement_project_revision_changed`). A retry reuses the replaced
//! attempt's retained source, which was exported under the old binding revision, so the hold
//! has no release of its own. A fresh start releases the attempt as a replacement does and
//! records that the task's attempts so far are no longer its lineage's history: the next
//! attempt is a first attempt from the project's current pinned source under the current
//! binding. Nothing is deleted; the earlier attempts and their retained results stay.
use crate::{Error, holds};
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// First `state_format_version` with `task_fresh_starts`.
pub const FORMAT: u32 = 156;
/// The only hold a fresh start releases.
pub const HOLD: &str = "replacement_project_revision_changed";

/// An owner's fresh start, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshStart {
    pub operation_id: Uuid,
    pub reason: String,
    pub actor: String,
    pub recorded_at_ms: i64,
    /// The project binding revision the fresh attempt starts under.
    pub project_revision: u64,
}

/// Why a retry is refused for the owner's choice of `fresh` and the task's hold `held` (the
/// code the offer path records, if any): the code and the message of the refusal. A plain
/// retry is refused for a hold that is not a budget ([`holds::retry_refusal`]); a fresh retry
/// only for a task that is not held by [`HOLD`], and says so.
pub fn refusal(fresh: bool, held: Option<&'static str>) -> Option<(&'static str, String)> {
    match (fresh, held) {
        (false, Some(code)) if !holds::is_budget_hold(code) => {
            Some((code, holds::retry_refusal(code)))
        }
        (true, held) if held != Some(HOLD) => {
            let why = held.map_or_else(|| "it is not held".into(), holds::phrase);
            let message = format!(
                "Fresh retry refused: it applies only to a task held because the project binding changed, and {why}."
            );
            Some((held.unwrap_or("fresh_retry_not_applicable"), message))
        }
        _ => None,
    }
}

/// Whether the store has `task_fresh_starts`.
pub async fn has_fresh_starts(conn: &mut PgConnection) -> Result<bool, Error> {
    let v: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(&mut *conn)
        .await?;
    Ok(v.parse::<u32>().map_err(|_| Error::Invalid("format"))? >= FORMAT)
}

/// Records the fresh start once per `operation_id` (a replay records nothing): every attempt
/// the task has now stops counting as its history, and its current hold is cleared. The
/// caller released `released` (the held attempt) in the same transaction.
pub async fn record(
    conn: &mut PgConnection,
    task: Uuid,
    released: Uuid,
    f: &FreshStart,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    let reason_ok = !f.reason.is_empty() && f.reason.chars().count() <= holds::REASON_MAX;
    if !reason_ok || f.actor.is_empty() || f.recorded_at_ms <= 0 {
        return Err(Error::Invalid("fresh start"));
    }
    if !has_fresh_starts(conn).await? {
        return Err(Error::Invalid("fresh start before its store format"));
    }
    let revision = i64::try_from(f.project_revision).map_err(|_| Error::Invalid("revision"))?;
    sqlx::query(
        "INSERT INTO task_fresh_starts(attempt_id,task_id,operation_id,released_attempt_id,hold_code,reason,actor,recorded_at_ms,project_revision) \
         SELECT a.attempt_id,a.task_id,$2,$3,$4,$5,$6,$7,$8 FROM execution_attempts a WHERE a.task_id=$1 \
         ON CONFLICT DO NOTHING",
    )
    .bind(task)
    .bind(f.operation_id)
    .bind(released)
    .bind(HOLD)
    .bind(&f.reason)
    .bind(&f.actor)
    .bind(f.recorded_at_ms)
    .bind(revision)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE task_holds SET cleared_at=$2,cleared_by='owner_override' WHERE task_id=$1 AND cleared_at IS NULL",
    )
    .bind(task)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The system comment of a fresh start: the task's history shows who started it fresh and why.
pub fn comment(actor: &str, reason: &str) -> String {
    format!(
        "AQ: an owner ({actor}) started this task fresh: it was held because {} (hold reason: {HOLD}). Its next attempt is a first attempt from the project's current source under the current binding; its earlier attempts and their retained results stay on record and no longer count against its budgets. Owner's reason: {reason}",
        holds::phrase(HOLD)
    )
}
/// Names a recorded fresh start in the task's history: one system comment whose id is derived
/// from the operation's, so a replay posts nothing new. The record stands if the post fails
/// (logged).
pub async fn post(pool: &PgPool, project: Uuid, task: Uuid, f: &FreshStart, now: DateTime<Utc>) {
    let thread = aq_task_comments::Thread {
        project_id: project,
        task_id: task,
    };
    let author = aq_task_comments::Author::System {
        component: "aq.swarm".into(),
    };
    let id = Uuid::new_v5(
        &task,
        format!("aq.fresh-start/{}", f.operation_id).as_bytes(),
    );
    match aq_task_comments::add(
        pool,
        id,
        thread,
        &author,
        &comment(&f.actor, &f.reason),
        now,
    )
    .await
    {
        Ok(_) | Err(aq_task_comments::Error::IdConflict) => {}
        Err(e) => tracing::warn!(%task, error = ?e, "fresh start comment not posted"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_fresh_retry_is_only_for_the_binding_change_hold() {
        // A plain retry: unchanged (a budget is overridden, another hold refused).
        assert_eq!(refusal(false, None), None);
        assert_eq!(refusal(false, Some("lineage_retry_budget_exhausted")), None);
        let (code, text) = refusal(false, Some(HOLD)).unwrap();
        assert_eq!(code, HOLD);
        assert!(text.contains("fresh=true"), "{text}");
        // A fresh retry is for that hold alone.
        assert_eq!(refusal(true, Some(HOLD)), None);
        let (code, text) = refusal(true, Some("stall_quarantined")).unwrap();
        assert_eq!(code, "stall_quarantined");
        assert!(text.contains("the stall ladder quarantined it"), "{text}");
        let (code, text) = refusal(true, None).unwrap();
        assert_eq!(code, "fresh_retry_not_applicable");
        assert!(text.contains("it is not held"), "{text}");
        let budget = refusal(true, Some("lineage_retry_budget_exhausted")).unwrap();
        assert_eq!(budget.0, "lineage_retry_budget_exhausted");
        for case in [Some(HOLD), Some("stall_quarantined"), None] {
            if let Some((_, text)) = refusal(true, case).or(refusal(false, case)) {
                assert!(text.chars().count() <= 256, "{text}");
            }
        }
    }
    #[test]
    fn the_comment_names_the_actor_the_hold_and_the_reason() {
        let text = comment("owner@localhost", "ceilings raised");
        assert!(text.contains("owner@localhost"), "{text}");
        assert!(text.contains(HOLD), "{text}");
        assert!(text.ends_with("Owner's reason: ceilings raised"), "{text}");
    }
}
