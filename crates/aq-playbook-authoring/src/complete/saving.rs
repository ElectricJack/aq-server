use super::{FACTS_CHANGED, Now, author, deploy, fail, prune};
use crate::{ActiveRun, Config, Ended, Error, Finished, Result, Run, Server, active, finish, save};
use aq_playbooks_model::Artifact;
use aq_playbooks_simulation::cases::Case;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;

pub(super) struct Claim {
    pub(super) draft: Value,
    pub(super) cases: Vec<Case>,
    pub(super) artifact: String,
}

/// How many of the source's newest runs, one after another, were refused
/// because the facts changed.
async fn facts_restarts(pool: &PgPool, source: &str) -> Result<usize> {
    let refusals: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT CASE WHEN status='refused' THEN refusal END FROM playbook_authoring_runs
         WHERE source_id=$1 AND status<>'superseded' ORDER BY id DESC LIMIT 64",
    )
    .bind(source)
    .fetch_all(pool)
    .await?;
    let changed = |r: &&Option<String>| r.as_deref() == Some(FACTS_CHANGED);
    Ok(refusals.iter().take_while(changed).count())
}

/// A save: checked by the server, installed, recorded as active, activated.
pub(super) async fn saved<S: Server>(s: &S, run: &Run, now: Now, claim: Claim) -> Result<()> {
    let pool: &PgPool = s.as_ref();
    let (source, config) = (run.source_id.as_str(), s.config());
    if !now.facts_are(run) {
        return restart_for_facts(s, run, config.facts_restarts as usize).await;
    }
    let cannot_deploy =
        "The playbook cannot be put into effect because playbooks are not running on this server.";
    if let Err(e) = s.ensure_grant(source, &now.record.capabilities).await {
        tracing::warn!(source, error = %e, "no grant for an authored playbook");
        fail(pool, run.id, Ended::Refused, cannot_deploy).await?;
        return Ok(());
    }
    let in_effect = active(pool, source).await?;
    let inputs = inputs_for(
        s,
        run,
        now,
        in_effect.as_ref(),
        previous_cases(pool, in_effect.as_ref()).await?,
    )?;
    let markdown = inputs.markdown.clone();
    let claimed = claim.artifact;
    let checked = check(config, claim.draft, claim.cases, inputs).await;
    let checked = match checked {
        Ok(checked) => checked,
        Err(refused) if !refused.regressed.is_empty() => {
            tracing::warn!(source, detail = %refused.detail, "authored save regressed a previous case");
            let question = json!({"question": refused.owner, "sentences": []});
            let asked = Finished {
                status: Ended::Question,
                question: Some(question),
                ..Finished::default()
            };
            finish(pool, run.id, &asked).await?;
            return prune(s, source).await;
        }
        Err(refused) => {
            tracing::warn!(source, detail = %refused.detail, "authored save refused");
            fail(pool, run.id, Ended::Refused, &refused.owner).await?;
            return prune(s, source).await;
        }
    };
    if checked.artifact.digest != claimed {
        // The agent's digest is a claim; the server's artifact is what counts.
        tracing::warn!(
            source,
            "the agent's artifact digest differs from the server's"
        );
    }
    if let Err(e) = s.install(&checked.artifact).await {
        tracing::warn!(source, error = %e, "authored artifact not installed");
        let owner = "The playbook passed its checks but could not be installed.";
        fail(pool, run.id, Ended::Refused, owner).await?;
        return prune(s, source).await;
    }
    let finished = SavedRun { checked, markdown }.finished(in_effect.as_ref())?;
    // Review focus 2: a superseded run is not recorded as saved, so its
    // artifact is never activated.
    if !finish(pool, run.id, &finished).await? {
        return Ok(());
    }
    // The run is active but not yet deployed: when this fails, every pass of
    // `tick` tries again until the engine has the artifact on the binding.
    if let Err(e) = deploy(s, run.id).await {
        tracing::error!(source, error = %e, "authored artifact recorded active but not activated yet");
    }
    prune(s, source).await
}

async fn restart_for_facts<S: Server>(s: &S, run: &Run, limit: usize) -> Result<()> {
    let pool = s.as_ref();
    let source = &run.source_id;
    // Review focus 3: nothing changes, and authoring starts again.
    fail(pool, run.id, Ended::Refused, FACTS_CHANGED).await?;
    if facts_restarts(pool, source).await? <= limit {
        author(s, source).await?;
    }
    Ok(())
}

fn inputs_for<S: Server>(
    s: &S,
    run: &Run,
    now: Now,
    in_effect: Option<&ActiveRun>,
    previous_cases: Vec<Case>,
) -> Result<save::Inputs> {
    let previous: Option<Artifact> =
        in_effect.and_then(|a| serde_json::from_value(a.artifact.clone()).ok());
    let catalog = s.catalog(&now.record)?;
    Ok(save::Inputs {
        grant_capabilities: now.record.capabilities.clone(),
        record: now.record,
        markdown: now.markdown,
        catalog,
        facts: now.facts,
        facts_version: run.facts_digest.clone(),
        previous,
        previous_cases,
    })
}

/// The case files the active run was sealed with. Files this engine no
/// longer reads are none: they cannot be run again.
async fn previous_cases(pool: &PgPool, in_effect: Option<&ActiveRun>) -> Result<Vec<Case>> {
    let Some(active) = in_effect else {
        return Ok(vec![]);
    };
    let files: Option<Option<sqlx::types::Json<Value>>> =
        sqlx::query_scalar("SELECT case_files FROM playbook_authoring_runs WHERE id=$1")
            .bind(active.id)
            .fetch_optional(pool)
            .await?;
    let Some(sqlx::types::Json(files)) = files.flatten() else {
        return Ok(vec![]);
    };
    Ok(serde_json::from_value(files).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "the active run's case files are unreadable");
        vec![]
    }))
}

async fn check(
    config: Config,
    draft: Value,
    cases: Vec<Case>,
    inputs: save::Inputs,
) -> std::result::Result<save::Saved, save::Refusal> {
    let deadline = Duration::from_millis(config.check_deadline_ms);
    let check =
        tokio::task::spawn_blocking(move || save::check_saved(&draft, &cases, &inputs, &config));
    match tokio::time::timeout(deadline, check).await {
        Ok(Ok(checked)) => checked,
        Ok(Err(_)) | Err(_) => Err(save::Refusal::new(
            "The server's check of the saved playbook did not finish in time.",
            "check deadline or panic",
        )),
    }
}

struct SavedRun {
    checked: save::Saved,
    markdown: String,
}

impl SavedRun {
    fn finished(self, in_effect: Option<&ActiveRun>) -> Result<Finished> {
        let Self { checked, markdown } = self;
        let case_files = serde_json::to_value(&checked.case_files).map_err(|_| Error::Corrupt)?;
        let digest = checked.artifact.digest.clone();
        // Saving the same playbook again keeps what a rollback would bring back.
        let previous_digest = match in_effect {
            Some(a) if a.artifact_digest == digest => a.previous_digest.clone(),
            Some(a) => Some(a.artifact_digest.clone()),
            None => None,
        };
        Ok(Finished {
            status: Ended::Saved,
            activate: true,
            artifact_digest: Some(digest),
            artifact: Some(serde_json::to_value(&checked.artifact).map_err(|_| Error::Corrupt)?),
            previous_digest,
            comparison: checked.comparison,
            cases: Some(checked.cases),
            markdown: Some(markdown),
            case_files: Some(case_files),
            ..Finished::default()
        })
    }
}
