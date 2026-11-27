//! A run from request to effect (spec 0013 §4.2-§4.5): the internal task is
//! submitted with the workspace, its outcome is checked by the server itself,
//! and a save that passes is installed, recorded and activated. A rollback
//! puts the previous playbook back. All of it can be repeated after a
//! restart: [`tick`] finds what is unfinished.
use crate::{
    Ended, Error, Finished, Result, Run, Server, SourceKind, SourceRecord, facts_digest, finish,
    in_flight, now_ms, record_digest, request, requested, running, running_for_task, runs,
    source_digest, store, stranded, task_for,
};
mod saving;
use aq_internal_work::{Outcome, Reason, store as internal};
use aq_playbooks_model::Artifact;
use aq_playbooks_simulation::cases::{Outcome as Authored, quote};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// The refusal of a save whose facts changed while the agent worked.
pub const FACTS_CHANGED: &str = "Settings changed while the playbook was being written, so it was not put into effect. Authoring has started again with the new settings.";
const DOCUMENT_CHANGED: &str =
    "The document changed while the playbook was being written. Authoring has started again.";
/// What the owner reads beside an `unable` that blames a host file.
const UNREADABLE: &str = "The agent's result could not be read.";
const HOST_FAULT: &str = "This is a fault in what the server gave the agent, not in your document.";

fn not_found(source: &str) -> Error {
    Error::NotFound(format!("There is no playbook source {source}"))
}

async fn fail(pool: &PgPool, run: i64, status: Ended, owner: &str) -> Result<bool> {
    let finished = Finished {
        status,
        refusal: Some(owner.to_owned()),
        ..Finished::default()
    };
    finish(pool, run, &finished).await
}

/// Makes sure the source's grant exists, records a run for the source as it
/// is now, and submits its internal task. `None` when nothing the agent would
/// see changed since the active playbook was authored.
pub async fn author<S: Server>(s: &S, source: &str) -> Result<Option<i64>> {
    let pool: &PgPool = s.as_ref();
    let record = (store::get(pool, source).await?).ok_or_else(|| not_found(source))?;
    s.ensure_grant(source, &record.capabilities).await?;
    let Some(run) = requested(s, source).await? else {
        return Ok(None);
    };
    submit_run(s, run).await?;
    Ok(Some(run))
}

/// What a run is checked against: the source as the server has it now.
struct Now {
    record: SourceRecord,
    markdown: String,
    facts: std::collections::BTreeMap<String, Value>,
}

impl Now {
    async fn read<S: Server>(s: &S, source: &str) -> Result<Self> {
        let pool: &PgPool = s.as_ref();
        let record = (store::get(pool, source).await?).ok_or_else(|| not_found(source))?;
        let (markdown, _) = s.markdown(&record).await?;
        let facts = s.facts(&record).await?;
        Ok(Self {
            record,
            markdown,
            facts,
        })
    }

    fn document_is(&self, run: &Run) -> bool {
        source_digest(&self.markdown) == run.source_digest
            && record_digest(&self.record) == run.record_digest
    }

    fn facts_are(&self, run: &Run) -> bool {
        facts_digest(&self.facts) == run.facts_digest
    }
}

