//! Exact multi-repository evidence for the existing task settlement owner.
//! Task state and approval remain owned by the Home task and its review flow.
mod node;
mod owner;

use crate::{AppState, error::AppError, profiled_execution::results::RecordedResult};
use serde::Serialize;
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

pub(crate) use node::routes as worker_routes;
pub(crate) use owner::routes as admin_routes;

#[derive(Clone, Serialize)]
struct Source {
    project_id: Uuid,
    project_revision: i64,
    repository_id: Uuid,
    binding_generation: i64,
    head: String,
    changed: bool,
    target_ref: Option<String>,
    publication_mode: Option<String>,
}

#[derive(Serialize)]
struct Plan {
    home: Uuid,
    task: Uuid,
    sources: BTreeMap<Uuid, Source>,
    missing_projects: Vec<Uuid>,
}

impl Plan {
    fn complete(&self) -> bool {
        self.missing_projects.is_empty()
    }

    fn digest(&self) -> Result<String, AppError> {
        let bytes = serde_json::to_vec(&self.sources).map_err(|_| AppError::Internal)?;
        Ok(aq_admission::Digest::of(&bytes).as_str().to_owned())
    }
}

fn conflict(message: &str) -> AppError {
    AppError::Conflict(message.into())
}

fn now(state: &AppState) -> Result<i64, AppError> {
    let ms = (state.clock)().timestamp_millis();
    if ms <= 0 {
        return Err(AppError::Internal);
    }
    Ok(ms)
}

fn available(_state: &AppState) -> bool {
    #[cfg(feature = "test-support")]
    if _state.store.fixture_profile.is_some_and(|v| v < 181) {
        return false;
    }
    true
}

async fn load(
    state: &AppState,
    db: &mut PgConnection,
    result: &RecordedResult,
) -> Result<Option<Plan>, AppError> {
    if !available(state) {
        return Ok(None);
    }
    let spec = &result.spec;
    let (home, task, attempt) = (
        spec.lifecycle.project_id,
        spec.task.task_id,
        result.report.binding.attempt_id,
    );
    if !has_scope(state, db, attempt, home, task).await? {
        return Ok(None);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("workspace:{home}:{task}"))
        .execute(&mut *db)
        .await
        .map_err(crate::error::db)?;
    sqlx::query(
        "SELECT project_id FROM project_execution_bindings
         WHERE project_id=$1 OR project_id IN (
           SELECT target_project_id FROM task_workspace_sources WHERE home_project_id=$1 AND task_id=$2
         )
         ORDER BY project_id FOR SHARE",
    ).bind(home).bind(task).fetch_all(&mut *db).await.map_err(crate::error::db)?;
    let supplemental = sqlx::query(
        "WITH required AS (
           SELECT target_project_id FROM task_workspace_git_grants WHERE attempt_id=$3
           UNION
           SELECT target_project_id FROM task_workspace_sources
           WHERE home_project_id=$1 AND task_id=$2 AND state IN ('pending','approved')
         )
         SELECT p.target_project_id,g.project_revision,g.repository_id,g.binding_generation,
           CASE WHEN g.access='read' THEN g.commit_oid ELSE r.result_head END AS head,
           g.access='write' AS writable,
           r.result_head<>g.commit_oid AS changed,b.binding#>>'{publication,target}' AS target_ref,
           b.binding#>>'{publication,mode}' AS publication_mode,
           coalesce(w.state='approved' AND w.access=g.access AND w.project_revision=g.project_revision
             AND w.repository_id=g.repository_id AND w.binding_generation=g.binding_generation
             AND w.commit_oid=g.commit_oid AND b.revision=g.project_revision
             AND b.binding->>'repository_id'=g.repository_id::text
             AND b.binding->>'repository_binding_generation'=g.binding_generation::text
             AND b.binding->>'enabled'='true',false) AS approved
         FROM required p
         LEFT JOIN task_workspace_git_grants g ON g.attempt_id=$3 AND g.target_project_id=p.target_project_id
         LEFT JOIN task_workspace_sources w ON w.home_project_id=$1 AND w.task_id=$2 AND w.target_project_id=p.target_project_id
         LEFT JOIN task_workspace_result_sources r ON r.attempt_id=g.attempt_id AND r.target_project_id=g.target_project_id
         LEFT JOIN project_execution_bindings b ON b.project_id=p.target_project_id
         ORDER BY p.target_project_id",
    ).bind(home).bind(task).bind(attempt).fetch_all(&mut *db).await.map_err(crate::error::db)?;
    let mut plan = Plan {
        home,
        task,
        sources: BTreeMap::new(),
        missing_projects: Vec::new(),
    };
    for row in supplemental {
        let project_id = row.get("target_project_id");
        let head: Option<String> = row.get("head");
        if let Some(head) = head.filter(|_| row.get::<bool, _>("approved")) {
            plan.sources.insert(
                project_id,
                Source {
                    project_id,
                    project_revision: row.get("project_revision"),
                    repository_id: row.get("repository_id"),
                    binding_generation: row.get("binding_generation"),
                    head,
                    changed: row.get::<Option<bool>, _>("writable") == Some(true)
                        && row.get::<Option<bool>, _>("changed") == Some(true),
                    target_ref: row.get("target_ref"),
                    publication_mode: row.get("publication_mode"),
                },
            );
        } else {
            plan.missing_projects.push(project_id);
        }
    }

    let source = &spec.source;
    let revision = i64::try_from(spec.project_revision).map_err(|_| AppError::Internal)?;
    let generation =
        i64::try_from(source.repository_binding_generation).map_err(|_| AppError::Internal)?;
    let home_result = sqlx::query(
        "SELECT r.head,b.binding#>>'{publication,target}' AS target_ref,b.binding#>>'{publication,mode}' AS publication_mode
         FROM delivery_pushed_results r CROSS JOIN project_execution_bindings b
         WHERE r.report_id=$1 AND r.attempt_id=$2 AND r.head IS NOT NULL
           AND b.project_id=$3 AND b.revision=$4 AND b.binding->>'repository_id'=$5::uuid::text
           AND b.binding->>'repository_binding_generation'=$6::bigint::text AND b.binding->>'enabled'='true'",
    ).bind(result.report.report_id).bind(attempt).bind(home).bind(revision).bind(source.repository_id).bind(generation)
     .fetch_optional(&mut *db).await.map_err(crate::error::db)?;
    if let Some(row) = home_result {
        let base = aq_machine_control::lineage_base(db, task, &source.resolved_object.hex).await?;
        let head: String = row.get("head");
        plan.sources.insert(
            home,
            Source {
                project_id: home,
                project_revision: revision,
                repository_id: source.repository_id,
                binding_generation: generation,
                changed: base.as_deref() != Some(head.as_str()),
                head,
                target_ref: row.get("target_ref"),
                publication_mode: row.get("publication_mode"),
            },
        );
    } else {
        plan.missing_projects.push(home);
    }
    Ok(Some(plan))
}

