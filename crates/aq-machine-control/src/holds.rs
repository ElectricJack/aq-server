//! Owner-visible task holds (R6; lessons W83, W87: a hold is never a silent skip). A
//! server-side decision that keeps a ready task from being offered (an exhausted retry, time or
//! cost budget, a lost attempt whose retry was not admitted, a successor refused poll after poll)
//! is recorded on the task once per attempt history, named in exactly one system comment (a
//! fixed template, no worker text) and logged at info once. The task's work view and the swarm
//! read show the recorded hold; nothing re-derives it.
use crate::Error;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// First `state_format_version` with `task_holds` and `successor_skips`.
pub const HOLDS_FORMAT: u32 = 106;
/// Consecutive polls that skip one successor before it is held as stuck. A node polls every few
/// seconds, so 20 polls is a minute or two: longer than a brief aq-git or server restart (a
/// handful of refused polls), short enough that the owner sees a stuck successor the same
/// session. The successor keeps being tried; the hold only tells the owner.
pub const STUCK_SKIPS: i32 = 20;
/// A hold that no poll has seen for this long (its task left the ready page: retired, completed
/// or blocked) no longer counts in the swarm read.
pub const FRESH_MINUTES: i64 = 15;
/// `seen_at` is refreshed at most this often, so a held task costs one read per poll.
const SEEN_EVERY_SECONDS: i64 = 60;

/// A server reason code: lower snake case, never worker text.
pub fn valid_code(code: &str) -> bool {
    let mut c = code.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && code.len() <= 96
        && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// The fixed phrase for a hold reason code; an unknown code gets the generic fallback.
pub fn phrase(code: &str) -> String {
    let fixed = match code {
        "lineage_retry_budget_exhausted" => "its retry budget is exhausted",
        "lineage_time_budget_exhausted" => "its lineage time budget is exhausted",
        "lineage_time_budget_exhausted_or_unknown" => {
            "its lineage time budget is exhausted or unknown"
        }
        "lineage_cost_budget_exhausted" => "its cost budget is exhausted",
        "task_sandbox_minutes_budget_exhausted" => "its sandbox minutes budget is exhausted",
        "replacement_policy_retry_budget_exhausted" => {
            "the catalog retry policy's maximum attempts are used up"
        }
        "replacement_policy_time_budget_exhausted" => {
            "the catalog retry policy's lineage time limit is reached"
        }
        "never_granted_retry_bound_exhausted" => "too many of its offers were never granted",
        "replacement_project_revision_changed" => {
            "the project binding changed since its last attempt, whose input a retry would reuse"
        }
        "replacement_after_unpublished_delivery_needs_new_source" => {
            "its delivery was not published, so a retry needs a new source"
        }
        "replacement_lineage_history_unknown" => "its attempt history cannot be read",
        "replacement_prior_execution_uncertain" => "its last attempt may still be running",
        "replacement_effects_unreconciled" => "its last attempt's effects are not reconciled",
        "replacement_source_retention_unknown" => {
            "its last attempt's input is not known to be retained"
        }
        "replacement_predecessor_not_failed" => "its last attempt has not failed",
        "task_already_settled_accepted" => "its result is already accepted",
        "task_terminally_disposed" => "an owner terminally disposed of it",
        "stall_quarantined" => "the stall ladder quarantined it",
        "lost_attempt_release_not_admitted" => "the retry of its lost attempt was not admitted",
        "handoff_successor_stuck" => {
            "its handoff successor was refused on 20 polls in a row (AQ keeps trying it)"
        }
        "replacement_successor_stuck" => {
            "its replacement was refused on 20 polls in a row (AQ keeps trying it)"
        }
        _ => return format!("held by the retry policy: {code}"),
    };
    fixed.into()
}

/// Whether an offer-path refusal is a hold (it stays until an owner acts or the task changes),
/// not a wait that clears by itself (a prior attempt still settling, say).
pub fn is_hold(code: &str) -> bool {
    code.ends_with("_budget_exhausted")
        || matches!(
            code,
            "lineage_time_budget_exhausted_or_unknown"
                | "never_granted_retry_bound_exhausted"
                | "replacement_project_revision_changed"
                | "replacement_after_unpublished_delivery_needs_new_source"
                | "replacement_lineage_history_unknown"
                | "handoff_successor_stuck"
                | "replacement_successor_stuck"
        )
}
fn stuck(code: &str) -> bool {
    code.ends_with("_successor_stuck")
}

/// The system comment of an offer-path hold.
pub fn comment_body(code: &str) -> String {
    let next = match stuck(code) {
        true => "AQ keeps offering it; an owner can retire the task.",
        false => "It is not offered again until an owner acts; an owner can retire the task.",
    };
    format!(
        "AQ: this task is held: {} (hold reason: {code}). {next}",
        phrase(code)
    )
}

async fn has_holds(conn: &mut PgConnection) -> Result<bool, Error> {
    let v: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(&mut *conn)
        .await?;
    Ok(v.parse::<u32>().map_err(|_| Error::Invalid("format"))? >= HOLDS_FORMAT)
}

/// A hold that still needs its comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Noted {
    pub comment_id: Uuid,
    pub attempt_id: Uuid,
    /// First recorded by this call (log it once).
    pub new: bool,
}

