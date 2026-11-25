//! Workspace expansion is checkpointed by the existing per-attempt handoff.
//! A fresh admission re-evaluates the reviewed scope and worker capability.
use crate::error::{AppError, db};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

pub(crate) const HEADER: &str = "aq-workspace-expansion";
pub(crate) const VERSION: &str = "1";
pub(crate) const UNSUPPORTED: &str = "workspace_expansion_worker_unsupported";
pub(crate) const CHANGED: &str = "workspace_expansion_offer_scope_changed";
pub(crate) const CHECKPOINT: &str = "workspace_expansion_checkpoint_pending";

async fn supported(connection: &mut PgConnection) -> Result<bool, AppError> {
    sqlx::query_scalar("SELECT to_regclass('task_workspace_offer_fences') IS NOT NULL")
        .fetch_one(connection)
        .await
        .map_err(db)
}

#[derive(Clone)]
pub(crate) struct Scope {
    pub evidence: Value,
    blocker: Option<&'static str>,
    requires_workspace: bool,
    supported: bool,
}

impl Scope {
    pub(crate) fn blocker(&self, capable: bool) -> Option<&'static str> {
        self.blocker.or_else(|| {
            if self.requires_workspace && !capable {
                Some(UNSUPPORTED)
            } else if !cfg!(feature = "project_environments") && self.has_services() {
                Some("workspace_expansion_service_capability_unavailable")
            } else {
                None
            }
        })
    }

    fn has_services(&self) -> bool {
        self.evidence["services"]
            .as_array()
            .is_some_and(|services| !services.is_empty())
    }

    #[cfg(feature = "project_environments")]
    pub(crate) fn apply_service_needs(&self, metadata: &mut Value) {
        if !self.has_services() {
            return;
        }
        use aq_project_policy::environments::{NEEDS_KEY, needs};
        let mut labels = needs(metadata);
        labels.extend(
            self.evidence["services"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
        labels.sort_unstable();
        labels.dedup();
        if !metadata.is_object() {
            *metadata = serde_json::json!({});
        }
        metadata[NEEDS_KEY] = serde_json::json!(labels);
    }
}

pub(crate) async fn scope_at_poll(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
) -> Result<Scope, AppError> {
    scope(&mut *pool.acquire().await.map_err(db)?, project, task).await
}

pub(crate) async fn scope(
    connection: &mut PgConnection,
    project: Uuid,
    task: Uuid,
) -> Result<Scope, AppError> {
    if !supported(connection).await? {
        return Ok(Scope {
            evidence: serde_json::json!({"sources":[],"services":[]}),
            blocker: None,
            requires_workspace: false,
            supported: false,
        });
    }
    let row = sqlx::query(
        "WITH sources AS (
             SELECT s.*, p.project_id IS NOT NULL
                 AND p.binding->>'enabled'='true'
                 AND p.revision=s.project_revision
                 AND p.binding->>'repository_id'=s.repository_id::text
                 AND p.binding->>'repository_binding_generation'=s.binding_generation::text
                 AS current_binding
             FROM task_workspace_sources s
             LEFT JOIN project_execution_bindings p ON p.project_id=s.target_project_id
             WHERE s.home_project_id=$1 AND s.task_id=$2
         ), services AS (
             SELECT service FROM task_workspace_services WHERE home_project_id=$1 AND task_id=$2
         )
         SELECT
             jsonb_build_object('sources',COALESCE((
                 SELECT jsonb_agg(to_jsonb(s)-'current_binding' ORDER BY s.target_project_id)
                 FROM sources s WHERE s.state='approved'
             ),'[]'::jsonb),'services',COALESCE((
                 SELECT jsonb_agg(service ORDER BY service) FROM services
             ),'[]'::jsonb)) AS evidence,
             EXISTS(SELECT 1 FROM task_workspace_creations
                    WHERE home_project_id=$1 AND task_id=$2) AS creation_pending,
             EXISTS(SELECT 1 FROM sources WHERE state='denied') AS denied,
             EXISTS(SELECT 1 FROM sources WHERE state='pending') AS pending,
             EXISTS(SELECT 1 FROM sources WHERE state='approved' AND current_binding IS NOT TRUE) AS stale,
             EXISTS(SELECT 1 FROM sources) OR EXISTS(SELECT 1 FROM services) AS required",
    )
    .bind(project)
    .bind(task)
    .fetch_one(connection)
    .await
    .map_err(db)?;
    Ok(Scope {
        evidence: row.get("evidence"),
        blocker: match (
            row.get("creation_pending"),
            row.get("denied"),
            row.get("pending"),
            row.get("stale"),
        ) {
            (true, _, _, _) => Some("workspace_creation_pending"),
            (_, true, _, _) => Some("workspace_expansion_approval_denied"),
            (_, _, true, _) => Some("workspace_expansion_approval_pending"),
            (_, _, _, true) => Some("workspace_expansion_binding_stale"),
            _ => None,
        },
        requires_workspace: row.get("required"),
        supported: true,
    })
}

pub(crate) async fn freeze(
    connection: &mut PgConnection,
    attempt: Uuid,
    scope: Scope,
) -> Result<(), AppError> {
    if scope.requires_workspace {
        sqlx::query("INSERT INTO task_workspace_offer_fences(attempt_id,scope) VALUES($1,$2)")
            .bind(attempt)
            .bind(scope.evidence)
            .execute(connection)
            .await
            .map_err(db)?;
    }
    Ok(())
}

pub(crate) async fn still_current(
    connection: &mut PgConnection,
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
    capable: bool,
) -> Result<Option<&'static str>, AppError> {
    let current = scope(connection, project, task).await?;
    if !current.supported {
        return Ok(None);
    }
    if let Some(blocker) = current.blocker(capable) {
        return Ok(Some(blocker));
    }
    let offered: Option<Value> =
        sqlx::query_scalar("SELECT scope FROM task_workspace_offer_fences WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_optional(connection)
            .await
            .map_err(db)?;
    let matches = offered
        .as_ref()
        .map_or(!current.requires_workspace, |offered| {
            offered == &current.evidence
        });
    Ok((!matches).then_some(CHANGED))
}

pub(crate) async fn fence_start(
    connection: &mut PgConnection,
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
) -> Result<(), AppError> {
    match still_current(connection, project, task, attempt, true).await? {
        Some(reason) => Err(AppError::Conflict(reason.into())),
        None => Ok(()),
    }
}

pub(crate) async fn checkpoint_required(
    connection: &mut PgConnection,
    attempt: Uuid,
) -> Result<bool, AppError> {
    if !supported(connection).await? {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM attempt_handoffs
                       WHERE attempt_id=$1 AND reason='workspace_expansion')
             OR EXISTS(SELECT 1 FROM task_workspace_sources WHERE requested_attempt_id=$1)
             OR EXISTS(SELECT 1 FROM task_workspace_services WHERE requested_attempt_id=$1)",
    )
    .bind(attempt)
    .fetch_one(connection)
    .await
    .map_err(db)
}
