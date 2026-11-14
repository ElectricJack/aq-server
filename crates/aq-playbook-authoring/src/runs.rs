//! The `playbook_authoring_runs` table: one row per authoring of a source.
//!
//! The order of a run's life, which makes each step safe to repeat after a
//! restart or with two resumers racing:
//! 1. [`requested`] records the run, before anything is submitted.
//! 2. [`task_for`] gives the run its internal task id, once: every caller gets
//!    the same id.
//! 3. The caller submits the task under that id
//!    (`aq_internal_work::service::Service::submit_as`), which makes no second
//!    task when the id is already stored.
//! 4. [`finish`] records the result, once, and only while the run is still
//!    running, so a superseded run never becomes active.
//!
//! After a restart, [`stranded`] lists runs stopped before step 2 and
//! [`in_flight`] lists runs between steps 2 and 4.
use crate::{Error, Host, Result, facts_digest, now_ms, record_digest, source_digest, store};
use aq_internal_work::Purpose;
use serde_json::Value;
use sqlx::{PgExecutor, PgPool, postgres::PgRow, types::Json};
use uuid::Uuid;

/// How long a run may be recorded without its internal task before
/// [`stranded`] returns it: the task is submitted within this time unless
/// the server stopped in between.
pub const GRACE_MS: i64 = 30_000;
/// How many times [`requested`] reads the host again when the source record
/// changed while it read.
const READS: usize = 3;

/// A recorded run, as [`stranded`] and [`in_flight`] return it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub id: i64,
    pub source_id: String,
    pub source_revision: u64,
    pub source_digest: String,
    pub facts_digest: String,
    pub record_digest: String,
    pub internal_task_id: Option<Uuid>,
    pub created_at_ms: i64,
}

/// The saved run whose artifact is active on its source's binding.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveRun {
    pub id: i64,
    pub source_revision: u64,
    pub source_digest: String,
    pub facts_digest: String,
    pub record_digest: String,
    pub artifact_digest: String,
    pub artifact: Value,
    /// The artifact a rollback would bring back.
    pub previous_digest: Option<String>,
    pub finished_at_ms: Option<i64>,
    /// When the artifact took effect (again, after a rollback).
    pub activated_at_ms: Option<i64>,
}

/// How a run ended, as [`finish`] records it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ended {
    Saved,
    Question,
    Unable,
    Refused,
    #[default]
    Failed,
}

impl Ended {
    pub fn as_str(self) -> &'static str {
        match self {
            Ended::Saved => "saved",
            Ended::Question => "question",
            Ended::Unable => "unable",
            Ended::Refused => "refused",
            Ended::Failed => "failed",
        }
    }
}

/// The result [`finish`] records. `activate` makes a saved run the source's
/// active one, in place of the run active before.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Finished {
    pub status: Ended,
    pub activate: bool,
    pub artifact_digest: Option<String>,
    pub artifact: Option<Value>,
    pub previous_digest: Option<String>,
    pub comparison: Option<Value>,
    pub cases: Option<Value>,
    pub question: Option<Value>,
    pub unable: Option<Value>,
    pub refusal: Option<String>,
    /// The document a saved run was authored from.
    pub markdown: Option<String>,
    /// The cases a saved run was saved with, as the agent wrote them.
    pub case_files: Option<Value>,
}

fn revision(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Corrupt)
}

fn not_found(source: &str) -> Error {
    Error::NotFound(format!("There is no playbook source {source}"))
}

/// What one request read: the host's Markdown revision and the digests.
struct Read {
    revision: i64,
    source: String,
    facts: String,
    record: String,
}

/// Records a run of `source` for its Markdown and facts now, as the host
/// gives them, and supersedes every older run of the source still running.
///
/// Nothing is recorded, and the result is `None`, when:
/// - the Markdown, facts and record are those the active run was authored
///   from. Every run still running is then superseded, so reverting to the
///   active text stops the authoring of the text in between;
/// - a request that read a newer Markdown revision came first. This request's
///   read is stale.
///
/// A run already running for exactly what was read is returned as it is.
/// The host is read with no connection or lock held. The record is written
/// under the source's row lock, after checking that what was read is still
/// current, so this holds across server processes.
pub async fn requested<H: Host>(host: &H, source: &str) -> Result<Option<i64>> {
    let pool = host.as_ref();
    for _ in 0..READS {
        let record = (store::get(pool, source).await?).ok_or_else(|| not_found(source))?;
        let (markdown, markdown_revision) = host.markdown(&record).await?;
        let facts = host.facts(&record).await?;
        let read = Read {
            revision: i64::try_from(markdown_revision)
                .map_err(|_| Error::Host(format!("{source} has an out-of-range revision")))?,
            source: source_digest(&markdown),
            facts: facts_digest(&facts),
            record: record_digest(&record),
        };
        if let Some(run) = record_read(pool, source, record.revision, &read).await? {
            return Ok(run);
        }
    }
    Err(Error::Conflict(format!(
        "The playbook source {source} kept changing while its authoring was requested"
    )))
}

