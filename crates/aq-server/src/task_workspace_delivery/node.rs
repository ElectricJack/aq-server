use super::{conflict, now};
use crate::{
    AppState,
    error::{AppError, db},
    profiled_execution::git_grant::workspaces::live_grant_locked,
    worker_api,
};
use aq_delivery::commands::{GitTarget, SocketGit, object_id};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::HeaderMap,
    routing::put,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    workspaces: Vec<Output>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    project_id: Uuid,
    repository_id: Uuid,
    result: Evidence,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    status: Status,
    head: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Unchanged,
    Pushed,
    Conflict,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Pushed => "pushed",
            Self::Conflict => "conflict",
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/git/grants/{attempt}/workspaces/result", put(record))
        .layer(DefaultBodyLimit::max(12 * 1024))
}

async fn record(
    State(state): State<AppState>,
    Path(attempt): Path<Uuid>,
    headers: HeaderMap,
    Json(report): Json<Report>,
) -> Result<Json<Value>, AppError> {
    let node = worker_api::authenticate_context(&state, &headers).await?;
    if !super::available(&state) {
        return Err(AppError::NotFound(
            "task workspace results unavailable".into(),
        ));
    }
    let ids: BTreeSet<_> = report
        .workspaces
        .iter()
        .map(|item| item.project_id)
        .collect();
    if ids.is_empty()
        || ids.len() > 16
        || ids.len() != report.workspaces.len()
        || report.workspaces.iter().any(|item| !valid(&item.result))
    {
        return Err(AppError::BadRequest(
            "invalid task workspace results".into(),
        ));
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    // Serialize with the stopped report writer before checking whether this
    // attempt is still allowed to freeze new source facts.
    let live: Option<bool> = sqlx::query_scalar(
        "SELECT a.holds_task AND h.state->>'status'='finished'
            AND h.state#>>'{stop,kind}'='stopped'
            AND NULLIF(h.state->>'cancellation_id','') IS NULL
            AND NOT EXISTS (SELECT 1 FROM profiled_result_heads r WHERE r.attempt_id=a.attempt_id)
         FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id)
         WHERE a.attempt_id=$1 AND a.node_id=$2 FOR UPDATE OF a,h",
    )
    .bind(attempt)
    .bind(node.id())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    let live = live.ok_or_else(|| AppError::NotFound("task workspace grant not found".into()))?;
    let rows = sqlx::query(
        "SELECT g.target_project_id,g.repository_id,g.binding_generation,g.task_ref,g.lease,
            coalesce(to_jsonb(g)->>'checkout_head',g.commit_oid) AS checkout_head,
            s.status,s.result_head
         FROM task_workspace_git_grants g
         LEFT JOIN task_workspace_result_sources s USING(attempt_id,target_project_id)
         WHERE g.attempt_id=$1 AND g.access='write' ORDER BY g.target_project_id",
    )
    .bind(attempt)
    .fetch_all(&mut *tx)
    .await
    .map_err(db)?;
    if rows.len() != ids.len() {
        return Err(conflict(
            "workspace result set differs from the sealed write grants",
        ));
    }
    for row in rows {
        let project: Uuid = row.get("target_project_id");
        let Some(output) = report
            .workspaces
            .iter()
            .find(|item| item.project_id == project)
        else {
            return Err(conflict(
                "workspace result set differs from the sealed write grants",
            ));
        };
        let repository_id: Uuid = row.get("repository_id");
        if repository_id != output.repository_id {
            return Err(conflict(
                "workspace result repository differs from its grant",
            ));
        }
        if let Some(status) = row.get::<Option<String>, _>("status") {
            if status != output.result.status.as_str()
                || row.get::<Option<String>, _>("result_head") != output.result.head
            {
                return Err(conflict(
                    "workspace result already recorded with different evidence",
                ));
            }
            continue;
        }
        if !live
            || live_grant_locked(&mut tx, attempt, node.id(), project)
                .await
                .map_err(db)?
                .is_none()
        {
            return Err(conflict("workspace result grant is no longer current"));
        }
        let generation: i64 = row.get("binding_generation");
        let target = GitTarget {
            repository_id,
            binding_generation: u64::try_from(generation).map_err(|_| AppError::Internal)?,
        };
        let reference: String = row.get("task_ref");
        let lease: Option<String> = row.get("lease");
        let base: String = row.get("checkout_head");
        let storage = &state
            .source_retention
            .as_ref()
            .ok_or_else(|| AppError::UpdatesUnavailable("source retention unavailable".into()))?
            .config
            .git_socket;
        let actual = SocketGit::new(storage)
            .show_ref(&target, &reference)
            .await
            .map_err(|_| AppError::UpdatesUnavailable("central Git unavailable".into()))?;
        if !observed(&output.result, &base, lease.as_deref(), actual.as_deref()) {
            return Err(conflict(
                "central workspace reference does not match the reported result",
            ));
        }
        sqlx::query(
            "INSERT INTO task_workspace_result_sources(attempt_id,target_project_id,result_head,status,recorded_at_ms)
             VALUES($1,$2,$3,$4,$5)",
        ).bind(attempt).bind(project).bind(&output.result.head).bind(output.result.status.as_str())
         .bind(now(&state)?).execute(&mut *tx).await.map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(Json(json!({"status":"recorded"})))
}

fn valid(result: &Evidence) -> bool {
    match (&result.head, result.status) {
        (None, Status::Conflict) => true,
        (Some(head), Status::Pushed | Status::Unchanged) => {
            object_id(head) && head.bytes().any(|b| b != b'0')
        }
        _ => false,
    }
}

fn observed(result: &Evidence, base: &str, lease: Option<&str>, actual: Option<&str>) -> bool {
    let head = result.head.as_deref();
    match result.status {
        Status::Unchanged => head == Some(base) && actual == lease,
        Status::Pushed => head != Some(base) && actual == head,
        Status::Conflict => actual != lease,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_distinguishes_unchanged_pushed_and_moved_workspaces() {
        let unchanged = Evidence {
            status: Status::Unchanged,
            head: Some("base".into()),
        };
        let pushed = Evidence {
            status: Status::Pushed,
            head: Some("new".into()),
        };
        let conflict = Evidence {
            status: Status::Conflict,
            head: None,
        };
        assert!(observed(&unchanged, "base", None, None));
        assert!(!observed(&unchanged, "base", None, Some("new")));
        assert!(observed(&pushed, "base", Some("base"), Some("new")));
        assert!(!observed(&pushed, "base", Some("base"), Some("different")));
        assert!(observed(&conflict, "base", Some("base"), Some("different")));
        assert!(!observed(&conflict, "base", Some("base"), Some("base")));
    }
}
