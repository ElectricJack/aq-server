use super::*;
use crate::{
    authz::RequireOwner,
    profiled_execution::{results, settlement::actor::ActorPin},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, State},
    http::request::Parts,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

mod publication;

struct HumanOwner(crate::store::User);

impl FromRequestParts<AppState> for HumanOwner {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        if aq_supervisor_control::read::delegated(&parts.extensions).is_some() {
            return Err(AppError::Forbidden(
                "task workspace delivery requires the human owner".into(),
            ));
        }
        RequireOwner::from_request_parts(parts, state)
            .await
            .map(|owner| Self(owner.0))
    }
}

#[derive(Clone, Copy, Deserialize)]
struct Subject {
    project: Uuid,
    task: Uuid,
    report: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    operation_id: Uuid,
    action: Action,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    IntegratedChecks {
        heads: BTreeMap<Uuid, String>,
        verdict: Verdict,
        evidence: String,
    },
    OwnerAcceptance {
        target_project_id: Uuid,
        head: String,
        evidence: String,
    },
    ExternalDelivery {
        target_project_id: Uuid,
        head: String,
        evidence: String,
    },
    Publish {
        target_project_id: Uuid,
        head: String,
    },
    VerifyMerge {
        target_project_id: Uuid,
        head: String,
        pull_request: u64,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Passed,
    Failed,
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{project}/tasks/{task}/results/{report}/workspace-delivery",
            get(read).post(write),
        )
        .layer(DefaultBodyLimit::max(12 * 1024))
}

async fn read(
    State(state): State<AppState>,
    HumanOwner(_): HumanOwner,
    Path(subject): Path<Subject>,
) -> Result<Json<Value>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(crate::error::db)?;
    let plan = plan(&state, &mut tx, subject).await?;
    let body = read_plan(&mut tx, &plan).await?;
    tx.commit().await.map_err(crate::error::db)?;
    Ok(Json(body))
}

async fn write(
    State(state): State<AppState>,
    HumanOwner(actor): HumanOwner,
    Path(subject): Path<Subject>,
    Json(command): Json<Command>,
) -> Result<Json<Value>, AppError> {
    if command.operation_id.is_nil() {
        return Err(AppError::BadRequest(
            "invalid workspace delivery operation ID".into(),
        ));
    }
    let actor = ActorPin::observe(&state, &actor).await?;
    let mut tx = state.store.pool.begin().await.map_err(crate::error::db)?;
    let plan = plan(&state, &mut tx, subject).await?;
    actor.fence(&mut tx).await?;
    let (operation, at) = (command.operation_id, now(&state)?);
    let external = matches!(&command.action, Action::ExternalDelivery { .. });
    match command.action {
        Action::IntegratedChecks {
            heads,
            verdict,
            evidence,
        } => {
            if !plan.complete()
                || heads.len() != plan.sources.len()
                || plan
                    .sources
                    .iter()
                    .any(|(project, source)| heads.get(project) != Some(&source.head))
            {
                return Err(conflict(
                    "integrated checks must name every exact frozen checkout",
                ));
            }
            record_checks(
                &mut tx,
                &plan,
                (operation, actor.id(), at),
                verdict,
                &evidence,
            )
            .await?;
        }
        Action::OwnerAcceptance {
            target_project_id,
            head,
            evidence,
        }
        | Action::ExternalDelivery {
            target_project_id,
            head,
            evidence,
        } => {
            let source = source(&plan, target_project_id, &head)?;
            if source.target_ref.is_some()
                && source.publication_mode.as_deref() == Some("pull_request")
            {
                return Err(conflict(
                    "this target requires a verified merge to its configured branch",
                ));
            }
            let kind = if external {
                "external_delivery"
            } else {
                "owner_acceptance"
            };
            record_target(
                &mut tx,
                &plan,
                source,
                (operation, actor.id(), at),
                kind,
                &evidence,
            )
            .await?;
        }
        Action::Publish {
            target_project_id,
            head,
        } => {
            let source = source(&plan, target_project_id, &head)?;
            let output =
                publication::publish(&state, tx, subject, &plan, source, operation, &actor).await?;
            return Ok(Json(output));
        }
        Action::VerifyMerge {
            target_project_id,
            head,
            pull_request,
        } => {
            let source = source(&plan, target_project_id, &head)?;
            if !(1..=1_000_000_000).contains(&pull_request) {
                return Err(AppError::BadRequest("invalid pull request number".into()));
            }
            let (delivery, target) = publication::configured(&state, source, &mut tx).await?;
            let evidence = delivery
                .verified_external_merge(&target, pull_request, &source.head)
                .await
                .map_err(|_| AppError::UpdatesUnavailable("target forge unavailable".into()))?
                .ok_or_else(|| {
                    conflict("target pull request has no verified merge for the retained revision")
                })?;
            record_target(
                &mut tx,
                &plan,
                source,
                (operation, actor.id(), at),
                "verified_merge",
                &evidence.url,
            )
            .await?;
        }
    }
    let body = read_plan(&mut tx, &plan).await?;
    tx.commit().await.map_err(crate::error::db)?;
    Ok(Json(body))
}

