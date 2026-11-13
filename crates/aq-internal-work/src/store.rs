//! The `internal_tasks` queue (spec 0012 §4). Runners claim with
//! `FOR UPDATE SKIP LOCKED`; every write after a claim is fenced on the row
//! still being `running` for the same `candidate_index`, so a superseded
//! runner cannot overwrite a newer attempt or a finished task.
use crate::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::{PgPool, postgres::PgRow};

/// The failure detail of a task whose deadline passed while it was queued.
const QUEUED_PAST_DEADLINE: &str = "deadline passed before a runner took the task";

/// Every column of `internal_tasks`, JSON columns decoded.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: Uuid,
    pub purpose: Purpose,
    pub workspace_kind: String,
    pub request: Request,
    pub candidates: Vec<Candidate>,
    pub candidate_index: u32,
    /// `queued`, `running`, `answered` or `failed`.
    pub status: String,
    pub blocking: bool,
    pub runner: Option<String>,
    pub claimed_at_ms: Option<i64>,
    pub deadline_at_ms: i64,
    pub created_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub attempts: Vec<Attempt>,
    pub answer: Option<Value>,
    pub usage: Option<Usage>,
    pub reason: Option<Reason>,
    pub detail: Option<String>,
    pub affected_kind: String,
    pub affected_id: String,
    pub project_id: Option<Uuid>,
    pub escalate: bool,
    /// `none`, `supervisor`, `owner` or `resolved`.
    pub escalation: String,
    pub supervisor_attempts: i32,
    pub diagnosis: Option<String>,
    pub retry_of: Option<Uuid>,
    /// When the run last moved to a supervisor or the owner.
    pub escalated_at_ms: Option<i64>,
}

impl Row {
    /// The answer or the failure, once the task has finished.
    pub fn outcome(&self) -> Option<Outcome> {
        match self.status.as_str() {
            "answered" => {
                let candidate = self.candidates.get(self.candidate_index as usize)?;
                Some(Outcome::Answered {
                    answer: self.answer.clone()?,
                    provider: candidate.provider.clone(),
                    model: candidate.model.clone(),
                    way: candidate.way,
                    elapsed_ms: elapsed(self.created_at_ms, self.finished_at_ms?),
                    usage: self.usage.clone(),
                    attempts: self.attempts.clone(),
                })
            }
            "failed" => Some(Outcome::Failed {
                reason: self.reason?,
                detail: self.detail.clone().unwrap_or_default(),
                attempts: self.attempts.clone(),
            }),
            _ => None,
        }
    }

    fn decode(row: &PgRow) -> Result<Row> {
        use sqlx::Row as _;
        let candidate_index: i32 = row.try_get("candidate_index")?;
        Ok(Row {
            id: row.try_get("id")?,
            purpose: from_text(&row.try_get::<String, _>("purpose")?)?,
            workspace_kind: row.try_get("workspace_kind")?,
            request: from_json(row.try_get("request")?)?,
            candidates: from_json(row.try_get("candidates")?)?,
            candidate_index: u32::try_from(candidate_index).map_err(|_| Error::Corrupt)?,
            status: row.try_get("status")?,
            blocking: row.try_get("blocking")?,
            runner: row.try_get("runner")?,
            claimed_at_ms: row.try_get("claimed_at_ms")?,
            deadline_at_ms: row.try_get("deadline_at_ms")?,
            created_at_ms: row.try_get("created_at_ms")?,
            finished_at_ms: row.try_get("finished_at_ms")?,
            attempts: from_json(row.try_get("attempts")?)?,
            answer: row.try_get("answer")?,
            usage: row
                .try_get::<Option<Value>, _>("usage")?
                .map(from_json)
                .transpose()?,
            reason: row
                .try_get::<Option<String>, _>("reason")?
                .as_deref()
                .map(from_text)
                .transpose()?,
            detail: row.try_get("detail")?,
            affected_kind: row.try_get("affected_kind")?,
            affected_id: row.try_get("affected_id")?,
            project_id: row.try_get("project_id")?,
            escalate: row.try_get("escalate")?,
            escalation: row.try_get("escalation")?,
            supervisor_attempts: row.try_get("supervisor_attempts")?,
            diagnosis: row.try_get("diagnosis")?,
            retry_of: row.try_get("retry_of")?,
            escalated_at_ms: row.try_get("escalated_at_ms")?,
        })
    }
}