/// The previous playbook as the next run's agent is shown it.
async fn previous(pool: &PgPool, source: &str) -> Result<Option<request::PreviousPlaybook>> {
    type Row = (
        sqlx::types::Json<Value>,
        Option<String>,
        Option<sqlx::types::Json<Vec<Value>>>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT artifact,markdown,case_files FROM playbook_authoring_runs
         WHERE source_id=$1 AND status='saved' AND active",
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let Some((artifact, markdown, cases)) = row else {
        return Ok(None);
    };
    // An artifact this engine no longer reads is shown as no previous one.
    Ok(
        (serde_json::from_value(artifact.0).ok()).map(|artifact| request::PreviousPlaybook {
            artifact,
            markdown,
            cases: cases.map(|c| c.0).unwrap_or_default(),
        }),
    )
}

/// Builds the workspace of run `id` and submits its internal task. A run
/// recorded for a document or facts that have changed since is replaced by a
/// run for what they are now.
pub async fn submit_run<S: Server>(s: &S, id: i64) -> Result<()> {
    let pool: &PgPool = s.as_ref();
    let mut id = id;
    for _ in 0..3 {
        let Some(run) = running(pool, id).await? else {
            return Ok(());
        };
        let now = Now::read(s, &run.source_id).await?;
        if !now.document_is(&run) || !now.facts_are(&run) {
            match requested(s, &run.source_id).await? {
                Some(newer) if newer != id => id = newer,
                _ => return Ok(()),
            }
            continue;
        }
        return submit_workspace(s, &run, now).await;
    }
    Ok(())
}

async fn submit_workspace<S: Server>(s: &S, run: &Run, now: Now) -> Result<()> {
    let pool: &PgPool = s.as_ref();
    let previous = previous(pool, &run.source_id).await?;
    let catalog = s.catalog(&now.record)?;
    let files = request::workspace_files(
        &now.record,
        request::WorkspaceSource {
            markdown: &now.markdown,
            facts: &now.facts,
            facts_version: &run.facts_digest,
        },
        &catalog,
        previous.as_ref(),
    );
    let files = match files {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(source = %run.source_id, error = %e, "authoring workspace refused");
            let owner = "The server could not prepare the agent's workspace for this source.";
            fail(pool, run.id, Ended::Failed, owner).await?;
            return Ok(());
        }
    };
    let Some(task) = task_for(pool, run.id).await? else {
        return Ok(());
    };
    let request = request::request(&now.record, files, &s.config());
    match s.submit(task, &request).await {
        // Not a passing fault: the request is refused as it is, or there
        // is no internal work here to take it.
        Err(Error::Invalid(why) | Error::Conflict(why)) => {
            tracing::warn!(source = %run.source_id, %why, "authoring request refused");
            let owner = format!("The agent's work could not be submitted: {why}.");
            fail(pool, run.id, Ended::Failed, &owner).await.map(drop)
        }
        other => other,
    }
}

fn failure_words(reason: Reason) -> &'static str {
    match reason {
        Reason::NoCandidate => {
            "No model that can work with tools is set up for writing playbooks. Choose one in Settings, then author again."
        }
        Reason::DeadlineExceeded | Reason::BudgetExceeded => {
            "The agent ran out of time before it finished. Author again, or make the document shorter."
        }
        Reason::InvalidOutput => "The agent ended without saving, asking or giving up.",
        _ => "The agent could not finish because the model service failed. Author again.",
    }
}

/// Records what the internal task of a run came to. A run that already
/// ended, or was superseded, is left as it is.
pub async fn completed<S: Server>(s: &S, internal_task: Uuid, outcome: &Outcome) -> Result<()> {
    let pool: &PgPool = s.as_ref();
    let Some(run) = running_for_task(pool, internal_task).await? else {
        return Ok(());
    };
    let answer = match outcome {
        Outcome::Failed { reason, detail, .. } => {
            tracing::warn!(source = %run.source_id, ?reason, %detail, "authoring run failed");
            fail(pool, run.id, Ended::Failed, failure_words(*reason)).await?;
            return prune(s, &run.source_id).await;
        }
        Outcome::Answered { answer, .. } => answer,
    };
    let authored = serde_json::from_value::<Authored>(answer.clone())
        .map_err(|e| e.to_string())
        .and_then(|authored| authored.check().map(|()| authored));
    let authored = match authored {
        Ok(authored) => authored,
        Err(why) => {
            tracing::warn!(source = %run.source_id, %why, "authoring outcome refused");
            fail(pool, run.id, Ended::Failed, UNREADABLE).await?;
            return prune(s, &run.source_id).await;
        }
    };
    let now = Now::read(s, &run.source_id).await?;
    if !now.document_is(&run) {
        fail(pool, run.id, Ended::Failed, DOCUMENT_CHANGED).await?;
        return author(s, &run.source_id).await.map(drop);
    }
    record_outcome(s, &run, now, authored).await
}