/// Records `reason` as the hold of `task`'s current attempt history, once: a later decision for
/// the same history (a replay, the next poll) records nothing, whatever its reason. `attempt` is
/// the attempt the hold is about (else the holding or latest attempt); `comment` overrides the
/// comment id (`v5(task, aq.task-hold/<attempts>)`). None when the hold and its comment are
/// already on record, or the store predates the table.
pub async fn note(
    conn: &mut PgConnection,
    project: Uuid,
    task: Uuid,
    reason: &str,
    attempt: Option<Uuid>,
    comment: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<Option<Noted>, Error> {
    if !valid_code(reason) {
        return Err(Error::Invalid("hold reason"));
    }
    if !has_holds(conn).await? {
        return Ok(None);
    }
    let row = sqlx::query(
        "WITH n AS (SELECT count(*)::INT AS attempts FROM execution_attempts WHERE task_id=$1) \
         SELECT n.attempts,h.commented,h.comment_id,h.attempt_id,h.seen_at<$2 AS stale FROM n \
         LEFT JOIN task_holds h ON h.task_id=$1 AND h.attempts=n.attempts",
    )
    .bind(task)
    .bind(now - chrono::Duration::seconds(SEEN_EVERY_SECONDS))
    .fetch_one(&mut *conn)
    .await?;
    let attempts: i32 = row.get("attempts");
    if let Some(commented) = row.get::<Option<bool>, _>("commented") {
        if row.get::<Option<bool>, _>("stale") == Some(true) {
            sqlx::query("UPDATE task_holds SET seen_at=$3 WHERE task_id=$1 AND attempts=$2")
                .bind(task)
                .bind(attempts)
                .bind(now)
                .execute(&mut *conn)
                .await?;
        }
        return Ok((!commented).then(|| Noted {
            comment_id: row.get("comment_id"),
            attempt_id: row.get("attempt_id"),
            new: false,
        }));
    }
    let attempt = match attempt {
        Some(a) => Some(a),
        None => sqlx::query_scalar(
            "SELECT a.attempt_id FROM execution_attempts a LEFT JOIN profiled_launches l USING(attempt_id) \
             WHERE a.task_id=$1 ORDER BY a.holds_task DESC, \
             (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT DESC NULLS LAST, \
             a.attempt_id DESC LIMIT 1",
        )
        .bind(task)
        .fetch_optional(&mut *conn)
        .await?,
    };
    let Some(attempt) = attempt.filter(|_| attempts > 0) else {
        return Err(Error::Invalid("a hold needs an attempt of its task"));
    };
    let comment = comment
        .unwrap_or_else(|| Uuid::new_v5(&task, format!("aq.task-hold/{attempts}").as_bytes()));
    let inserted = sqlx::query(
        "INSERT INTO task_holds(task_id,attempts,project_id,reason,attempt_id,comment_id,held_at,seen_at) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$7) ON CONFLICT DO NOTHING",
    )
    .bind(task)
    .bind(attempts)
    .bind(project)
    .bind(reason)
    .bind(attempt)
    .bind(comment)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    Ok(inserted.then_some(Noted {
        comment_id: comment,
        attempt_id: attempt,
        new: true,
    }))
}

/// Posts a hold's system comment (idempotent by its id) and marks it on record. A failure is
/// logged; the next decision for the hold posts it again.
pub async fn post(
    pool: &PgPool,
    noted: &Noted,
    thread: aq_task_comments::Thread,
    body: &str,
    now: DateTime<Utc>,
) {
    let author = aq_task_comments::Author::System {
        component: "aq.swarm".into(),
    };
    let task = thread.task_id;
    let posted = aq_task_comments::add(pool, noted.comment_id, thread, &author, body, now).await;
    if let Err(e) = posted {
        tracing::warn!(%task, error = ?e, "task hold comment not posted");
        return;
    }
    let marked = sqlx::query("UPDATE task_holds SET commented=TRUE WHERE comment_id=$1")
        .bind(noted.comment_id)
        .execute(pool)
        .await;
    if let Err(e) = marked {
        tracing::warn!(%task, error = ?e, "task hold comment not marked");
    }
}

/// The offer path's hold of a ready task it does not offer: `reason` is recorded once, logged
/// at info once, and named in one system comment. Never fails the poll; a refusal that is not a
/// hold ([`is_hold`]) records nothing.
pub async fn hold(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
    reason: &str,
    attempt: Option<Uuid>,
    now: DateTime<Utc>,
) {
    if !is_hold(reason) {
        return;
    }
    let noted = async {
        let mut conn = pool.acquire().await?;
        note(&mut conn, project, task, reason, attempt, None, now).await
    };
    let noted = match noted.await {
        Ok(Some(n)) => n,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(%task, reason, error = ?e, "task hold not recorded");
            return;
        }
    };
    if noted.new {
        tracing::info!(%task, %project, reason, attempt = %noted.attempt_id,
            "task held; it is not offered and the owner is told in a system comment");
    }
    let thread = aq_task_comments::Thread {
        project_id: project,
        task_id: task,
    };
    post(pool, &noted, thread, &comment_body(reason), now).await;
}

