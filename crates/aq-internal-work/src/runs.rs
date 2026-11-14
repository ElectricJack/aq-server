//! The read side of internal work: runs as the dashboard shows them (spec
//! 0012 §7). A run is a stored task; nothing here reads credentials or config.
use crate::store::{self, Filter, Row};
use crate::{Affected, Attempt, Error, Outcome, Purpose, Result, Usage, Way};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 100;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    affected_kind: Option<String>,
    affected_id: Option<String>,
    purpose: Option<String>,
    status: Option<String>,
    /// `"created_at_ms:uuid"`, the previous page's `next_cursor`.
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Serialize)]
struct Run {
    id: Uuid,
    purpose: Purpose,
    status: String,
    affected: Affected,
    instructions: String,
    input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<crate::Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    attempts: Vec<Attempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    way: Option<Way>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
    created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at_ms: Option<i64>,
    escalation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnosis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_of: Option<Uuid>,
    /// What each candidate's agent did; only the read of one run has it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    transcripts: Vec<Transcript>,
}

#[derive(Serialize)]
struct Transcript {
    candidate_index: u32,
    events: Value,
}

/// `row` as the dashboard shows it, with the `transcripts` of its agent
/// candidates (none in a list).
fn run(row: Row, transcripts: Vec<(u32, Value)>) -> Value {
    let outcome = row.outcome();
    let mut run = Run {
        id: row.id,
        purpose: row.purpose,
        status: row.status,
        affected: Affected {
            kind: row.affected_kind,
            id: row.affected_id,
            project_id: row.project_id,
        },
        instructions: row.request.instructions,
        input: row.request.input,
        answer: None,
        reason: row.reason,
        detail: row.detail,
        attempts: row.attempts,
        provider: None,
        model: None,
        way: None,
        elapsed_ms: None,
        usage: None,
        created_at_ms: row.created_at_ms,
        finished_at_ms: row.finished_at_ms,
        escalation: row.escalation,
        diagnosis: row.diagnosis,
        retry_of: row.retry_of,
        transcripts: transcripts
            .into_iter()
            .map(|(candidate_index, events)| Transcript {
                candidate_index,
                events,
            })
            .collect(),
    };
    if let Some(Outcome::Answered {
        answer,
        provider,
        model,
        way,
        elapsed_ms,
        usage,
        ..
    }) = outcome
    {
        run.answer = Some(answer);
        run.provider = Some(provider);
        run.model = Some(model);
        run.way = Some(way);
        run.elapsed_ms = Some(elapsed_ms);
        run.usage = usage;
    }
    serde_json::to_value(run).expect("a run serializes to JSON")
}

fn bad(message: &str) -> Error {
    Error::Invalid(message.into())
}

/// `{runs, next_cursor}`, newest first.
pub async fn list(pool: &PgPool, q: Query) -> Result<Value> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(bad("Invalid page limit"));
    }
    let purpose = q
        .purpose
        .map(|p| {
            serde_json::from_value::<Purpose>(Value::String(p)).map_err(|_| bad("Invalid purpose"))
        })
        .transpose()?;
    if q.status
        .as_deref()
        .is_some_and(|s| !["queued", "running", "answered", "failed"].contains(&s))
    {
        return Err(bad("Invalid status"));
    }
    let cursor = q
        .cursor
        .map(|c| {
            let (ms, id) = c.split_once(':').ok_or_else(|| bad("Invalid cursor"))?;
            let ms = ms.parse::<i64>().map_err(|_| bad("Invalid cursor"))?;
            Ok::<_, Error>((ms, Uuid::parse_str(id).map_err(|_| bad("Invalid cursor"))?))
        })
        .transpose()?;
    let filter = Filter {
        affected_kind: q.affected_kind,
        affected_id: q.affected_id,
        purpose,
        status: q.status,
    };
    let mut rows = store::list(pool, &filter, cursor, i64::from(limit) + 1).await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = rows
        .last()
        .filter(|_| more)
        .map(|r| format!("{}:{}", r.created_at_ms, r.id));
    let runs: Vec<Value> = rows.into_iter().map(|row| run(row, Vec::new())).collect();
    Ok(json!({"runs": runs, "next_cursor": next}))
}

pub async fn read(pool: &PgPool, id: Uuid) -> Result<Value> {
    let row = store::get(pool, id)
        .await?
        .ok_or_else(|| Error::NotFound("Internal run not found".into()))?;
    Ok(run(row, store::transcripts(pool, id).await?))
}
