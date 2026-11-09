//! Owner-visible task holds (R6; lessons W83, W87: a hold is never a silent skip). A
//! server-side decision that keeps a ready task from being offered (an exhausted retry, time or
//! cost budget, a quarantine, a lost attempt whose retry was not admitted, a successor refused
//! poll after poll) is recorded on the task once per attempt history, named in exactly one
//! system comment (a fixed template, no worker text) and logged at info once.
//!
//! "Held" has one definition, the `task_holds_current` view: the task view ([`current`]) and the
//! swarm read ([`counts`]) both read it. A hold is cleared when the task gets a new attempt or
//! its claim is released (migration 0106 triggers, whatever the path), and when AQ observes the
//! task closed or retired in Graph ([`clear_closed`] from the task view, [`sweep`]).
use crate::Error;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// First `state_format_version` with `task_holds` and `successor_skips`.
pub const HOLDS_FORMAT: u32 = 106;
/// A successor is held as stuck once it has been skipped on every poll for [`STUCK_MINUTES`]
/// and at least this often. The time bound keeps a fleet of N polling nodes from tripping it N
/// times sooner; ten minutes is far past a brief aq-git or server restart, and short enough
/// that the owner sees a stuck successor the same session. The successor keeps being tried.
pub const STUCK_SKIPS: i32 = 20;
pub const STUCK_MINUTES: i64 = 10;
/// A gap this long between skips (no poll considered the successor) starts the count again.
pub const SKIP_GAP_MINUTES: i64 = 5;
/// Holds the sweep checks against Graph per run.
pub const SWEEP: i64 = 64;

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
            "its handoff successor was refused on every poll for 10 minutes (AQ keeps trying it)"
        }
        "replacement_successor_stuck" => {
            "its replacement was refused on every poll for 10 minutes (AQ keeps trying it)"
        }
        _ => return format!("held by the retry policy: {code}"),
    };
    fixed.into()
}

/// Whether an offer-path refusal is a hold: it stays until an owner acts or the task changes
/// (a quarantine has no release path; H3 holds it until an owner acts). A wait that clears by
/// itself (a prior attempt still settling, a history read that failed) is not.
pub fn is_hold(code: &str) -> bool {
    code.ends_with("_budget_exhausted")
        || matches!(
            code,
            "lineage_time_budget_exhausted_or_unknown"
                | "never_granted_retry_bound_exhausted"
                | "replacement_project_revision_changed"
                | "replacement_after_unpublished_delivery_needs_new_source"
                | "stall_quarantined"
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
const COUNT: &str = "SELECT count(*)::INT FROM execution_attempts WHERE task_id=$1";
pub async fn attempts_in(conn: &mut PgConnection, task: Uuid) -> Result<i32, Error> {
    Ok(sqlx::query_scalar(COUNT)
        .bind(task)
        .fetch_one(&mut *conn)
        .await?)
}
/// The task's attempt count, read before the decision a hold records (None, logged, on error).
pub async fn attempts(pool: &PgPool, task: Uuid) -> Option<i32> {
    let n = sqlx::query_scalar(COUNT).bind(task).fetch_one(pool).await;
    n.inspect_err(|e| tracing::warn!(%task, error = ?e, "task attempts not counted"))
        .ok()
}

/// The decision to hold `task`.
#[derive(Debug, Clone, Copy)]
pub struct Decision<'a> {
    pub project: Uuid,
    pub task: Uuid,
    pub reason: &'a str,
    /// The task's attempt count read before the decision ([`attempts`]); the hold is recorded
    /// only while it is unchanged, so a refusal that a new attempt already answered is not.
    pub seen: i32,
    /// The attempt the hold is about; else the holding or latest attempt.
    pub attempt: Option<Uuid>,
}

/// A recorded hold whose comment is not on record yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Noted {
    pub comment_id: Uuid,
    pub attempt_id: Uuid,
    /// The stored reason and comment (built from it when the hold was recorded).
    pub reason: String,
    pub body: String,
    /// First recorded by this call (log it once).
    pub new: bool,
}