/// The earlier run a new run retries, and the supervisor attempts it carries.
#[derive(Clone, Copy, Debug)]
pub struct Lineage {
    pub retry_of: Uuid,
    pub supervisor_attempts: i32,
}

/// Queues `request` for `candidates` in fallback order. With no candidate the
/// task is stored already `failed` with `no_candidate`.
pub async fn submit(
    pool: &PgPool,
    request: &Request,
    candidates: &[Candidate],
    now_ms: i64,
) -> Result<Uuid> {
    let failure = candidates.is_empty().then_some((
        Reason::NoCandidate,
        "no candidate is configured for this purpose",
    ));
    insert(
        pool,
        Uuid::new_v4(),
        request,
        candidates,
        failure,
        None,
        now_ms,
    )
    .await
}

/// [`submit`] under the caller's `id`, or with no candidate (`Err` gives the
/// detail) [`submit_failed`] with `no_candidate`. A task with that id already
/// stored is left as it is, so a caller that records the id before it submits
/// can submit again after a restart without making a second task.
pub async fn submit_as(
    pool: &PgPool,
    id: Uuid,
    request: &Request,
    candidates: std::result::Result<&[Candidate], &str>,
    now_ms: i64,
) -> Result<Uuid> {
    let (candidates, failure) = match candidates {
        Ok([]) => (&[][..], Some("no candidate is configured for this purpose")),
        Ok(candidates) => (candidates, None),
        Err(detail) => (&[][..], Some(detail)),
    };
    let failure = failure.map(|detail| (Reason::NoCandidate, detail));
    insert(pool, id, request, candidates, failure, None, now_ms).await
}

/// Stores `request` already failed with `reason` and `detail`.
pub async fn submit_failed(
    pool: &PgPool,
    request: &Request,
    reason: Reason,
    detail: &str,
    now_ms: i64,
) -> Result<Uuid> {
    let failure = Some((reason, detail));
    insert(pool, Uuid::new_v4(), request, &[], failure, None, now_ms).await
}

/// [`submit`], or [`submit_failed`] with `no_candidate` and `failure` as the
/// detail, for a retry: the link to the earlier run is part of the insert, so
/// the escalation step never sees the retry unlinked.
pub async fn submit_retry(
    pool: &PgPool,
    request: &Request,
    candidates: std::result::Result<&[Candidate], &str>,
    lineage: Lineage,
    now_ms: i64,
) -> Result<Uuid> {
    let (candidates, failure) = match candidates {
        Ok([]) => (&[][..], Some("no candidate is configured for this purpose")),
        Ok(candidates) => (candidates, None),
        Err(detail) => (&[][..], Some(detail)),
    };
    let failure = failure.map(|detail| (Reason::NoCandidate, detail));
    let lineage = Some(lineage);
    insert(
        pool,
        Uuid::new_v4(),
        request,
        candidates,
        failure,
        lineage,
        now_ms,
    )
    .await
}