/// A task's current hold in `project` (its attempt history has not moved on since it was
/// recorded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub reason: String,
    pub attempt_id: Uuid,
    /// The attempt's authority status.
    pub status: String,
    pub held_at: DateTime<Utc>,
}
pub async fn current(pool: &PgPool, project: Uuid, task: Uuid) -> Result<Option<Current>, Error> {
    let mut conn = pool.acquire().await?;
    if !has_holds(&mut conn).await? {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT h.reason,h.attempt_id,h.held_at,COALESCE(p.state->>'status',e.state->>'status','unknown') AS status \
         FROM task_holds h JOIN execution_attempts e ON e.attempt_id=h.attempt_id \
         LEFT JOIN profiled_authority_hot p ON p.attempt_id=h.attempt_id \
         WHERE h.task_id=$1 AND h.project_id=$2 \
         AND h.attempts=(SELECT count(*) FROM execution_attempts WHERE task_id=$1)",
    )
    .bind(task)
    .bind(project)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| Current {
        reason: r.get("reason"),
        attempt_id: r.get("attempt_id"),
        status: r.get("status"),
        held_at: r.get("held_at"),
    }))
}

/// Current holds a poll saw in the last [`FRESH_MINUTES`], counted per (project, reason).
pub async fn counts(
    conn: &mut PgConnection,
    projects: &[Uuid],
    now: DateTime<Utc>,
) -> Result<Vec<(Uuid, String, i64)>, Error> {
    if projects.is_empty() || !has_holds(conn).await? {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as(
        "SELECT h.project_id,h.reason,count(*) FROM task_holds h \
         WHERE h.project_id=ANY($1) AND h.seen_at>=$2 \
         AND h.attempts=(SELECT count(*) FROM execution_attempts a WHERE a.task_id=h.task_id) \
         GROUP BY 1,2 ORDER BY 1,2 LIMIT 256",
    )
    .bind(projects)
    .bind(now - chrono::Duration::minutes(FRESH_MINUTES))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// Counts one more consecutive skip of `task`'s successor of `predecessor`. From
/// [`STUCK_SKIPS`] on it also names the stuck hold: `handoff_successor_stuck` when the
/// predecessor was handed off, else `replacement_successor_stuck`.
pub async fn skip(
    conn: &mut PgConnection,
    task: Uuid,
    predecessor: Uuid,
    now: DateTime<Utc>,
) -> Result<(i32, Option<&'static str>), Error> {
    if !has_holds(conn).await? {
        return Ok((1, None));
    }
    let skips: i32 = sqlx::query_scalar(
        "INSERT INTO successor_skips(task_id,predecessor,skips,last_at) VALUES($1,$2,1,$3) \
         ON CONFLICT(task_id,predecessor) DO UPDATE SET skips=successor_skips.skips+1,last_at=$3 \
         RETURNING skips",
    )
    .bind(task)
    .bind(predecessor)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    if skips < STUCK_SKIPS {
        return Ok((skips, None));
    }
    let handoff = crate::handoff_of(conn, predecessor).await?.is_some();
    Ok((
        skips,
        Some(match handoff {
            true => "handoff_successor_stuck",
            false => "replacement_successor_stuck",
        }),
    ))
}
/// A successor of `task` passed every skip: the count starts again. A failure is logged.
pub async fn cleared(pool: &PgPool, task: Uuid) {
    let cleared = async {
        let mut conn = pool.acquire().await?;
        if has_holds(&mut conn).await? {
            sqlx::query("DELETE FROM successor_skips WHERE task_id=$1")
                .bind(task)
                .execute(&mut *conn)
                .await?;
        }
        Ok::<_, Error>(())
    };
    if let Err(e) = cleared.await {
        tracing::warn!(%task, error = ?e, "successor skips not cleared");
    }
}
/// Selection refused `task`'s successor of `predecessor` (spec 0007 Q1 backstop): a budget
/// code among `reasons` holds the task ([`hold`]); any other refusal is a counted skip
/// ([`skipped`]).
pub async fn selection_refused(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
    predecessor: Uuid,
    reasons: &[String],
    now: DateTime<Utc>,
) {
    match reasons.iter().find(|c| is_hold(c)) {
        Some(code) => hold(pool, project, task, code, Some(predecessor), now).await,
        None => {
            let why = format!("selection refused: {reasons:?}");
            skipped(pool, project, task, predecessor, &why, now).await
        }
    }
}
/// The offer path skipped `task`'s successor of `predecessor` this poll (`why`: a refused
/// export or selection; server codes and errors only). The skip is counted and logged at warn
/// on the first skip and every hundredth (debug otherwise), and from [`STUCK_SKIPS`] in a row
/// the task is held as stuck ([`hold`]). Never fails the poll.
pub async fn skipped(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
    predecessor: Uuid,
    why: &str,
    now: DateTime<Utc>,
) {
    let counted = async {
        let mut conn = pool.acquire().await?;
        skip(&mut conn, task, predecessor, now).await
    };
    let (skips, stuck) = match counted.await {
        Ok(counted) => counted,
        Err(e) => {
            tracing::warn!(%task, %predecessor, why, error = ?e, "successor skip not counted; skipped this poll");
            return;
        }
    };
    match skip_warned(skips) {
        true => {
            tracing::warn!(%task, %predecessor, skips, why, "replacement successor refused; skipped this poll")
        }
        false => {
            tracing::debug!(%task, %predecessor, skips, why, "replacement successor refused; skipped this poll")
        }
    }
    if let Some(code) = stuck {
        hold(pool, project, task, code, Some(predecessor), now).await;
    }
}
/// The per-poll skip line is a warn on the first skip and every hundredth, else debug.
pub fn skip_warned(skips: i32) -> bool {
    skips == 1 || skips % 100 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_known_code_has_its_own_phrase_and_an_unknown_one_the_fallback() {
        let known = [
            "lineage_retry_budget_exhausted",
            "lineage_time_budget_exhausted",
            "lineage_time_budget_exhausted_or_unknown",
            "lineage_cost_budget_exhausted",
            "task_sandbox_minutes_budget_exhausted",
            "replacement_policy_retry_budget_exhausted",
            "replacement_policy_time_budget_exhausted",
            "never_granted_retry_bound_exhausted",
            "replacement_project_revision_changed",
            "replacement_after_unpublished_delivery_needs_new_source",
            "replacement_lineage_history_unknown",
            "replacement_prior_execution_uncertain",
            "replacement_effects_unreconciled",
            "replacement_source_retention_unknown",
            "replacement_predecessor_not_failed",
            "task_already_settled_accepted",
            "task_terminally_disposed",
            "stall_quarantined",
            "lost_attempt_release_not_admitted",
            "handoff_successor_stuck",
            "replacement_successor_stuck",
        ];
        let phrases: std::collections::BTreeSet<String> = known.iter().map(|c| phrase(c)).collect();
        assert_eq!(phrases.len(), known.len(), "one distinct phrase per code");
        for code in known {
            let p = phrase(code);
            assert!(!p.starts_with("held by the retry policy"), "{code}: {p}");
            assert!(!p.contains("after lost attempts"), "{code}: {p}");
        }
        assert_eq!(
            phrase("task_terminally_disposed"),
            "an owner terminally disposed of it"
        );
        assert_eq!(
            phrase("stall_quarantined"),
            "the stall ladder quarantined it"
        );
        assert_eq!(
            phrase("lost_attempt_release_not_admitted"),
            "the retry of its lost attempt was not admitted"
        );
        assert_eq!(
            phrase("some_new_code"),
            "held by the retry policy: some_new_code"
        );
    }
    #[test]
    fn holds_are_budget_and_stuck_codes_and_waits_are_not() {
        for code in [
            "lineage_retry_budget_exhausted",
            "replacement_policy_retry_budget_exhausted",
            "replacement_policy_time_budget_exhausted",
            "lineage_cost_budget_exhausted",
            "handoff_successor_stuck",
        ] {
            assert!(is_hold(code), "{code}");
        }
        for code in [
            "replacement_prior_execution_uncertain",
            "replacement_effects_unreconciled",
            "task_already_settled_accepted",
            "stall_quarantined",
        ] {
            assert!(!is_hold(code), "{code}");
        }
    }
    #[test]
    fn comments_use_the_phrase_and_never_free_text() {
        let body = comment_body("replacement_policy_retry_budget_exhausted");
        assert!(body.contains("the catalog retry policy's maximum attempts are used up"));
        assert!(body.contains("not offered again"));
        assert!(comment_body("handoff_successor_stuck").contains("AQ keeps offering it"));
        assert!(valid_code("handoff_successor_stuck"));
        for bad in ["", "Bad", "has space", "drop;table", "_x"] {
            assert!(!valid_code(bad), "{bad}");
        }
    }
    #[test]
    fn the_lost_attempt_comment_names_its_own_reason() {
        use aq_protocol::LostReason::{LostAfterCrash, LostAfterReboot};
        let a = Uuid::from_u128(7);
        let body = |r, code| crate::lost::hold_comment_body(a, r, code);
        let disposed = body(LostAfterCrash, "task_terminally_disposed");
        assert!(
            disposed.contains(
                "lost after a crash, and an owner terminally disposed of it (hold reason: task_terminally_disposed)"
            ),
            "{disposed}"
        );
        assert!(!disposed.contains("retry budget"), "{disposed}");
        let quarantined = body(LostAfterCrash, "stall_quarantined");
        assert!(quarantined.contains("and the stall ladder quarantined it"));
        let budget = body(LostAfterReboot, "lineage_retry_budget_exhausted");
        assert!(budget.contains("lost after a reboot, and its retry budget is exhausted"));
        let unknown = body(LostAfterCrash, "x_y");
        assert!(unknown.contains("and held by the retry policy: x_y (hold reason: x_y)"));
    }
    #[test]
    fn the_skip_warn_is_rate_limited() {
        let warned: Vec<i32> = (1..=300).filter(|n| skip_warned(*n)).collect();
        assert_eq!(warned, [1, 100, 200, 300]);
    }
}
