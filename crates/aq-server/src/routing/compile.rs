//! Compiling the routing policy when it is saved (spec 0011 §6, §7). The compile is internal
//! work; `aq_routing::compile` builds the request and checks the answer, and a policy that checks
//! takes effect at once. One that does not changes nothing. Each compile is a `routing_policies`
//! row; a waiter records its result, and [`serve`] records any result a waiter missed (a restart,
//! or a retry from the internal runs page).
use crate::{
    AppState,
    error::{AppError, db},
    profile_catalog::settings::error as settings_error,
};
use aq_internal_work::{Affected, AnswerShape, Outcome, Purpose, Request};
use aq_profile_settings::structure;
use aq_routing::{
    Class,
    classify::Options,
    compile as source,
    policy::{Policy, validate},
};
use serde_json::{Value, json};
use sqlx::{PgPool, types::Json};
use std::{collections::BTreeSet, time::Duration};
use uuid::Uuid;

/// How often [`serve`] looks for finished compiles nobody recorded.
const RESUME_EVERY: Duration = Duration::from_secs(5);
const KIND: &str = "routing_policy";

/// What Settings offers: each class with an assigned profile in the active catalog, and the
/// (provider, harness) pairs of that profile's candidates whose harness is qualified.
pub async fn options(state: &AppState) -> Result<Options, AppError> {
    let (_, digest) = crate::project_bindings::active_catalog(state).await?;
    let Some(digest) = digest else {
        return Ok(Options::default());
    };
    let catalog = crate::project_bindings::catalog(state, &digest).await?;
    let pool = &state.store.pool;
    let (document, _, _) = structure::document(pool).await.map_err(settings_error)?;
    let registries = state.config.intelligence.iter();
    let qualified: BTreeSet<&str> = (registries.flat_map(|r| &r.config.qualified_harnesses))
        .map(|h| h.harness.as_str())
        .collect();
    let profiles = &catalog.artifact().catalog.intelligence;
    let mut options = Options::default();
    for a in &document.assignments {
        let class = format!("{}-{}", a.parent_id, a.effort).parse::<Class>();
        let profile =
            (profiles.iter()).find(|p| p.id == a.profile_id && p.automatic_routing != Some(true));
        if let (Ok(class), Some(profile)) = (class, profile) {
            let candidates =
                (profile.candidates.iter()).map(|c| (c.provider.as_str(), c.harnesses.as_slice()));
            options
                .classes
                .insert(class, source::offers(candidates, &qualified));
        }
    }
    Ok(options)
}

fn now_ms(state: &AppState) -> i64 {
    (state.clock)().timestamp_millis()
}

/// Submits the compile of the routing Markdown saved at `structure_revision` and records it as
/// compiling. `None` when a newer save has replaced that revision (it compiles itself), or when
/// nothing could be submitted, which is recorded as a failed compile.
pub async fn requested(
    state: &AppState,
    structure_revision: u64,
) -> Result<Option<Uuid>, AppError> {
    let pool = &state.store.pool;
    let (document, revision, _) = structure::document(pool).await.map_err(settings_error)?;
    if revision != structure_revision {
        return Ok(None);
    }
    let options = options(state).await?;
    let source_digest = source::source_digest(&document.routing_markdown);
    let insert = "INSERT INTO routing_policies(structure_revision,source_digest,internal_task_id,
        status,error,created_at_ms,finished_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)";
    let record = |task: Option<Uuid>, error: Option<&'static str>| {
        let status = if error.is_some() {
            "failed"
        } else {
            "compiling"
        };
        let now = now_ms(state);
        sqlx::query(insert)
            .bind(revision as i64)
            .bind(&source_digest)
            .bind(task)
            .bind(status)
            .bind(error)
            .bind(now)
            .bind(error.map(|_| now))
            .execute(pool)
    };
    if options.classes.is_empty() {
        let error = "No level has an assigned profile in Settings, so there is nothing to route to";
        record(None, Some(error)).await.map_err(db)?;
        return Ok(None);
    }
    let parents: Vec<(String, String)> = (document.parents.iter())
        .map(|p| (p.id.clone(), p.description.clone()))
        .collect();
    let request = Request {
        purpose: Purpose::PlaybookCompilation,
        instructions: source::COMPILER_INSTRUCTIONS.into(),
        input: source::input(&document.routing_markdown, &options, &parents),
        answer: AnswerShape::Json {
            schema: source::answer_schema(&options),
        },
        blocking: false,
        affected: Affected {
            kind: KIND.into(),
            id: revision.to_string(),
            project_id: None,
        },
        deadline_ms: 600_000,
        max_output_bytes: 262_144,
        escalate: true,
        profile_override: None,
    };
    let task = match state.internal_work.submit(request).await {
        Ok(task) => task,
        Err(error) => {
            tracing::warn!(?error, "routing policy compile not submitted");
            let error = "Internal work did not take the compile, so this version was not compiled";
            record(None, Some(error)).await.map_err(db)?;
            return Ok(None);
        }
    };
    record(Some(task), None).await.map_err(db)?;
    tokio::spawn(watch(state.clone(), task));
    Ok(Some(task))
}

async fn watch(state: AppState, task: Uuid) {
    let recorded = match state.internal_work.wait(task).await {
        Ok(outcome) => completed(&state, task, &outcome).await,
        Err(error) => Err(error),
    };
    if let Err(error) = recorded {
        tracing::warn!(?error, %task, "routing policy compile not recorded; resumed later");
    }
}