async fn record_outcome<S: Server>(s: &S, run: &Run, now: Now, authored: Authored) -> Result<()> {
    let finished = match authored {
        Authored::Question {
            question,
            sentences,
        } => {
            // The owner is shown the server's text of each line, never the agent's.
            let quoted: std::result::Result<Vec<_>, String> = (sentences.iter())
                .map(|s| quote(&now.markdown, s.line, s.end_line))
                .collect();
            match quoted {
                Ok(sentences) => Finished {
                    status: Ended::Question,
                    question: Some(json!({"question": question, "sentences": sentences})),
                    ..Finished::default()
                },
                Err(why) => {
                    tracing::warn!(source = %run.source_id, %why, "question quotes no sentence");
                    refusal(Ended::Failed, UNREADABLE)
                }
            }
        }
        Authored::Unable {
            reason,
            line: Some(line),
            end_line,
            ..
        } => match quote(&now.markdown, line, end_line) {
            Ok(sentence) => Finished {
                status: Ended::Unable,
                unable: Some(json!({"reason": reason, "line": line, "sentence": sentence.text})),
                ..Finished::default()
            },
            Err(why) => {
                tracing::warn!(source = %run.source_id, %why, "unable quotes no sentence");
                refusal(Ended::Failed, UNREADABLE)
            }
        },
        Authored::Unable { reason, file, .. } => {
            tracing::warn!(source = %run.source_id, ?file, %reason, "agent blames a host file");
            Finished {
                status: Ended::Unable,
                unable: Some(json!({"reason": reason, "sentence": HOST_FAULT, "host_fault": true})),
                ..Finished::default()
            }
        }
        Authored::Saved {
            draft,
            cases,
            artifact,
            ..
        } => {
            return saving::saved(
                s,
                run,
                now,
                saving::Claim {
                    draft,
                    cases,
                    artifact,
                },
            )
            .await;
        }
    };
    finish(s.as_ref(), run.id, &finished).await?;
    prune(s, &run.source_id).await
}

fn refusal(status: Ended, owner: &str) -> Finished {
    Finished {
        status,
        refusal: Some(owner.to_owned()),
        ..Finished::default()
    }
}

async fn prune<S: Server>(s: &S, source: &str) -> Result<()> {
    runs::prune(s.as_ref(), source, s.config().keep_finished_runs).await?;
    Ok(())
}

/// The saved run a rollback of `source` would make active again: the newest
/// one holding the artifact the active run replaced.
pub(crate) async fn rollback_target(
    db: impl sqlx::PgExecutor<'_>,
    source: &str,
    lock: bool,
) -> Result<Option<(i64, Value, Option<i64>)>> {
    type Row = (i64, sqlx::types::Json<Value>, Option<i64>, Option<i64>);
    let lock = if lock { "FOR UPDATE OF p" } else { "" };
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT p.id,p.artifact,p.activated_at_ms,p.finished_at_ms
         FROM playbook_authoring_runs a JOIN playbook_authoring_runs p
           ON p.source_id=a.source_id AND p.artifact_digest=a.previous_digest
          AND p.status='saved' AND NOT p.active
         WHERE a.source_id=$1 AND a.active ORDER BY p.id DESC LIMIT 1 {lock}"
    ))
    .bind(source)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(id, artifact, activated, finished)| (id, artifact.0, activated.or(finished))))
}