/// Records `d` as the task's hold, once: while a hold is current, a later decision (a replay,
/// the next poll, a racing node) records nothing, whatever its reason. `comment` overrides the
/// comment (id and body); by default the id is `v5(task, aq.task-hold/<seen>/<earlier holds>)`
/// and the body [`comment_body`]. None when the hold and its comment are on record, the count
/// moved, or the store predates the table.
pub async fn note(
    conn: &mut PgConnection,
    d: &Decision<'_>,
    comment: Option<(Uuid, String)>,
    now: DateTime<Utc>,
) -> Result<Option<Noted>, Error> {
    if !valid_code(d.reason) {
        return Err(Error::Invalid("hold reason"));
    }
    if !has_holds(conn).await? {
        return Ok(None);
    }
    let noted = |r: &sqlx::postgres::PgRow, new| Noted {
        comment_id: r.get("comment_id"),
        attempt_id: r.get("attempt_id"),
        reason: r.get("reason"),
        body: r.get("comment_body"),
        new,
    };
    let current = sqlx::query(
        "SELECT comment_id,attempt_id,reason,comment_body,commented FROM task_holds_current WHERE task_id=$1",
    )
    .bind(d.task)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(r) = current {
        return Ok((!r.get::<bool, _>("commented")).then(|| noted(&r, false)));
    }
    // A hold left behind by an attempt that raced its trigger no longer applies.
    sqlx::query(
        "UPDATE task_holds SET cleared_at=$2,cleared_by='new_attempt' WHERE task_id=$1 \
         AND cleared_at IS NULL AND attempts<>(SELECT count(*) FROM execution_attempts WHERE task_id=$1)",
    )
    .bind(d.task)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let attempt = match d.attempt {
        Some(a) => Some(a),
        None => sqlx::query_scalar(
            "SELECT a.attempt_id FROM execution_attempts a LEFT JOIN profiled_launches l USING(attempt_id) \
             WHERE a.task_id=$1 ORDER BY a.holds_task DESC, \
             (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT DESC NULLS LAST, \
             a.attempt_id DESC LIMIT 1",
        )
        .bind(d.task)
        .fetch_optional(&mut *conn)
        .await?,
    };
    let Some(attempt) = attempt.filter(|_| d.seen > 0) else {
        return Err(Error::Invalid("a hold needs an attempt of its task"));
    };
    let (comment, body) = match comment {
        Some(c) => c,
        None => {
            let earlier: i64 =
                sqlx::query_scalar("SELECT count(*) FROM task_holds WHERE task_id=$1")
                    .bind(d.task)
                    .fetch_one(&mut *conn)
                    .await?;
            let name = format!("aq.task-hold/{}/{earlier}", d.seen);
            (
                Uuid::new_v5(&d.task, name.as_bytes()),
                comment_body(d.reason),
            )
        }
    };
    // Only while the count the decision saw is current, in this one statement.
    let inserted = sqlx::query(
        "INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) \
         SELECT $1,$2,$3,$4,$5,$6,$7,$8 WHERE (SELECT count(*) FROM execution_attempts WHERE task_id=$2)=$3 \
         ON CONFLICT DO NOTHING RETURNING comment_id,attempt_id,reason,comment_body",
    )
    .bind(comment)
    .bind(d.task)
    .bind(d.seen)
    .bind(d.project)
    .bind(d.reason)
    .bind(attempt)
    .bind(body)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(inserted.map(|r| noted(&r, true)))
}