async fn check_passed(db: &mut PgConnection, plan: &Plan) -> Result<bool, AppError> {
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT verdict FROM task_workspace_integrated_checks
         WHERE home_project_id=$1 AND task_id=$2 AND inputs_digest=$3
         ORDER BY sequence DESC LIMIT 1",
    )
    .bind(plan.home)
    .bind(plan.task)
    .bind(plan.digest()?)
    .fetch_optional(db)
    .await
    .map_err(crate::error::db)?;
    Ok(latest.as_deref() == Some("passed"))
}

async fn target_accepted(
    db: &mut PgConnection,
    plan: &Plan,
    source: &Source,
) -> Result<bool, AppError> {
    sqlx::query_scalar(
        "SELECT EXISTS(
           SELECT 1 FROM task_workspace_target_acceptance
           WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3 AND project_revision=$4
             AND repository_id=$5 AND binding_generation=$6 AND result_head=$7
             AND target_ref IS NOT DISTINCT FROM $8
         ) OR EXISTS(
           SELECT 1 FROM delivery_merge_evidence
           WHERE task_id=$2 AND project_id=$3 AND expected_head_sha=$7 AND target_ref=$8
             AND target#>>'{git,repository_id}'=$5::uuid::text AND target#>>'{git,binding_generation}'=$6::bigint::text
         )",
    ).bind(plan.home).bind(plan.task).bind(source.project_id).bind(source.project_revision)
     .bind(source.repository_id).bind(source.binding_generation).bind(&source.head).bind(&source.target_ref)
     .fetch_one(db).await.map_err(crate::error::db)
}

pub(crate) async fn gate(
    state: &AppState,
    db: &mut PgConnection,
    result: &RecordedResult,
    human_home_review: bool,
) -> Result<bool, AppError> {
    let Some(plan) = load(state, db, result).await? else {
        return Ok(true);
    };
    if !plan.complete() || !check_passed(db, &plan).await? {
        return Ok(false);
    }
    for source in plan.sources.values().filter(|s| s.changed) {
        if source.project_id == plan.home && human_home_review && source.target_ref.is_none() {
            continue;
        }
        if !target_accepted(db, &plan, source).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) async fn has_supplemental(
    state: &AppState,
    db: &mut PgConnection,
    result: &RecordedResult,
) -> Result<bool, AppError> {
    has_scope(
        state,
        db,
        result.report.binding.attempt_id,
        result.spec.lifecycle.project_id,
        result.spec.task.task_id,
    )
    .await
}

pub(crate) async fn has_scope(
    state: &AppState,
    db: &mut PgConnection,
    attempt: Uuid,
    project: Uuid,
    task: Uuid,
) -> Result<bool, AppError> {
    if !available(state) {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM task_workspace_git_grants WHERE attempt_id=$1)
         OR EXISTS(SELECT 1 FROM task_workspace_sources WHERE home_project_id=$2 AND task_id=$3 AND state IN ('pending','approved'))",
    ).bind(attempt).bind(project).bind(task).fetch_one(db).await.map_err(crate::error::db)
}