/// Puts the previous playbook of `source` back in effect. A run in flight is
/// superseded: the owner chose the previous playbook, and a later save of
/// the document starts a new run.
///
/// The target is chosen, activated in the engine and recorded under the
/// source's lock and its own row lock, so a save that finishes meanwhile
/// waits and cannot have it deleted; nothing is recorded unless exactly the
/// chosen run became active.
pub async fn rollback<S: Server>(s: &S, source: &str) -> Result<()> {
    let pool: &PgPool = s.as_ref();
    let record = (store::get(pool, source).await?).ok_or_else(|| not_found(source))?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT 1 FROM playbook_sources WHERE id=$1 FOR UPDATE")
        .bind(source)
        .execute(&mut *tx)
        .await?;
    let Some((target, artifact, _)) = rollback_target(&mut *tx, source, true).await? else {
        return Err(Error::Conflict(
            "There is no earlier playbook to go back to.".into(),
        ));
    };
    let artifact: Artifact = serde_json::from_value(artifact).map_err(|_| Error::Corrupt)?;
    s.ensure_grant(source, &record.capabilities).await?;
    s.install(&artifact).await?;
    s.activate(source, &record.binding, &artifact.digest)
        .await?;
    let now = now_ms();
    sqlx::query("UPDATE playbook_authoring_runs SET active=FALSE WHERE source_id=$1 AND active")
        .bind(source)
        .execute(&mut *tx)
        .await?;
    let activated = sqlx::query(
        "UPDATE playbook_authoring_runs SET active=TRUE,deployed=TRUE,activated_at_ms=$2
         WHERE id=$1 AND status='saved'",
    )
    .bind(target)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if activated.rows_affected() != 1 {
        // Dropping the transaction leaves what was active as it was.
        return Err(Error::Conflict(
            "The earlier playbook is no longer kept; nothing was changed.".into(),
        ));
    }
    sqlx::query(
        "UPDATE playbook_authoring_runs SET status='superseded',finished_at_ms=$2
         WHERE source_id=$1 AND status='running'",
    )
    .bind(source)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes a registered source: its grant is revoked, so its playbook can do
/// nothing more, its runs stop and nothing of it stays active. The run
/// history is kept. The built-in source is refused.
pub async fn remove<S: Server>(s: &S, source: &str) -> Result<()> {
    let pool: &PgPool = s.as_ref();
    let record = (store::get(pool, source).await?).ok_or_else(|| not_found(source))?;
    if record.kind != SourceKind::Registered {
        return Err(Error::Conflict(format!(
            "{source} is a built-in source and cannot be deleted"
        )));
    }
    s.revoke_grant(source).await?;
    store::remove(pool, source).await
}

/// Installs and activates every active run's artifact again, as the server
/// does at start: an engine that lost one, or a stop between recording a
/// save and activating it, is repaired. Returns how many are active.
pub async fn reinstall_active<S: Server>(s: &S) -> Result<usize> {
    let pool: &PgPool = s.as_ref();
    type Row = (String, String, sqlx::types::Json<Value>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT r.source_id,s.binding,r.artifact FROM playbook_authoring_runs r
         JOIN playbook_sources s ON s.id=r.source_id
         WHERE r.status='saved' AND r.active AND s.deleted_at_ms IS NULL ORDER BY r.id",
    )
    .fetch_all(pool)
    .await?;
    // One source that cannot be put back does not keep the others out.
    let mut first = Ok(rows.len());
    for (source, binding, artifact) in &rows {
        if let Err(e) = reinstall(s, source, binding, &artifact.0).await {
            tracing::warn!(%source, error = %e, "active playbook not reinstalled");
            first = first.and(Err(e));
        }
    }
    first
}

async fn reinstall<S: Server>(s: &S, source: &str, binding: &str, artifact: &Value) -> Result<()> {
    let artifact: Artifact =
        serde_json::from_value(artifact.clone()).map_err(|_| Error::Corrupt)?;
    let record = (store::get(s.as_ref(), source).await?).ok_or_else(|| not_found(source))?;
    s.ensure_grant(source, &record.capabilities).await?;
    s.install(&artifact).await?;
    s.activate(source, binding, &artifact.digest).await
}

/// One pass over the unfinished runs: a run without its internal task gets
/// it submitted, and a run whose task has ended has the result recorded.
/// Returns how many runs it looked at.
pub async fn tick<S: Server>(s: &S) -> Result<usize> {
    let pool: &PgPool = s.as_ref();
    let mut runs = stranded(pool, now_ms()).await?;
    runs.extend(in_flight(pool).await?);
    for run in &runs {
        let task = match run.internal_task_id {
            Some(task) => internal::get(pool, task).await.map_err(internal_error)?,
            None => None,
        };
        let stepped = advance(s, run, task).await;
        if let Err(e) = stepped {
            tracing::warn!(source = %run.source_id, run = run.id, error = %e, "authoring run not advanced");
        }
    }
    deploy_pending(s).await?;
    Ok(runs.len())
}

async fn advance<S: Server>(s: &S, run: &Run, task: Option<internal::Row>) -> Result<()> {
    match (run.internal_task_id, task) {
        (Some(id), Some(row)) => match row.outcome() {
            Some(outcome) => completed(s, id, &outcome).await,
            None => Ok(()),
        },
        _ => submit_run(s, run.id).await,
    }
}

/// Activates every active run the engine has not acknowledged yet: a save
/// whose activation failed is tried again on every pass.
async fn deploy_pending<S: Server>(s: &S) -> Result<()> {
    let pool: &PgPool = s.as_ref();
    let runs: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id,source_id FROM playbook_authoring_runs
         WHERE status='saved' AND active AND NOT deployed ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    for (run, source) in &runs {
        if let Err(e) = deploy(s, *run).await {
            tracing::warn!(%source, error = %e, "active playbook not activated yet");
        }
    }
    Ok(())
}

/// Activates run `id`'s artifact in the engine and marks it deployed, if it
/// is still its source's active run. All of it happens under the source's
/// lock, as a rollback does, so the engine is never left on an artifact the
/// tables no longer call active. Returns whether it deployed anything.
pub async fn deploy<S: Server>(s: &S, id: i64) -> Result<bool> {
    let pool: &PgPool = s.as_ref();
    let mut tx = pool.begin().await?;
    let source: Option<String> = sqlx::query_scalar(
        "SELECT s.id FROM playbook_sources s JOIN playbook_authoring_runs r ON r.source_id=s.id
         WHERE r.id=$1 FOR UPDATE OF s",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(source) = source else {
        return Ok(false);
    };
    // Read again under the lock: a rollback may have made another run active.
    type Row = (String, sqlx::types::Json<Value>);
    let row: Option<Row> = sqlx::query_as(
        "SELECT s.binding,r.artifact FROM playbook_authoring_runs r
         JOIN playbook_sources s ON s.id=r.source_id
         WHERE r.id=$1 AND r.status='saved' AND r.active AND NOT r.deployed
           AND s.deleted_at_ms IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((binding, artifact)) = row else {
        return Ok(false);
    };
    reinstall(s, &source, &binding, &artifact.0).await?;
    sqlx::query("UPDATE playbook_authoring_runs SET deployed=TRUE WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// [`author`], unless the newest run of the source was already for the
/// document, facts and record as they are now, however it ended. For the
/// server's own reconciliation (the engine opened, a slow periodic pass): a
/// save whose authoring was missed is authored, and a question or a refusal
/// is not asked again until something changes.
pub async fn author_missed<S: Server>(s: &S, source: &str) -> Result<Option<i64>> {
    let pool: &PgPool = s.as_ref();
    let now = Now::read(s, source).await?;
    let newest: Option<(String, String, String)> = sqlx::query_as(
        "SELECT source_digest,facts_digest,record_digest FROM playbook_authoring_runs
         WHERE source_id=$1 ORDER BY id DESC LIMIT 1",
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let same = newest.is_some_and(|(document, facts, record)| {
        document == source_digest(&now.markdown)
            && facts == facts_digest(&now.facts)
            && record == record_digest(&now.record)
    });
    match same {
        true => Ok(None),
        false => author(s, source).await,
    }
}

pub fn internal_error(e: aq_internal_work::Error) -> Error {
    match e {
        aq_internal_work::Error::Invalid(m) => Error::Invalid(m),
        aq_internal_work::Error::Conflict(m) => Error::Conflict(m),
        aq_internal_work::Error::NotFound(m) => Error::NotFound(m),
        aq_internal_work::Error::Database(e) => Error::Database(e),
        aq_internal_work::Error::Corrupt => Error::Corrupt,
    }
}

/// Runs [`tick`] every `poll_ms` until `stop`, after putting the active
/// playbooks back in the engine ([`reinstall_active`], tried until it
/// succeeds once: the engine may open after the server does).
pub async fn serve<S: Server>(s: S, mut stop: tokio::sync::watch::Receiver<bool>) {
    let poll = Duration::from_millis(s.config().poll_ms.max(100));
    let mut reinstalled = false;
    for pass in 0_u64.. {
        // Every 15th pass until it has succeeded once.
        if !reinstalled && pass % 15 == 0 {
            reinstalled = reinstall_active(&s).await.is_ok();
        }
        if let Err(e) = tick(&s).await {
            tracing::warn!(error = %e, "authoring runs not advanced");
        }
        // The delivery document is Settings' too: a missed save is caught up
        // on a slower pass (spec 0016 §8).
        if pass % 30 == 0
            && let Some(record) = s.delivery()
        {
            crate::catch_up(&s, &record).await;
        }
        tokio::select! {
            _ = stop.wait_for(|stop| *stop) => return,
            () = tokio::time::sleep(poll) => {}
        }
    }
}