/// Stores the task `id`; one already stored under `id` is left as it is.
async fn insert(
    pool: &PgPool,
    id: Uuid,
    request: &Request,
    candidates: &[Candidate],
    failure: Option<(Reason, &str)>,
    lineage: Option<Lineage>,
    now_ms: i64,
) -> Result<Uuid> {
    request.validate()?;
    let deadline_at_ms = now_ms + request.deadline_ms as i64;
    let failed = failure.is_some();
    sqlx::query(
        "INSERT INTO internal_tasks(id,purpose,request,candidates,status,blocking,deadline_at_ms,
             created_at_ms,finished_at_ms,reason,detail,affected_kind,affected_id,project_id,escalate,
             retry_of,supervisor_attempts,workspace_kind)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(to_text(&request.purpose))
    .bind(to_json(request))
    .bind(to_json(&candidates))
    .bind(if failed { "failed" } else { "queued" })
    .bind(request.blocking)
    .bind(deadline_at_ms)
    .bind(now_ms)
    .bind(failed.then_some(now_ms))
    .bind(failure.map(|(reason, _)| to_text(&reason)))
    .bind(failure.map(|(_, detail)| detail))
    .bind(&request.affected.kind)
    .bind(&request.affected.id)
    .bind(request.affected.project_id)
    .bind(request.escalate)
    .bind(lineage.map(|l| l.retry_of))
    .bind(lineage.map_or(0, |l| l.supervisor_attempts))
    .bind(if request.agent.is_some() { "agent" } else { "none" })
    .execute(pool)
    .await?;
    Ok(id)
}

/// [`claim`] for a login host, which takes an agent task only when `agents`
/// (it runs no agent now), so that one of its slots always serves answer
/// tasks.
pub async fn claim_login(
    pool: &PgPool,
    harnesses: &[&str],
    agents: bool,
    runner: &str,
    now_ms: i64,
) -> Result<Option<Claimed>> {
    claim_next(pool, Way::Login, harnesses, agents, runner, now_ms).await
}

/// Stores what candidate `candidate` of task `task` did as an agent: the
/// host's clipped and redacted `events`, a JSON array of at most
/// [`TRANSCRIPT_BYTES`]. A candidate runs once, so its first transcript is
/// kept and a later one ignored.
pub async fn save_transcript(
    pool: &PgPool,
    task: Uuid,
    candidate: u32,
    events: &Value,
) -> Result<()> {
    let bytes = events.to_string().len();
    if !events.is_array() || bytes > TRANSCRIPT_BYTES {
        return Err(Error::Invalid(
            "a transcript is a JSON array of at most 256 KiB".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO internal_task_transcripts(task_id,candidate_index,events,bytes)
         VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
    )
    .bind(task)
    .bind(i32::try_from(candidate).map_err(|_| Error::Corrupt)?)
    .bind(events)
    .bind(bytes as i32)
    .execute(pool)
    .await?;
    Ok(())
}

/// Each candidate's transcript of task `task`, in candidate order.
pub async fn transcripts(pool: &PgPool, task: Uuid) -> Result<Vec<(u32, Value)>> {
    let rows: Vec<(i32, Value)> = sqlx::query_as(
        "SELECT candidate_index,events FROM internal_task_transcripts
         WHERE task_id=$1 ORDER BY candidate_index",
    )
    .bind(task)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(index, events)| Ok((u32::try_from(index).map_err(|_| Error::Corrupt)?, events)))
        .collect()
}

/// Takes the next queued task whose current candidate is served `way` (for
/// [`Way::Login`], by one of `harnesses`): blocking tasks first, then oldest.
///
/// A Codex agent run holds the Codex login for up to 30 minutes, so an agent
/// task whose current candidate is Codex is taken only while no blocking
/// answer task ends on Codex: none is queued, or running on an earlier
/// candidate it may fail over from, with Codex as its last candidate.
pub async fn claim(
    pool: &PgPool,
    way: Way,
    harnesses: &[&str],
    runner: &str,
    now_ms: i64,
) -> Result<Option<Claimed>> {
    claim_next(pool, way, harnesses, true, runner, now_ms).await
}

async fn claim_next(
    pool: &PgPool,
    way: Way,
    harnesses: &[&str],
    agents: bool,
    runner: &str,
    now_ms: i64,
) -> Result<Option<Claimed>> {
    let row = sqlx::query(
        "WITH next AS (
             SELECT id FROM internal_tasks a
             WHERE status='queued'
               AND candidates->candidate_index->>'way'=$1
               AND ($1<>'login' OR candidates->candidate_index->>'harness'=ANY($2))
               AND ($5 OR workspace_kind<>'agent')
               AND NOT (workspace_kind='agent'
                   AND candidates->candidate_index->>'harness'='codex'
                   AND EXISTS(SELECT 1 FROM internal_tasks b
                       WHERE b.blocking AND b.workspace_kind<>'agent' AND b.id<>a.id
                         AND (b.status='queued' OR b.status='running'
                              AND b.candidate_index+1 < jsonb_array_length(b.candidates))
                         AND b.candidates->-1->>'way'='login'
                         AND b.candidates->-1->>'harness'='codex'))
             ORDER BY blocking DESC, created_at_ms, id
             LIMIT 1
             FOR UPDATE SKIP LOCKED)
         UPDATE internal_tasks t SET status='running', runner=$3, claimed_at_ms=$4
         FROM next WHERE t.id=next.id
         RETURNING t.*",
    )
    .bind(to_text(&way))
    .bind(harnesses)
    .bind(runner)
    .bind(now_ms)
    .bind(agents)
    .fetch_optional(pool)
    .await?;
    row.as_ref()
        .map(Row::decode)
        .transpose()?
        .map(claimed)
        .transpose()
}