/// Records what one request read. `None` when the record changed since it
/// was read, so the host must be read again.
async fn record_read(
    pool: &PgPool,
    source: &str,
    record_revision: u64,
    read: &Read,
) -> Result<Option<Option<i64>>> {
    let mut tx = pool.begin().await?;
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT revision,requested_revision FROM playbook_sources WHERE id=$1 FOR UPDATE",
    )
    .bind(source)
    .fetch_optional(&mut *tx)
    .await?;
    let (stored, seen) = row.ok_or_else(|| not_found(source))?;
    if revision(stored)? != record_revision {
        return Ok(None);
    }
    if read.revision < seen {
        tracing::debug!(
            source,
            "authoring request read older Markdown than one before it"
        );
        return Ok(Some(None));
    }
    sqlx::query("UPDATE playbook_sources SET requested_revision=$2 WHERE id=$1")
        .bind(source)
        .bind(read.revision)
        .execute(&mut *tx)
        .await?;
    let now = now_ms();
    let unchanged: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_authoring_runs WHERE source_id=$1
            AND status='saved' AND active AND source_digest=$2 AND facts_digest=$3
            AND record_digest=$4)",
    )
    .bind(source)
    .bind(&read.source)
    .bind(&read.facts)
    .bind(&read.record)
    .fetch_one(&mut *tx)
    .await?;
    if unchanged {
        tracing::debug!(source, "playbook source unchanged since its active run");
        supersede(&mut *tx, source, i64::MAX, now).await?;
        tx.commit().await?;
        return Ok(Some(None));
    }
    // Every request supersedes the runs before it, so at most one is running.
    let running: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM playbook_authoring_runs WHERE source_id=$1 AND status='running'
            AND source_digest=$2 AND facts_digest=$3 AND record_digest=$4",
    )
    .bind(source)
    .bind(&read.source)
    .bind(&read.facts)
    .bind(&read.record)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = running {
        tx.commit().await?;
        return Ok(Some(Some(id)));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO playbook_authoring_runs(source_id,source_revision,source_digest,
            facts_digest,record_digest,created_at_ms) VALUES($1,$2,$3,$4,$5,$6) RETURNING id",
    )
    .bind(source)
    .bind(read.revision)
    .bind(&read.source)
    .bind(&read.facts)
    .bind(&read.record)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    supersede(&mut *tx, source, id, now).await?;
    tx.commit().await?;
    Ok(Some(Some(id)))
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

/// The internal task id of `run`: the one it has, or a new one recorded now.
/// Every caller gets the same id, so two resumers submit one task. `None`
/// once the run is no longer running.
pub async fn task_for(pool: &PgPool, run: i64) -> Result<Option<Uuid>> {
    let id: Option<Option<Uuid>> = sqlx::query_scalar(
        "UPDATE playbook_authoring_runs SET internal_task_id=COALESCE(internal_task_id,$2)
         WHERE id=$1 AND status='running' RETURNING internal_task_id",
    )
    .bind(run)
    .bind(Uuid::new_v4())
    .fetch_optional(pool)
    .await?;
    Ok(id.flatten())
}

/// Records how `run` ended, if it is still running; returns whether it did.
/// A run that finished already, or was superseded, is left as it is, so a
/// result is recorded once and a superseded run never becomes active.
pub async fn finish(pool: &PgPool, run: i64, finished: &Finished) -> Result<bool> {
    let saved = finished.status == Ended::Saved;
    if finished.activate && !saved {
        return Err(Error::Invalid("Only a saved run can become active".into()));
    }
    if saved && (finished.artifact_digest.is_none() || finished.artifact.is_none()) {
        return Err(Error::Invalid("A saved run needs its artifact".into()));
    }
    let mut tx = pool.begin().await?;
    // Source before run, the order `requested` locks in.
    let source: Option<String> = sqlx::query_scalar(
        "SELECT s.id FROM playbook_sources s JOIN playbook_authoring_runs r ON r.source_id=s.id
         WHERE r.id=$1 FOR UPDATE OF s",
    )
    .bind(run)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(source) = source else {
        return Ok(false);
    };
    let running: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_authoring_runs WHERE id=$1 AND status='running')",
    )
    .bind(run)
    .fetch_one(&mut *tx)
    .await?;
    if !running {
        return Ok(false);
    }
    if finished.activate {
        sqlx::query(
            "UPDATE playbook_authoring_runs SET active=FALSE WHERE source_id=$1 AND active",
        )
        .bind(&source)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE playbook_authoring_runs SET status=$2,active=$3,artifact_digest=$4,artifact=$5,
            previous_digest=$6,comparison=$7,cases=$8,question=$9,unable=$10,refusal=$11,
            finished_at_ms=$12,markdown=$13,case_files=$14,
            activated_at_ms=CASE WHEN $3 THEN $12 END,deployed=NOT $3 WHERE id=$1",
    )
    .bind(run)
    .bind(finished.status.as_str())
    .bind(finished.activate)
    .bind(&finished.artifact_digest)
    .bind(finished.artifact.as_ref().map(Json))
    .bind(&finished.previous_digest)
    .bind(finished.comparison.as_ref().map(Json))
    .bind(finished.cases.as_ref().map(Json))
    .bind(finished.question.as_ref().map(Json))
    .bind(finished.unable.as_ref().map(Json))
    .bind(&finished.refusal)
    .bind(now_ms())
    .bind(&finished.markdown)
    .bind(finished.case_files.as_ref().map(Json))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// The run whose artifact is active on `source`'s binding, if one is.