pub(crate) async fn merge_readiness(
    state: &AppState,
    db: &mut PgConnection,
    result: &RecordedResult,
) -> Result<aq_playbook_domain::delivery_runs::MergeReadiness, AppError> {
    use aq_admission::outcome::ReviewRequirement;
    use aq_playbook_domain::delivery_runs::MergeReadiness;
    let Some(plan) = load(state, db, result).await? else {
        return Ok(MergeReadiness::Proceed);
    };
    if !plan.complete() || !check_passed(db, &plan).await? {
        return Ok(MergeReadiness::PendingEvidence);
    }
    let home = plan.sources.get(&plan.home).ok_or(AppError::Internal)?;
    if home.target_ref.is_some() && target_accepted(db, &plan, home).await? {
        return Ok(MergeReadiness::AlreadyDelivered);
    }
    if result.outcome.review == ReviewRequirement::HumanOwner {
        return Ok(MergeReadiness::PendingEvidence);
    }
    Ok(MergeReadiness::Proceed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(project_id: Uuid, head: char) -> Source {
        Source {
            project_id,
            project_revision: 1,
            repository_id: Uuid::new_v4(),
            binding_generation: 2,
            head: head.to_string().repeat(40),
            changed: true,
            target_ref: Some("refs/heads/main".into()),
            publication_mode: Some("pull_request".into()),
        }
    }

    #[tokio::test]
    async fn partial_retry_reuses_one_verified_merge_but_requires_new_integrated_checks() {
        let database = crate::test_support::TestDb::create().await;
        let _store = database.store().await;
        let pool = database.pool().await;
        let mut conn = pool.acquire().await.unwrap();
        let owner = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'delivery@example.org','owner',now())")
            .bind(owner).execute(&mut *conn).await.unwrap();
        let home = Uuid::new_v4();
        let sibling = Uuid::new_v4();
        let task = Uuid::new_v4();
        let dependency = Uuid::new_v4();
        let first = source(home, 'a');
        let second = source(sibling, 'b');
        let mut readonly = source(dependency, 'd');
        readonly.changed = false;
        let mut plan = Plan {
            home,
            task,
            sources: BTreeMap::from([
                (home, first.clone()),
                (sibling, second.clone()),
                (dependency, readonly),
            ]),
            missing_projects: vec![],
        };
        sqlx::query(
            "INSERT INTO task_workspace_target_acceptance(operation_id,home_project_id,task_id,target_project_id,
                project_revision,repository_id,binding_generation,result_head,target_ref,kind,evidence,actor_id,recorded_at_ms)
             VALUES($1,$2,$3,$2,1,$4,2,$5,'refs/heads/main','verified_merge','PR 23',$6,1)",
        ).bind(Uuid::new_v4()).bind(home).bind(task).bind(first.repository_id)
         .bind(&first.head).bind(owner).execute(&mut *conn).await.unwrap();
        sqlx::query(
            "INSERT INTO task_workspace_integrated_checks(operation_id,home_project_id,task_id,inputs_digest,verdict,evidence,actor_id,recorded_at_ms)
             VALUES($1,$2,$3,$4,'passed','suite 99',$5,1)",
        ).bind(Uuid::new_v4()).bind(home).bind(task).bind(plan.digest().unwrap())
         .bind(owner).execute(&mut *conn).await.unwrap();

        assert!(target_accepted(&mut conn, &plan, &first).await.unwrap());
        assert!(!target_accepted(&mut conn, &plan, &second).await.unwrap());
        assert!(check_passed(&mut conn, &plan).await.unwrap());
        plan.sources.get_mut(&dependency).unwrap().head = "e".repeat(40);
        assert!(!check_passed(&mut conn, &plan).await.unwrap());
        plan.sources.get_mut(&dependency).unwrap().head = "d".repeat(40);

        plan.sources.get_mut(&sibling).unwrap().head = "c".repeat(40);
        assert!(target_accepted(&mut conn, &plan, &first).await.unwrap());
        assert!(
            !target_accepted(&mut conn, &plan, &plan.sources[&sibling])
                .await
                .unwrap()
        );
        assert!(!check_passed(&mut conn, &plan).await.unwrap());
    }
}