async fn plan(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    subject: Subject,
) -> Result<Plan, AppError> {
    let result = results::load_locked(state, tx, subject.report).await?;
    if result.spec.lifecycle.project_id != subject.project
        || result.spec.task.task_id != subject.task
    {
        return Err(AppError::NotFound("task workspace result not found".into()));
    }
    if !result.is_current_report_head {
        return Err(conflict(
            "workspace delivery needs the current retained result",
        ));
    }
    load(state, tx, &result)
        .await?
        .ok_or_else(|| AppError::NotFound("task has no writable supplemental workspaces".into()))
}

async fn read_plan(db: &mut PgConnection, plan: &Plan) -> Result<Value, AppError> {
    let mut targets = BTreeMap::new();
    for s in plan.sources.values() {
        let accepted = !s.changed || target_accepted(db, plan, s).await?;
        let pr: Option<Value> = sqlx::query_scalar(
            "SELECT jsonb_build_object('published',published_at_ms IS NOT NULL,
                  'pull_request',pull_request,'url',pull_request_url)
             FROM task_workspace_publications
             WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3
              AND repository_id=$4 AND binding_generation=$5 AND project_revision=$6
              AND result_head=$7 AND target_ref IS NOT DISTINCT FROM $8",
        )
        .bind(plan.home)
        .bind(plan.task)
        .bind(s.project_id)
        .bind(s.repository_id)
        .bind(s.binding_generation)
        .bind(s.project_revision)
        .bind(&s.head)
        .bind(&s.target_ref)
        .fetch_optional(&mut *db)
        .await
        .map_err(crate::error::db)?;
        targets.insert(s.project_id, json!({"accepted":accepted,"publication":pr}));
    }
    Ok(
        json!({"plan":plan,"targets":targets,"integrated_checks_passed":plan.complete() && check_passed(db,plan).await?}),
    )
}

fn source(plan: &Plan, project: Uuid, head: &str) -> Result<&Source, AppError> {
    plan.sources
        .get(&project)
        .filter(|s| s.changed && s.head == head)
        .ok_or_else(|| conflict("target is not a changed writable workspace at the named revision"))
}

fn evidence_valid(text: &str) -> Result<(), AppError> {
    if text.trim().is_empty() || text.len() > 2048 || text.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "invalid workspace delivery evidence reference".into(),
        ));
    }
    Ok(())
}

type Stamp = (Uuid, Uuid, i64);

async fn record_checks(
    db: &mut PgConnection,
    plan: &Plan,
    stamp: Stamp,
    verdict: Verdict,
    evidence: &str,
) -> Result<(), AppError> {
    evidence_valid(evidence)?;
    let (operation, actor, at) = stamp;
    let digest = plan.digest()?;
    sqlx::query(
        "INSERT INTO task_workspace_integrated_checks(operation_id,home_project_id,task_id,inputs_digest,verdict,evidence,actor_id,recorded_at_ms)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(operation_id) DO NOTHING",
    ).bind(operation).bind(plan.home).bind(plan.task).bind(&digest).bind(verdict.name()).bind(evidence).bind(actor).bind(at)
     .execute(&mut *db).await.map_err(crate::error::db)?;
    let exact:bool=sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM task_workspace_integrated_checks WHERE operation_id=$1 AND home_project_id=$2
           AND task_id=$3 AND inputs_digest=$4 AND verdict=$5 AND evidence=$6 AND actor_id=$7)",
    ).bind(operation).bind(plan.home).bind(plan.task).bind(digest).bind(verdict.name()).bind(evidence).bind(actor)
     .fetch_one(db).await.map_err(crate::error::db)?;
    if !exact {
        return Err(conflict(
            "workspace integrated check operation ID already used",
        ));
    }
    Ok(())
}

async fn record_target(
    db: &mut PgConnection,
    plan: &Plan,
    source: &Source,
    stamp: Stamp,
    kind: &str,
    evidence: &str,
) -> Result<(), AppError> {
    evidence_valid(evidence)?;
    let (operation, actor, at) = stamp;
    sqlx::query(
        "INSERT INTO task_workspace_target_acceptance(operation_id,home_project_id,task_id,target_project_id,
            project_revision,repository_id,binding_generation,result_head,target_ref,kind,evidence,actor_id,recorded_at_ms)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(operation_id) DO NOTHING",
    ).bind(operation).bind(plan.home).bind(plan.task).bind(source.project_id).bind(source.project_revision)
     .bind(source.repository_id).bind(source.binding_generation).bind(&source.head).bind(&source.target_ref)
     .bind(kind).bind(evidence).bind(actor).bind(at).execute(&mut *db).await.map_err(crate::error::db)?;
    let exact:bool=sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM task_workspace_target_acceptance WHERE operation_id=$1 AND home_project_id=$2
          AND task_id=$3 AND target_project_id=$4 AND project_revision=$5 AND repository_id=$6 AND binding_generation=$7
          AND result_head=$8 AND target_ref IS NOT DISTINCT FROM $9 AND kind=$10 AND evidence=$11 AND actor_id=$12)",
    ).bind(operation).bind(plan.home).bind(plan.task).bind(source.project_id).bind(source.project_revision)
     .bind(source.repository_id).bind(source.binding_generation).bind(&source.head).bind(&source.target_ref)
     .bind(kind).bind(evidence).bind(actor).fetch_one(db).await.map_err(crate::error::db)?;
    if !exact {
        return Err(conflict(
            "workspace target evidence operation ID already used",
        ));
    }
    Ok(())
}