/// The [`Claimed`] of a row a claim has just taken.
fn claimed(row: Row) -> Result<Claimed> {
    let candidate = row
        .candidates
        .get(row.candidate_index as usize)
        .cloned()
        .ok_or(Error::Corrupt)?;
    Ok(Claimed {
        id: row.id,
        request: row.request,
        candidate,
        candidate_index: row.candidate_index,
        deadline_at_ms: row.deadline_at_ms,
        claimed_at_ms: row.claimed_at_ms.ok_or(Error::Corrupt)?,
        candidate_count: row.candidates.len(),
    })
}

/// Takes the next queued task whose current candidate is served by the
/// login `harness` and may be passed over while that login is busy: a later
/// candidate exists, the deadline has not passed, and the task is blocking
/// or has less than `short_ms` left. Blocking tasks first, then oldest. The
/// caller hands it to [`pass_candidate`].
pub async fn claim_passable(
    pool: &PgPool,
    harness: &str,
    runner: &str,
    now_ms: i64,
    short_ms: i64,
) -> Result<Option<Claimed>> {
    let row = sqlx::query(
        "WITH next AS (
             SELECT id FROM internal_tasks
             WHERE status='queued'
               AND candidates->candidate_index->>'way'='login'
               AND candidates->candidate_index->>'harness'=$1
               AND candidate_index+1 < jsonb_array_length(candidates)
               AND deadline_at_ms > $3
               AND (blocking OR deadline_at_ms-$3 < $4)
             ORDER BY blocking DESC, created_at_ms, id
             LIMIT 1
             FOR UPDATE SKIP LOCKED)
         UPDATE internal_tasks t SET status='running', runner=$2, claimed_at_ms=$3
         FROM next WHERE t.id=next.id
         RETURNING t.*",
    )
    .bind(harness)
    .bind(runner)
    .bind(now_ms)
    .bind(short_ms)
    .fetch_optional(pool)
    .await?;
    row.as_ref()
        .map(Row::decode)
        .transpose()?
        .map(claimed)
        .transpose()
}

