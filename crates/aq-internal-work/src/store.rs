//! The `internal_tasks` queue (spec 0012 §4). Runners claim with
//! `FOR UPDATE SKIP LOCKED`; every write after a claim is fenced on the row
//! still being `running` for the same `candidate_index`, so a superseded
//! runner cannot overwrite a newer attempt or a finished task.
use crate::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::{PgPool, postgres::PgRow};

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
        })
    }
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
    insert(pool, request, candidates, failure, now_ms).await
}

/// Stores `request` already failed with `reason` and `detail`.
pub async fn submit_failed(
    pool: &PgPool,
    request: &Request,
    reason: Reason,
    detail: &str,
    now_ms: i64,
) -> Result<Uuid> {
    insert(pool, request, &[], Some((reason, detail)), now_ms).await
}

async fn insert(
    pool: &PgPool,
    request: &Request,
    candidates: &[Candidate],
    failure: Option<(Reason, &str)>,
    now_ms: i64,
) -> Result<Uuid> {
    request.validate()?;
    let id = Uuid::new_v4();
    let deadline_at_ms = now_ms + request.deadline_ms as i64;
    let failed = failure.is_some();
    sqlx::query(
        "INSERT INTO internal_tasks(id,purpose,request,candidates,status,blocking,deadline_at_ms,
             created_at_ms,finished_at_ms,reason,detail,affected_kind,affected_id,project_id,escalate)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
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
    .execute(pool)
    .await?;
    Ok(id)
}

/// Takes the next queued task whose current candidate is served `way` (for
/// [`Way::Login`], by one of `harnesses`): blocking tasks first, then oldest.
pub async fn claim(
    pool: &PgPool,
    way: Way,
    harnesses: &[&str],
    runner: &str,
    now_ms: i64,
) -> Result<Option<Claimed>> {
    let row = sqlx::query(
        "WITH next AS (
             SELECT id FROM internal_tasks
             WHERE status='queued'
               AND candidates->candidate_index->>'way'=$1
               AND ($1<>'login' OR candidates->candidate_index->>'harness'=ANY($2))
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
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let row = Row::decode(&row)?;
    let candidate = row
        .candidates
        .get(row.candidate_index as usize)
        .cloned()
        .ok_or(Error::Corrupt)?;
    Ok(Some(Claimed {
        id: row.id,
        request: row.request,
        candidate,
        candidate_index: row.candidate_index,
        deadline_at_ms: row.deadline_at_ms,
    }))
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

/// Ends tasks whose deadline or runner is gone and returns how many it
/// changed. A `running` task whose runner has not reported within the
/// candidate budget ([`CANDIDATE_TIMEOUT_MS`], or less when the task deadline
/// is nearer) plus [`SWEEP_GRACE_MS`] fails that candidate as
/// `deadline_exceeded`, exactly as the runner would have: it moves on to the
/// next candidate, never back to `queued` on the same one, so a late runner
/// stays fenced out. A `queued` task past its deadline fails
/// `deadline_exceeded`.
pub async fn sweep(pool: &PgPool, now_ms: i64) -> Result<u64> {
    let stale: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT id,candidate_index FROM internal_tasks
         WHERE status='running' AND LEAST(claimed_at_ms+$2, deadline_at_ms)+$3 < $1",
    )
    .bind(now_ms)
    .bind(CANDIDATE_TIMEOUT_MS)
    .bind(SWEEP_GRACE_MS)
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
    .bind("deadline passed before a runner took the task")
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

/// Marks `id` as the retry of `of`.
pub async fn set_retry_of(pool: &PgPool, id: Uuid, of: Uuid) -> Result<()> {
    sqlx::query("UPDATE internal_tasks SET retry_of=$2 WHERE id=$1")
        .bind(id)
        .bind(of)
        .execute(pool)
        .await?;
    Ok(())
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