/// Posts a hold's stored comment (idempotent by its id) and marks it on record. A comment
/// already under that id (a replay, or a post whose mark was lost) counts as posted. Any other
/// failure is logged; the next decision for the hold posts it again.
pub async fn post(
    pool: &PgPool,
    noted: &Noted,
    thread: aq_task_comments::Thread,
    now: DateTime<Utc>,
) {
    let author = aq_task_comments::Author::System {
        component: "aq.swarm".into(),
    };
    let task = thread.task_id;
    let posted = aq_task_comments::add(pool, noted.comment_id, thread, &author, &noted.body, now);
    match posted.await {
        Ok(_) | Err(aq_task_comments::Error::IdConflict) => {}
        Err(e) => {
            tracing::warn!(%task, error = ?e, "task hold comment not posted");
            return;
        }
    }
    let marked = sqlx::query("UPDATE task_holds SET commented=TRUE WHERE comment_id=$1")
        .bind(noted.comment_id)
        .execute(pool)
        .await;
    if let Err(e) = marked {
        tracing::warn!(%task, error = ?e, "task hold comment not marked");
    }
}

/// The offer path's hold of a ready task it does not offer: recorded once, logged at info once
/// and named in one system comment. Never fails the poll; a refusal that is not a hold
/// ([`is_hold`]), or a decision without its attempt count, records nothing.
pub async fn hold(pool: &PgPool, d: Option<Decision<'_>>, now: DateTime<Utc>) {
    let Some(d) = d.filter(|d| is_hold(d.reason)) else {
        return;
    };
    let noted = async {
        let mut conn = pool.acquire().await?;
        note(&mut conn, &d, None, now).await
    };
    let noted = match noted.await {
        Ok(Some(n)) => n,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(task = %d.task, reason = d.reason, error = ?e, "task hold not recorded");
            return;
        }
    };
    if noted.new {
        tracing::info!(task = %d.task, project = %d.project, reason = d.reason, attempt = %noted.attempt_id,
            "task held; it is not offered and the owner is told in a system comment");
    }
    let thread = aq_task_comments::Thread {
        project_id: d.project,
        task_id: d.task,
    };
    post(pool, &noted, thread, now).await;
}

/// A task's current hold in `project`.
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
         FROM task_holds_current h JOIN execution_attempts e ON e.attempt_id=h.attempt_id \
         LEFT JOIN profiled_authority_hot p ON p.attempt_id=h.attempt_id \
         WHERE h.task_id=$1 AND h.project_id=$2",
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