/// Records the result of the compile `internal_task`. A policy that checks against what
/// Settings offers now becomes the active policy, unless a later revision has been submitted for
/// compiling, when it is stored superseded. Anything else is stored failed, and the active
/// policy is untouched. A result already recorded is left as it is.
pub async fn completed(
    state: &AppState,
    internal_task: Uuid,
    outcome: &Outcome,
) -> Result<(), AppError> {
    let options = options(state).await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    // One result at a time decides what routes.
    sqlx::query("SELECT 1 FROM routing_state WHERE singleton FOR UPDATE")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let row = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT id,structure_revision,status FROM routing_policies WHERE internal_task_id=$1",
    )
    .bind(internal_task)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    let (id, revision) = match row {
        Some((_, _, status)) if status != "compiling" => return Ok(()),
        Some((id, revision, _)) => (id, revision),
        // A retry of a recorded compile compiles the same source.
        None => match sqlx::query_as::<_, (i64, i64)>(
            "INSERT INTO routing_policies(structure_revision,source_digest,internal_task_id,status,
                created_at_ms) SELECT p.structure_revision,p.source_digest,t.id,'compiling',t.created_at_ms
             FROM internal_tasks t JOIN routing_policies p ON p.internal_task_id=t.retry_of
             WHERE t.id=$1 RETURNING id,structure_revision",
        )
        .bind(internal_task)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        {
            Some(row) => row,
            None => return Ok(()),
        },
    };
    let newer: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM routing_policies WHERE structure_revision>$1)",
    )
    .bind(revision)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let answer = match outcome {
        Outcome::Answered { answer, .. } => Ok(answer),
        Outcome::Failed { reason, detail, .. } => {
            let reason = json!(reason).as_str().unwrap_or_default().replace('_', " ");
            Err(format!("The compile did not finish ({reason}): {detail}"))
        }
    };
    let source::Recorded {
        status,
        valid,
        error,
        problems,
        unresolved,
    } = source::record(answer, &options, newer);
    if status == "active" {
        sqlx::query("UPDATE routing_policies SET status='superseded' WHERE status='active'")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("UPDATE routing_state SET active_policy_digest=$1 WHERE singleton")
            .bind(valid.as_ref().map(|v| &v.1))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
    }
    sqlx::query(
        "UPDATE routing_policies SET status=$2,digest=$3,compiled=$4,summary=$5,error=$6,
            problems=$7,unresolved=$8,finished_at_ms=$9 WHERE id=$1",
    )
    .bind(id)
    .bind(status)
    .bind(valid.as_ref().map(|v| &v.1))
    .bind(valid.as_ref().map(|v| Json(&v.0)))
    .bind(valid.as_ref().map(|v| Json(&v.2)))
    .bind(error)
    .bind(Json(&problems))
    .bind(Json(&unresolved))
    .bind(now_ms(state))
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)
}

/// The policy that routes: the compiled policy, its digest and its structure revision.
pub async fn active(pool: &PgPool) -> Result<Option<(Policy, String, u64)>, AppError> {
    let row = sqlx::query_as::<_, (Json<Policy>, String, i64)>(
        "SELECT compiled,digest,structure_revision FROM routing_policies WHERE status='active'",
    )
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    Ok(row.map(|(policy, digest, revision)| (policy.0, digest, revision as u64)))
}

/// Records every finished compile that is not recorded yet: one whose waiter ended with the
/// server, and a retry of a recorded compile. Returns how many it recorded.
pub async fn resume(state: &AppState) -> Result<usize, AppError> {
    let pool = &state.store.pool;
    let tasks: Vec<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM internal_tasks t LEFT JOIN routing_policies p ON p.internal_task_id=t.id
         WHERE t.affected_kind=$1 AND t.purpose='playbook_compilation'
           AND t.status IN ('answered','failed')
           AND (p.status='compiling' OR (p.id IS NULL AND t.retry_of IS NOT NULL))
         ORDER BY t.created_at_ms",
    )
    .bind(KIND)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let mut recorded = 0;
    for task in tasks {
        let outcome = aq_internal_work::store::get(pool, task).await?;
        if let Some(outcome) = outcome.and_then(|row| row.outcome()) {
            completed(state, task, &outcome).await?;
            recorded += 1;
        }
    }
    Ok(recorded)
}

/// Runs [`resume`] now and every few seconds until `stop`.
pub async fn serve(state: AppState, mut stop: tokio::sync::watch::Receiver<bool>) {
    while !*stop.borrow() {
        if let Err(error) = resume(&state).await {
            tracing::warn!(?error, "routing policy compiles not resumed");
        }
        tokio::select! {
            _ = tokio::time::sleep(RESUME_EVERY) => {}
            changed = stop.changed() => if changed.is_err() { return },
        }
    }
}

/// `GET /api/settings/intelligence/routing`: the switch, the active policy in words with what
/// no longer validates against Settings now, and the latest compile.
pub(crate) async fn view(state: &AppState) -> Result<Value, AppError> {
    let pool = &state.store.pool;
    let enabled = super::enabled(pool).await?;
    let active = sqlx::query_as::<_, (Value, Json<Policy>)>(
        "SELECT jsonb_build_object('digest',digest,'structure_revision',structure_revision,
            'summary',COALESCE(summary,'[]'),'created_at_ms',created_at_ms,
            'took_effect_at_ms',finished_at_ms), compiled
         FROM routing_policies WHERE status='active'",
    )
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    let active = match active {
        Some((mut active, policy)) => {
            active["problems"] = json!(validate(&policy.0, &options(state).await?));
            active
        }
        None => Value::Null,
    };
    let latest: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('status',status,'structure_revision',structure_revision,
            'error',error,'problems',problems,'unresolved',unresolved,
            'internal_task_id',internal_task_id,'created_at_ms',created_at_ms,
            'finished_at_ms',finished_at_ms)
         FROM routing_policies ORDER BY structure_revision DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    Ok(json!({"enabled": enabled, "active": active, "latest": latest}))
}
