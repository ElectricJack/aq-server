//! The `playbook_authoring_runs` table: one row per authoring of a source,
//! recorded before its internal task is submitted, so a crash in between
//! leaves a row [`stranded`] finds again.
use crate::{Error, Host, Result, facts_digest, now_ms, source_digest, store};
use aq_internal_work::Purpose;
use serde_json::Value;
use sqlx::{PgExecutor, PgPool, types::Json};
use uuid::Uuid;

/// How long a run may be recorded without its internal task before
/// [`stranded`] returns it: the request is submitted within this time unless
/// the server stopped in between.
pub const GRACE_MS: i64 = 30_000;

/// A recorded run, as [`stranded`] returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub id: i64,
    pub source_id: String,
    pub source_revision: u64,
    pub source_digest: String,
    pub facts_digest: String,
    pub created_at_ms: i64,
}

/// The saved run whose artifact is active on its source's binding.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveRun {
    pub id: i64,
    pub source_revision: u64,
    pub source_digest: String,
    pub facts_digest: String,
    pub artifact_digest: String,
    pub artifact: Value,
    pub finished_at_ms: Option<i64>,
}

fn revision(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Corrupt)
}

/// Records a run of `source` for its Markdown and facts now, as the host
/// gives them, and supersedes every older run of the source still running.
/// Nothing is recorded, and the result is `None`, when the Markdown and the
/// facts are those the active run was authored from. Requests for one source
/// are taken one at a time, so the newest run is always of the newest text.
pub async fn requested<H: Host>(host: &H, source: &str) -> Result<Option<i64>> {
    let mut tx = host.as_ref().begin().await?;
    // The lock orders requests of this source: the host is read under it.
    let record = (store::locked(&mut *tx, source).await?)
        .ok_or_else(|| Error::NotFound(format!("There is no playbook source {source}")))?;
    let (markdown, source_revision) = host.markdown(&record).await?;
    let facts = host.facts(&record).await?;
    let (text, facts) = (source_digest(&markdown), facts_digest(&facts));
    let unchanged: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_authoring_runs WHERE source_id=$1
            AND status='saved' AND active AND source_digest=$2 AND facts_digest=$3)",
    )
    .bind(source)
    .bind(&text)
    .bind(&facts)
    .fetch_one(&mut *tx)
    .await?;
    if unchanged {
        tracing::debug!(source, "playbook source unchanged since its active run");
        tx.commit().await?;
        return Ok(None);
    }
    let now = now_ms();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO playbook_authoring_runs(source_id,source_revision,source_digest,
            facts_digest,created_at_ms) VALUES($1,$2,$3,$4,$5) RETURNING id",
    )
    .bind(source)
    .bind(
        i64::try_from(source_revision)
            .map_err(|_| Error::Host(format!("{source} has an out-of-range revision")))?,
    )
    .bind(&text)
    .bind(&facts)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    supersede(&mut *tx, source, id, now).await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Supersedes every run of `source` older than `run` that is still running.
/// Returns how many it superseded.
pub async fn supersede_older(pool: &PgPool, source: &str, run: i64) -> Result<u64> {
    supersede(pool, source, run, now_ms()).await
}

async fn supersede(db: impl PgExecutor<'_>, source: &str, run: i64, now: i64) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE playbook_authoring_runs SET status='superseded',finished_at_ms=$3
         WHERE source_id=$1 AND id<$2 AND status='running'",
    )
    .bind(source)
    .bind(run)
    .bind(now)
    .execute(db)
    .await?;
    Ok(done.rows_affected())
}

/// The run whose artifact is active on `source`'s binding, if one is.
pub async fn active(pool: &PgPool, source: &str) -> Result<Option<ActiveRun>> {
    type Row = (i64, i64, String, String, String, Json<Value>, Option<i64>);
    let row: Option<Row> = sqlx::query_as(
        "SELECT id,source_revision,source_digest,facts_digest,artifact_digest,artifact,
            finished_at_ms FROM playbook_authoring_runs
         WHERE source_id=$1 AND status='saved' AND active",
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let Some((id, source_revision, source_digest, facts_digest, digest, artifact, finished)) = row
    else {
        return Ok(None);
    };
    Ok(Some(ActiveRun {
        id,
        source_revision: revision(source_revision)?,
        source_digest,
        facts_digest,
        artifact_digest: digest,
        artifact: artifact.0,
        finished_at_ms: finished,
    }))
}

/// The source an artifact was authored for, and the purpose its judgments
/// run under (spec 0013 §6.5).
pub async fn run_for_artifact(pool: &PgPool, digest: &str) -> Result<Option<(String, Purpose)>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT s.id,s.judgment_purpose FROM playbook_authoring_runs r
         JOIN playbook_sources s ON s.id=r.source_id
         WHERE r.artifact_digest=$1 ORDER BY r.id DESC LIMIT 1",
    )
    .bind(digest)
    .fetch_optional(pool)
    .await?;
    let Some((source, purpose)) = row else {
        return Ok(None);
    };
    let purpose = serde_json::from_value(Value::String(purpose)).map_err(|_| Error::Corrupt)?;
    Ok(Some((source, purpose)))
}

/// Records `internal_task` as the task of `run`, if the run is still running
/// and has none yet. Returns whether it did.
pub async fn attach(pool: &PgPool, run: i64, internal_task: Uuid) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE playbook_authoring_runs SET internal_task_id=$2
         WHERE id=$1 AND status='running' AND internal_task_id IS NULL",
    )
    .bind(run)
    .bind(internal_task)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Every run still running with no internal task that was recorded more than
/// [`GRACE_MS`] before `now_ms`: the server stopped between recording and
/// submitting it. Oldest first.
pub async fn stranded(pool: &PgPool, now_ms: i64) -> Result<Vec<Run>> {
    let rows: Vec<(i64, String, i64, String, String, i64)> = sqlx::query_as(
        "SELECT id,source_id,source_revision,source_digest,facts_digest,created_at_ms
         FROM playbook_authoring_runs
         WHERE status='running' AND internal_task_id IS NULL AND created_at_ms<$1 ORDER BY id",
    )
    .bind(now_ms - GRACE_MS)
    .fetch_all(pool)
    .await?;
    (rows.into_iter())
        .map(
            |(id, source_id, rev, source_digest, facts_digest, created_at_ms)| {
                Ok(Run {
                    id,
                    source_id,
                    source_revision: revision(rev)?,
                    source_digest,
                    facts_digest,
                    created_at_ms,
                })
            },
        )
        .collect()
}