/// Current holds counted per (project, reason): the same rows as [`current`].
pub async fn counts(
    conn: &mut PgConnection,
    projects: &[Uuid],
) -> Result<Vec<(Uuid, String, i64)>, Error> {
    if projects.is_empty() || !has_holds(conn).await? {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as(
        "SELECT project_id,reason,count(*) FROM task_holds_current WHERE project_id=ANY($1) \
         GROUP BY 1,2 ORDER BY 1,2 LIMIT 256",
    )
    .bind(projects)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// AQ observed `task` closed, retired or deleted in Graph: its hold is cleared. Logged on error.
pub async fn clear_closed(pool: &PgPool, task: Uuid, now: DateTime<Utc>) {
    let cleared = sqlx::query(
        "UPDATE task_holds SET cleared_at=$2,cleared_by='task_closed' WHERE task_id=$1 AND cleared_at IS NULL",
    )
    .bind(task)
    .bind(now)
    .execute(pool)
    .await;
    if let Err(e) = cleared {
        tracing::warn!(%task, error = ?e, "closed task's hold not cleared");
    }
}

/// Checks up to [`SWEEP`] current holds (least recently checked first) against Graph and clears
/// those whose task is closed, retired or deleted. A failed read keeps the hold. Returns the
/// number cleared.
pub async fn sweep(
    pool: &PgPool,
    tasks: &dyn aq_row_retention::TaskState,
    now: DateTime<Utc>,
) -> Result<u64, Error> {
    let mut conn = pool.acquire().await?;
    if !has_holds(&mut conn).await? {
        return Ok(0);
    }
    let held: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT h.task_id,e.state#>>'{offer,manifest,bindings,graph_id}' FROM task_holds_current h \
         JOIN execution_attempts e ON e.attempt_id=h.attempt_id ORDER BY h.checked_at NULLS FIRST,h.task_id LIMIT $1",
    )
    .bind(SWEEP)
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);
    let mut cleared = 0;
    for (task, workspace) in held {
        let Some(workspace) = workspace.and_then(|w| w.parse().ok()) else {
            continue;
        };
        match tasks.closed(workspace, task).await {
            Ok(true) => {
                clear_closed(pool, task, now).await;
                cleared += 1;
            }
            Ok(false) | Err(_) => {
                sqlx::query(
                    "UPDATE task_holds SET checked_at=$2 WHERE task_id=$1 AND cleared_at IS NULL",
                )
                .bind(task)
                .bind(now)
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(cleared)
}

/// Counts one more skip of `task`'s successor of `predecessor`. Once it has been skipped on every
/// poll for [`STUCK_MINUTES`] and at least [`STUCK_SKIPS`] times it also names the stuck hold:
/// `handoff_successor_stuck` when the predecessor was handed off, else
/// `replacement_successor_stuck`.
pub async fn skip(
    conn: &mut PgConnection,
    task: Uuid,
    predecessor: Uuid,
    now: DateTime<Utc>,
) -> Result<(i32, Option<&'static str>), Error> {
    if !has_holds(conn).await? {
        return Ok((1, None));
    }
    let (skips, first): (i32, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO successor_skips AS s(task_id,predecessor,skips,first_at,last_at) VALUES($1,$2,1,$3,$3) \
         ON CONFLICT(task_id,predecessor) DO UPDATE SET \
         skips=CASE WHEN s.last_at<$4 THEN 1 ELSE s.skips+1 END, \
         first_at=CASE WHEN s.last_at<$4 THEN $3 ELSE s.first_at END, \
         last_at=GREATEST(s.last_at,$3) RETURNING skips,first_at",
    )
    .bind(task)
    .bind(predecessor)
    .bind(now)
    .bind(now - chrono::Duration::minutes(SKIP_GAP_MINUTES))
    .fetch_one(&mut *conn)
    .await?;
    if skips < STUCK_SKIPS || now - first < chrono::Duration::minutes(STUCK_MINUTES) {
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
/// The task, and the attempt count read before the refusal, of a skipped successor.
#[derive(Debug, Clone, Copy)]
pub struct Successor {
    pub project: Uuid,
    pub task: Uuid,
    pub predecessor: Uuid,
    pub seen: Option<i32>,
}
impl Successor {
    fn decision(self, reason: &str) -> Option<Decision<'_>> {
        Some(Decision {
            project: self.project,
            task: self.task,
            reason,
            seen: self.seen?,
            attempt: Some(self.predecessor),
        })
    }
}
/// Selection refused the successor (spec 0007 Q1 backstop): a budget code among `reasons` holds
/// the task ([`hold`]); any other refusal is a counted skip ([`skipped`]).
pub async fn selection_refused(
    pool: &PgPool,
    s: Successor,
    reasons: &[String],
    now: DateTime<Utc>,
) {
    match reasons.iter().find(|c| is_hold(c)) {
        Some(code) => hold(pool, s.decision(code), now).await,
        None => skipped(pool, s, &format!("selection refused: {reasons:?}"), now).await,
    }
}
/// The offer path skipped the successor this poll (`why`: a refused export or selection; server
/// codes and errors only). The skip is counted and logged at warn on the first skip and every
/// hundredth (debug otherwise); a stuck successor ([`skip`]) holds the task. Never fails the
/// poll.
pub async fn skipped(pool: &PgPool, s: Successor, why: &str, now: DateTime<Utc>) {
    let (task, predecessor) = (s.task, s.predecessor);
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
        hold(pool, s.decision(code), now).await;
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
        assert!(
            is_hold("stall_quarantined"),
            "a quarantine lasts until an owner acts"
        );
        for code in [
            "replacement_prior_execution_uncertain",
            "replacement_effects_unreconciled",
            "replacement_lineage_history_unknown",
            "task_already_settled_accepted",
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