pub async fn active(pool: &PgPool, source: &str) -> Result<Option<ActiveRun>> {
    type Row = (
        i64,
        i64,
        String,
        String,
        String,
        String,
        Json<Value>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT id,source_revision,source_digest,facts_digest,record_digest,artifact_digest,
            artifact,previous_digest,finished_at_ms,activated_at_ms FROM playbook_authoring_runs
         WHERE source_id=$1 AND status='saved' AND active",
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let Some((
        id,
        rev,
        source_digest,
        facts_digest,
        record_digest,
        digest,
        artifact,
        previous,
        finished,
        activated,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(ActiveRun {
        id,
        source_revision: revision(rev)?,
        source_digest,
        facts_digest,
        record_digest,
        artifact_digest: digest,
        artifact: artifact.0,
        previous_digest: previous,
        finished_at_ms: finished,
        activated_at_ms: activated,
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

const RUN_COLUMNS: &str = "id,source_id,source_revision,source_digest,facts_digest,record_digest,
    internal_task_id,created_at_ms";

fn decode(row: &PgRow) -> Result<Run> {
    use sqlx::Row as _;
    Ok(Run {
        id: row.try_get("id")?,
        source_id: row.try_get("source_id")?,
        source_revision: revision(row.try_get("source_revision")?)?,
        source_digest: row.try_get("source_digest")?,
        facts_digest: row.try_get("facts_digest")?,
        record_digest: row.try_get("record_digest")?,
        internal_task_id: row.try_get("internal_task_id")?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}

/// Every run still running with no internal task that was recorded more than
/// [`GRACE_MS`] before `now_ms`: the server stopped between recording it and
/// [`task_for`]. Oldest first.
pub async fn stranded(pool: &PgPool, now_ms: i64) -> Result<Vec<Run>> {
    let rows = sqlx::query(&format!(
        "SELECT {RUN_COLUMNS} FROM playbook_authoring_runs
         WHERE status='running' AND internal_task_id IS NULL AND created_at_ms<$1 ORDER BY id"
    ))
    .bind(now_ms - GRACE_MS)
    .fetch_all(pool)
    .await?;
    rows.iter().map(decode).collect()
}

/// Every run still running that has its internal task id: its task may be
/// unsubmitted (the server stopped after [`task_for`]), running, or finished
/// with its result not yet recorded. Oldest first.
pub async fn in_flight(pool: &PgPool) -> Result<Vec<Run>> {
    let rows = sqlx::query(&format!(
        "SELECT {RUN_COLUMNS} FROM playbook_authoring_runs
         WHERE status='running' AND internal_task_id IS NOT NULL ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(decode).collect()
}

/// The run still running whose internal task is `task`.
pub async fn running_for_task(pool: &PgPool, task: Uuid) -> Result<Option<Run>> {
    let row = sqlx::query(&format!(
        "SELECT {RUN_COLUMNS} FROM playbook_authoring_runs
         WHERE internal_task_id=$1 AND status='running'"
    ))
    .bind(task)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(decode).transpose()
}

/// The run `id`, while it is still running.
pub async fn running(pool: &PgPool, id: i64) -> Result<Option<Run>> {
    let row = sqlx::query(&format!(
        "SELECT {RUN_COLUMNS} FROM playbook_authoring_runs WHERE id=$1 AND status='running'"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(decode).transpose()
}

/// Rolling deletion: deletes the finished runs of `source` beyond its newest
/// `keep`. The active run, the one a rollback would bring back and a run
/// still running are never deleted. Returns how many it deleted.
pub async fn prune(pool: &PgPool, source: &str, keep: u64) -> Result<u64> {
    let keep = i64::try_from(keep).unwrap_or(i64::MAX);
    // Under the source's lock, as a save and a rollback are: what a rollback
    // has chosen to bring back is not deleted under it.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT 1 FROM playbook_sources WHERE id=$1 FOR UPDATE")
        .bind(source)
        .execute(&mut *tx)
        .await?;
    let done = sqlx::query(
        "DELETE FROM playbook_authoring_runs r WHERE r.source_id=$1 AND r.status<>'running'
            AND NOT r.active
            AND r.id NOT IN (SELECT id FROM playbook_authoring_runs
                WHERE source_id=$1 AND status<>'running' ORDER BY id DESC LIMIT $2)
            AND NOT EXISTS (SELECT 1 FROM playbook_authoring_runs a
                WHERE a.source_id=$1 AND a.active AND a.previous_digest=r.artifact_digest)",
    )
    .bind(source)
    .bind(keep)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(done.rows_affected())
}

/// Records that the engine runs the active run `id`'s artifact on its binding.
pub async fn deployed(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("UPDATE playbook_authoring_runs SET deployed=TRUE WHERE id=$1 AND active")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