/// Passes over candidate `candidate_index` without running it: the attempt
/// is recorded with [`Reason::LoginBusy`] and `detail`, and the task moves
/// on to the next candidate. It never fails a task with that reason: when no
/// later candidate is left, or the deadline passed meanwhile, the task fails
/// as a queued task past its deadline would. Does nothing when that
/// candidate no longer holds the task.
pub async fn pass_candidate(
    pool: &PgPool,
    id: Uuid,
    candidate_index: u32,
    detail: &str,
    now_ms: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let Some(row) = held(&mut tx, id, candidate_index).await? else {
        return Ok(());
    };
    let next = candidate_index as usize + 1;
    if next < row.candidates.len() && now_ms < row.deadline_at_ms {
        let attempt = to_json(&attempt(&row, Some(Reason::LoginBusy), detail, now_ms)?);
        sqlx::query(
            "UPDATE internal_tasks SET status='queued', candidate_index=$2, runner=NULL,
                 claimed_at_ms=NULL, attempts=attempts||jsonb_build_array($3::jsonb)
             WHERE id=$1",
        )
        .bind(id)
        .bind(next as i32)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE internal_tasks SET status='failed', reason=$2, detail=$3, finished_at_ms=$4
             WHERE id=$1",
        )
        .bind(id)
        .bind(to_text(&Reason::DeadlineExceeded))
        .bind(QUEUED_PAST_DEADLINE)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Records the answer of candidate `candidate_index`. Does nothing when that
/// candidate no longer holds the task.
pub async fn complete(
    pool: &PgPool,
    id: Uuid,
    candidate_index: u32,
    answer: &Value,
    usage: Option<&Usage>,
    now_ms: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let Some(row) = held(&mut tx, id, candidate_index).await? else {
        return Ok(());
    };
    let attempt = attempt(&row, None, "", now_ms)?;
    sqlx::query(
        "UPDATE internal_tasks SET status='answered', answer=$2, usage=$3, finished_at_ms=$4,
             attempts=attempts||jsonb_build_array($5::jsonb)
         WHERE id=$1",
    )
    .bind(id)
    .bind(answer)
    .bind(usage.map(to_json))
    .bind(now_ms)
    .bind(to_json(&attempt))
    .execute(&mut *tx)
    .await?;
    // A retry that answers settles the failure it retried (spec 0012 §8).
    resolve_chain(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}

/// Records the failure of candidate `candidate_index`. A provider error,
/// unavailable provider, invalid output or per-candidate deadline moves the
/// task to the next candidate while one exists and the task deadline has not
/// passed; otherwise, and for any other reason, the task fails with `reason`.
/// Does nothing when that candidate no longer holds the task.
pub async fn fail_candidate(
    pool: &PgPool,
    id: Uuid,
    candidate_index: u32,
    reason: Reason,
    detail: &str,
    now_ms: i64,
) -> Result<()> {
    fail_held(pool, id, candidate_index, reason, detail, now_ms).await?;
    Ok(())
}

/// [`fail_candidate`], returning whether the candidate still held the task.
async fn fail_held(
    pool: &PgPool,
    id: Uuid,
    candidate_index: u32,
    reason: Reason,
    detail: &str,
    now_ms: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let Some(row) = held(&mut tx, id, candidate_index).await? else {
        return Ok(false);
    };
    let attempt = to_json(&attempt(&row, Some(reason), detail, now_ms)?);
    let retryable = matches!(
        reason,
        Reason::ProviderError
            | Reason::ProviderUnavailable
            | Reason::InvalidOutput
            | Reason::DeadlineExceeded
    );
    let next = candidate_index as usize + 1;
    if retryable && next < row.candidates.len() && now_ms < row.deadline_at_ms {
        sqlx::query(
            "UPDATE internal_tasks SET status='queued', candidate_index=$2, runner=NULL,
                 claimed_at_ms=NULL, attempts=attempts||jsonb_build_array($3::jsonb)
             WHERE id=$1",
        )
        .bind(id)
        .bind(next as i32)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE internal_tasks SET status='failed', reason=$2, detail=$3, finished_at_ms=$4,
                 attempts=attempts||jsonb_build_array($5::jsonb)
             WHERE id=$1",
        )
        .bind(id)
        .bind(to_text(&reason))
        .bind(detail)
        .bind(now_ms)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// [`candidate_budget_ms`] of a row's current attempt, in SQL over its
/// columns, or [`agent_candidate_budget_ms`] for agent work; `$2` is
/// [`MIN_CANDIDATE_BUDGET_MS`] and `$4` [`AGENT_FALLBACK_RESERVE_MS`].
/// Integer division truncates toward zero in both. NULL without a
/// `claimed_at_ms`, which only a claim sets; the sweep then falls back to the
/// task deadline.
const BUDGET_SQL: &str = "GREATEST(0, LEAST(deadline_at_ms-claimed_at_ms, CASE
    WHEN workspace_kind='agent' THEN GREATEST($4, deadline_at_ms-claimed_at_ms
        - $4*GREATEST(jsonb_array_length(candidates)-candidate_index-1, 0))
    ELSE GREATEST($2, (deadline_at_ms-claimed_at_ms)
        / GREATEST(jsonb_array_length(candidates)-candidate_index, 1)) END))";

/// Ends tasks whose deadline or runner is gone and returns how many it
/// changed. A `running` task whose runner has not reported within its
/// candidate budget ([`candidate_budget_ms`]) plus [`SWEEP_GRACE_MS`] fails
/// that candidate as `deadline_exceeded` with "runner did not report": a
/// live runner stops and reports by the end of the budget, so only a runner
/// that died gets here. The task moves on to the next
/// candidate, never back to `queued` on the same one, so a late runner stays
/// fenced out. A `queued` task past its deadline fails `deadline_exceeded`.
pub async fn sweep(pool: &PgPool, now_ms: i64) -> Result<u64> {
    let stale: Vec<(Uuid, i32)> = sqlx::query_as(&format!(
        "SELECT id,candidate_index FROM internal_tasks
         WHERE status='running' AND COALESCE(claimed_at_ms+{BUDGET_SQL}, deadline_at_ms)+$3 < $1"
    ))
    .bind(now_ms)
    .bind(MIN_CANDIDATE_BUDGET_MS)
    .bind(SWEEP_GRACE_MS)
    .bind(AGENT_FALLBACK_RESERVE_MS)
    .fetch_all(pool)
    .await?;
    let mut changed = 0;
    for (id, index) in stale {
        let index = u32::try_from(index).map_err(|_| Error::Corrupt)?;
        let reason = Reason::DeadlineExceeded;
        if fail_held(pool, id, index, reason, "runner did not report", now_ms).await? {
            changed += 1;
        }
    }
    let expired = sqlx::query(
        "UPDATE internal_tasks SET status='failed', reason=$2, detail=$3, finished_at_ms=$1
         WHERE status='queued' AND deadline_at_ms <= $1",
    )
    .bind(now_ms)
    .bind(to_text(&Reason::DeadlineExceeded))
    .bind(QUEUED_PAST_DEADLINE)
    .execute(pool)
    .await?;
    Ok(changed + expired.rows_affected())
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Row>> {
    sqlx::query("SELECT * FROM internal_tasks WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(Row::decode)
        .transpose()
}

/// Which runs [`list`] returns; `None` matches every value.
#[derive(Default)]
pub struct Filter {
    pub affected_kind: Option<String>,
    pub affected_id: Option<String>,
    pub purpose: Option<Purpose>,
    pub status: Option<String>,
}

/// Up to `limit` rows, newest first, strictly older than `cursor`
/// (`created_at_ms`, `id`).
pub async fn list(
    pool: &PgPool,
    filter: &Filter,
    cursor: Option<(i64, Uuid)>,
    limit: i64,
) -> Result<Vec<Row>> {
    sqlx::query(
        "SELECT * FROM internal_tasks
         WHERE ($1::text IS NULL OR affected_kind=$1) AND ($2::text IS NULL OR affected_id=$2)
           AND ($3::text IS NULL OR purpose=$3) AND ($4::text IS NULL OR status=$4)
           AND ($5::bigint IS NULL OR (created_at_ms,id)<($5,$6::uuid))
         ORDER BY created_at_ms DESC, id DESC LIMIT $7",
    )
    .bind(&filter.affected_kind)
    .bind(&filter.affected_id)
    .bind(filter.purpose.as_ref().map(to_text))
    .bind(&filter.status)
    .bind(cursor.map(|c| c.0))
    .bind(cursor.map(|c| c.1))
    .bind(limit)
    .fetch_all(pool)
    .await?
    .iter()
    .map(Row::decode)
    .collect()
}

/// Counts a supervisor's retry of the failed run `id` and returns the count
/// the retry carries.
pub async fn count_supervisor_attempt(pool: &PgPool, id: Uuid) -> Result<i32> {
    sqlx::query_scalar(
        "UPDATE internal_tasks SET supervisor_attempts=supervisor_attempts+1
         WHERE id=$1 AND status='failed' RETURNING supervisor_attempts",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::Conflict("Only a failed run can be retried".into()))
}

/// Moves failed runs on (spec 0012 §8) and returns those now with a
/// supervisor, each to be told once:
/// - a run that asked to escalate, or a retry of an escalated run, goes to a
///   supervisor; one carrying [`SUPERVISOR_ATTEMPTS`] goes to the owner;
/// - a purpose with [`FAILURE_BURST_RUNS`] failed runs within
///   [`FAILURE_BURST_WINDOW_MS`] sends its newest failed run to a supervisor,
///   unless a run of that purpose escalated within the window;
/// - a run with a supervisor for [`SUPERVISOR_SILENCE_MS`] that nobody
///   retried, escalated or resolved goes to the owner.
///
/// Every move is conditional on the run's current state, so two servers
/// never move, or tell a supervisor about, the same run twice.
pub async fn escalate_due(pool: &PgPool, now_ms: i64) -> Result<Vec<Row>> {
    const ASKED: &str = "t.status='failed' AND t.escalation='none'
        AND (t.escalate OR t.retry_of IS NOT NULL AND EXISTS(SELECT 1 FROM internal_tasks o
            WHERE o.id=t.retry_of AND o.escalation IN ('supervisor','owner')))";
    sqlx::query(&format!(
        "UPDATE internal_tasks t SET escalation='owner', escalated_at_ms=$1, diagnosis=$3
         WHERE {ASKED} AND t.supervisor_attempts>=$2"
    ))
    .bind(now_ms)
    .bind(SUPERVISOR_ATTEMPTS)
    .bind("supervisor attempts exhausted")
    .execute(pool)
    .await?;
    let mut moved = sqlx::query(&format!(
        "UPDATE internal_tasks t SET escalation='supervisor', escalated_at_ms=$1
         WHERE {ASKED} AND t.supervisor_attempts<$2 RETURNING t.*"
    ))
    .bind(now_ms)
    .bind(SUPERVISOR_ATTEMPTS)
    .fetch_all(pool)
    .await?;
    let burst = sqlx::query(
        "WITH recent AS (
             SELECT id,purpose,finished_at_ms FROM internal_tasks
             WHERE status='failed' AND finished_at_ms>$1-$3),
         newest AS (
             SELECT DISTINCT ON (r.purpose) r.id FROM recent r
             WHERE r.purpose IN (SELECT purpose FROM recent GROUP BY purpose HAVING count(*)>=$2)
               AND NOT EXISTS(SELECT 1 FROM internal_tasks e
                   WHERE e.purpose=r.purpose AND e.escalated_at_ms>$1-$3)
             ORDER BY r.purpose, r.finished_at_ms DESC, r.id DESC)
         UPDATE internal_tasks t SET escalation='supervisor', escalated_at_ms=$1
         FROM newest WHERE t.id=newest.id AND t.escalation='none' RETURNING t.*",
    )
    .bind(now_ms)
    .bind(FAILURE_BURST_RUNS)
    .bind(FAILURE_BURST_WINDOW_MS)
    .fetch_all(pool)
    .await?;
    moved.extend(burst);
    sqlx::query(
        "UPDATE internal_tasks t SET escalation='owner', escalated_at_ms=$1, diagnosis=$3
         WHERE t.escalation='supervisor' AND t.escalated_at_ms<=$1-$2
           AND NOT EXISTS(SELECT 1 FROM internal_tasks r WHERE r.retry_of=t.id)",
    )
    .bind(now_ms)
    .bind(SUPERVISOR_SILENCE_MS)
    .bind("supervisor did not respond")
    .execute(pool)
    .await?;
    moved.iter().map(Row::decode).collect()
}

/// Sends the failed run `id` to the owner with `diagnosis` (a supervisor's
/// `/escalate`).
pub async fn mark_owner(pool: &PgPool, id: Uuid, diagnosis: &str, now_ms: i64) -> Result<()> {
    let moved = sqlx::query(
        "UPDATE internal_tasks SET escalation='owner', diagnosis=$2, escalated_at_ms=$3
         WHERE id=$1 AND status='failed' AND escalation<>'resolved'",
    )
    .bind(id)
    .bind(diagnosis)
    .bind(now_ms)
    .execute(pool)
    .await?;
    if moved.rows_affected() == 0 {
        found(pool, id).await?;
        return Err(Error::Conflict(
            "Only an unresolved failed run can be escalated".into(),
        ));
    }
    Ok(())
}

/// Marks the escalated run `id`, and every escalated run it retried, resolved.
pub async fn resolve(pool: &PgPool, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let escalated: Option<String> =
        sqlx::query_scalar("SELECT escalation FROM internal_tasks WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    match escalated.as_deref() {
        None => return Err(not_found()),
        Some("supervisor" | "owner") => {}
        Some(_) => {
            return Err(Error::Conflict(
                "Only a run with a supervisor or the owner can be resolved".into(),
            ));
        }
    }
    resolve_chain(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}

/// `id` and the runs it retried, back to the first: those with a supervisor
/// or the owner become `resolved`.
async fn resolve_chain(tx: &mut sqlx::PgConnection, id: Uuid) -> Result<()> {
    sqlx::query(
        "WITH RECURSIVE chain(id) AS (
             SELECT $1::uuid
             UNION SELECT t.retry_of FROM internal_tasks t JOIN chain c ON t.id=c.id
                 WHERE t.retry_of IS NOT NULL)
         UPDATE internal_tasks SET escalation='resolved'
         WHERE id IN (SELECT id FROM chain) AND escalation IN ('supervisor','owner')",
    )
    .bind(id)
    .execute(tx)
    .await?;
    Ok(())
}

async fn found(pool: &PgPool, id: Uuid) -> Result<()> {
    get(pool, id).await?.map(drop).ok_or_else(not_found)
}

fn not_found() -> Error {
    Error::NotFound("Internal run not found".into())
}

/// The locked row, when candidate `candidate_index` is still running it.
async fn held(tx: &mut sqlx::PgConnection, id: Uuid, candidate_index: u32) -> Result<Option<Row>> {
    sqlx::query(
        "SELECT * FROM internal_tasks WHERE id=$1 AND candidate_index=$2 AND status='running'
         FOR UPDATE",
    )
    .bind(id)
    .bind(candidate_index as i32)
    .fetch_optional(tx)
    .await?
    .as_ref()
    .map(Row::decode)
    .transpose()
}

fn attempt(row: &Row, reason: Option<Reason>, detail: &str, now_ms: i64) -> Result<Attempt> {
    let candidate = row
        .candidates
        .get(row.candidate_index as usize)
        .ok_or(Error::Corrupt)?;
    Ok(Attempt {
        provider: candidate.provider.clone(),
        model: candidate.model.clone(),
        way: candidate.way,
        reason,
        detail: detail.into(),
        elapsed_ms: elapsed(row.claimed_at_ms.unwrap_or(now_ms), now_ms),
    })
}

fn elapsed(from_ms: i64, to_ms: i64) -> u64 {
    u64::try_from(to_ms - from_ms).unwrap_or(0)
}

/// The snake_case name serde gives a unit variant.
fn to_text<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => unreachable!("unit enum variants serialize to strings"),
    }
}

fn from_text<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_value(Value::String(text.into())).map_err(|_| Error::Corrupt)
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Value {
    serde_json::to_value(value).expect("internal work types serialize to JSON")
}

fn from_json<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| Error::Corrupt)
}
