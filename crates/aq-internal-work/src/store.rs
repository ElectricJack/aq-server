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

mod attempts;
mod claims;
mod escalation;
mod submission;

pub use attempts::{CandidateAttempt, sweep};
pub use escalation::{count_supervisor_attempt, escalate_due, mark_owner, resolve};
pub use submission::{FailureCause, Lineage, resolve_failures, submit};

/// The task queue at the instant of an operation. All writes made by the
/// operation use this timestamp, including the attempt history.
#[derive(Clone, Copy)]
pub struct Queue<'a> {
    pool: &'a PgPool,
    now_ms: i64,
}

impl<'a> Queue<'a> {
    pub fn at(pool: &'a PgPool, now_ms: i64) -> Self {
        Self { pool, now_ms }
    }
}

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
