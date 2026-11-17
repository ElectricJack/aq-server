//! The route on a task (spec 0011 §3) and the routing switch (§4). The current
//! route is `aq:route` in the task's graph metadata, written with the task's
//! expected version; `task_routes` keeps every route the task has had.
use crate::{
    AppState,
    agent_access::{self, human::graph_actions::current},
    error::{AppError, db},
};
mod http;
use aq_routing::{
    Class, CompactRoute, Placement, Route, Source,
    classify::{self, Options},
};
pub use aq_routing::{StoredRoute, Written};
pub(crate) use http::{admin_routes, settings_routes, view};
use serde_json::json;
use sqlx::{PgPool, types::Json};
use std::time::Duration;
use uuid::Uuid;

pub async fn write(
    state: &AppState,
    workspace: Uuid,
    task: Uuid,
    expected_version: u64,
    route: &Route,
) -> Result<Written, AppError> {
    let by = route.source;
    store(state, workspace, task, expected_version, by, Some(route)).await
}

pub async fn clear(
    state: &AppState,
    workspace: Uuid,
    task: Uuid,
    expected_version: u64,
    by: Source,
) -> Result<Written, AppError> {
    store(state, workspace, task, expected_version, by, None).await
}

fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("Graph route write unavailable".into())
}

async fn store(
    state: &AppState,
    workspace: Uuid,
    task: Uuid,
    expected_version: u64,
    by: Source,
    route: Option<&Route>,
) -> Result<Written, AppError> {
    let rt = state.agent_access.as_deref().ok_or_else(unavailable)?;
    let node = current(rt, workspace, task, "task").await?;
    // The metadata cloned below is this version's; post it only against this version.
    if node["version"].as_u64() != Some(expected_version) {
        return Ok(Written::Stale);
    }
    // An unreadable route is replaced like no route, so placement can clear it (spec 0011 §4).
    let stored = CompactRoute::from_metadata(&node["metadata"]).unwrap_or(None);
    if !aq_routing::may_replace(stored.as_ref(), by) {
        return Ok(Written::Stale);
    }
    let pool = &state.store.pool;
    let mut tx = pool.begin().await.map_err(db)?;
    let inserted = sqlx::query_scalar::<_, i64>(
        "INSERT INTO task_routes(task_id,sequence,route,at_ms)
         SELECT $1, COALESCE(MAX(sequence),0)+1, $2, $3 FROM task_routes WHERE task_id=$1
         RETURNING sequence",
    )
    .bind(task)
    .bind(route.map(Json))
    .bind((state.clock)().timestamp_millis())
    .fetch_one(&mut *tx)
    .await;
    let sequence = match inserted {
        // A concurrent writer took this sequence; at most one of us can win the version.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Ok(Written::Stale),
        r => r.map_err(db)? as u64,
    };
    // Until the commit below, any error rolls the history row back.
    let compact = route
        .map(|r| r.compact(sequence))
        .transpose()
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let metadata =
        aq_routing::with_route(&node["metadata"], compact.as_ref()).map_err(|_| unavailable())?;
    let body = json!({
        "idempotency_key": format!("aq-route:{task}:{sequence}:{}", Uuid::new_v4().simple()),
        "reason": "AQ routing",
        "operations": [{"op":"update_node","id":task,"expected_version":expected_version,
            "patch":{"metadata":metadata}}],
    });
    if !agent_access::settlement_request_valid(&body) {
        return Err(unavailable());
    }
    let bytes = serde_json::to_vec(&body).map_err(|_| unavailable())?;
    tx.commit().await.map_err(db)?;
    let posted = tokio::time::timeout(
        Duration::from_secs(3),
        agent_access::settlement_post(rt, workspace, bytes),
    )
    .await;
    // A lost or unreadable response keeps the history row: the update may have applied.
    let Ok(Ok((status, raw))) = posted else {
        return Err(unavailable());
    };
    if status.is_success() && agent_access::settlement_response_valid(&raw) {
        if route.is_some() {
            routed(pool, task).await;
        }
        return Ok(Written::Routed { sequence });
    }
    let code = raw["error"]["code"].as_str();
    let stale = status.as_u16() == 409 && code == Some("conflict");
    // Graph refused the update outright, so this route never existed.
    if stale || (matches!(status.as_u16(), 400 | 401 | 403 | 404 | 422) && code.is_some()) {
        sqlx::query("DELETE FROM task_routes WHERE task_id=$1 AND sequence=$2")
            .bind(task)
            .bind(sequence as i64)
            .execute(pool)
            .await
            .map_err(db)?;
    }
    if stale {
        return Ok(Written::Stale);
    }
    Err(unavailable())
}

/// The task has a route: what was raised about its routing is resolved (spec
/// 0011 §4 W6) and why it was left unrouted no longer applies. Never fails
/// the write.
async fn routed(pool: &PgPool, task: Uuid) {
    let affected = aq_internal_work::Affected {
        kind: "task".into(),
        id: task.to_string(),
        project_id: None,
    };
    let purpose = aq_internal_work::Purpose::RoutingClassification;
    let resolved = aq_internal_work::store::resolve_failures(pool, purpose, &affected).await;
    let cleared = sqlx::query(
        "UPDATE route_needed_emissions SET wanted_at_ms=NULL,outcome=NULL,reasons=NULL WHERE task_id=$1",
    )
    .bind(task)
    .execute(pool)
    .await;
    if resolved.is_err() || cleared.is_err() {
        tracing::warn!(%task, "routing failure of a routed task not resolved");
    }
}

/// Why the task was last left unrouted (`not_offered` or `unroutable`, with
/// the playbook's reasons), while that still holds.
pub(crate) async fn unrouted(
    pool: &PgPool,
    task: Uuid,
) -> Result<Option<serde_json::Value>, AppError> {
    let row: Option<(String, Option<Json<serde_json::Value>>, i64)> = sqlx::query_as(
        "SELECT outcome,reasons,task_version FROM route_needed_emissions WHERE task_id=$1 AND outcome IS NOT NULL",
    )
    .bind(task)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    Ok(row.map(|(outcome, reasons, version)| {
        json!({"outcome": outcome, "reasons": reasons.map(|r| r.0), "task_version": version})
    }))
}

/// Every route the task has had, newest first.
pub async fn history(pool: &PgPool, task: Uuid) -> Result<Vec<StoredRoute>, AppError> {
    let rows = sqlx::query_as::<_, (i64, Option<Json<Route>>, i64)>(
        "SELECT sequence,route,at_ms FROM task_routes WHERE task_id=$1 ORDER BY sequence DESC",
    )
    .bind(task)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    Ok(rows
        .into_iter()
        .map(|(sequence, route, at_ms)| StoredRoute {
            sequence: sequence as u64,
            route: route.map(|r| r.0),
            at_ms,
        })
        .collect())
}

pub async fn enabled(pool: &PgPool) -> Result<bool, AppError> {
    sqlx::query_scalar("SELECT enabled FROM routing_state WHERE singleton")
        .fetch_one(pool)
        .await
        .map_err(db)
}

/// Routing turns on only while a routing playbook is active (spec 0013 §7); turning it off
/// always works.
pub async fn set_enabled(pool: &PgPool, on: bool) -> Result<(), AppError> {
    // In effect means the engine has it: a playbook only saved routes nothing.
    let in_effect = aq_playbook_authoring::active(pool, ROUTING_SOURCE).await?;
    if on && !in_effect.is_some_and(|run| run.deployed) {
        return Err(AppError::Conflict(
            "Routing can be turned on once a routing playbook is active".into(),
        ));
    }
    sqlx::query("UPDATE routing_state SET enabled=$1 WHERE singleton")
        .bind(on)
        .execute(pool)
        .await
        .map_err(db)?;
    Ok(())
}

pub use aq_playbook_authoring::builtin::{ROUTING as ROUTING_SOURCE, ROUTING_BINDING as BINDING};

/// Writes the built-in routing source's record (at start; the same record changes nothing).
pub async fn ensure_source(pool: &PgPool) -> Result<(), AppError> {
    let record = aq_playbook_authoring::builtin::routing();
    Ok(aq_playbook_authoring::ensure_builtin(pool, &record).await?)
}

/// Authors the routing document after a save of the structure or of Settings. Nothing starts
/// for an empty document or when nothing the agent would see changed. A server without an
/// open playbook engine, or without the source, only logs: the save has succeeded.
pub async fn author(state: &AppState) {
    let document = aq_profile_settings::structure::document(&state.store.pool).await;
    if document.is_ok_and(|(d, _, _)| d.routing_markdown.trim().is_empty()) {
        return;
    }
    if let Err(error) = aq_playbook_authoring::author(state, ROUTING_SOURCE).await {
        tracing::info!(%error, "the routing document was not authored");
    }
}

/// When the engine opens and then on a slow pass: authors the routing document unless its
/// newest run was already for the document and offers as they are (a missed save is caught up).
pub(crate) async fn reconcile(state: AppState) {
    let every = Duration::from_millis(state.config.playbook_authoring.poll_ms.saturating_mul(30));
    loop {
        let pool = &state.store.pool;
        let document = aq_profile_settings::structure::document(pool).await;
        let empty = document.is_ok_and(|(d, _, _)| d.routing_markdown.trim().is_empty());
        if !empty
            && ensure_source(pool).await.is_ok()
            && let Err(error) = aq_playbook_authoring::author_missed(&state, ROUTING_SOURCE).await
        {
            tracing::info!(%error, "the routing document is not authored yet");
        }
        tokio::time::sleep(every).await;
    }
}

/// What Settings offers: each class that has a level document in the active role
/// snapshot (a level's id is the router's class, spec 0014 §5.2), and every
/// (provider, harness) pair of that level's candidates. Worker inventory is not read: a
/// route may name what no worker can run now, and placement makes such a route wait.
pub async fn options(state: &AppState) -> Result<Options, AppError> {
    let mut options = Options::default();
    let Some(snapshot) = crate::role_settings::active(&state.store.pool).await? else {
        return Ok(options);
    };
    for profile in snapshot.intelligence_profiles() {
        if let Ok(class) = profile.id.parse::<Class>() {
            let candidates =
                (profile.candidates.iter()).map(|c| (c.provider.as_str(), c.harnesses.as_slice()));
            options.classes.insert(class, classify::offers(candidates));
        }
    }
    Ok(options)
}

/// Whether the active role snapshot has the level document of the class.
pub async fn class_assigned(state: &AppState, class: Class) -> Result<bool, AppError> {
    let snapshot = crate::role_settings::active(&state.store.pool).await?;
    Ok(snapshot.is_some_and(|s| s.intelligence(&class.to_string()).is_some()))
}

/// Every project's tasks are routed: no binding names a router profile any
/// more (spec 0014 §5.2). Kept for the callers that still ask; removed with
/// the old catalog.
pub(crate) async fn on_router(
    _state: &AppState,
    _record: &crate::project_bindings::domain::ProjectRecord,
) -> Result<bool, AppError> {
    Ok(true)
}

/// Spec 0011 §4: how placement treats each ready task of `record`'s project, keyed by
/// task; a task left out is placed unchanged. A route names its level by class id; a
/// class with no level document holds the task (`role_settings::held`), it is not
/// routed again.
pub async fn placements(
    state: &AppState,
    record: &crate::project_bindings::domain::ProjectRecord,
    ready: &[crate::agent_access::scheduling::ReadyTask],
) -> Result<std::collections::BTreeMap<Uuid, Placement>, AppError> {
    let mut out = std::collections::BTreeMap::new();
    if ready.is_empty() || !crate::role_settings::tables(state) {
        return Ok(out);
    }
    let (pool, on) = (&state.store.pool, enabled(&state.store.pool).await?);
    let ids: Vec<Uuid> = ready.iter().map(|r| r.revision.task_id).collect();
    let ended: std::collections::BTreeMap<Uuid, i64> = sqlx::query_as(
        "SELECT task_id,count(*) FROM execution_attempts WHERE task_id=ANY($1) AND NOT holds_task GROUP BY 1",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await
    .map_err(db)?
    .into_iter()
    .collect();
    let (now, project) = (
        (state.clock)().timestamp_millis(),
        record.binding.project_id,
    );
    let mut routed_ids = vec![];
    for task in ready {
        let (id, metadata) = (task.revision.task_id, &task.node["metadata"]);
        let attempts = ended.get(&id).map_or(0, |n| *n as u64);
        let placement = Placement::of(true, on, metadata, attempts);
        if placement == Placement::Reroute
            && matches!(CompactRoute::from_metadata(metadata), Ok(Some(_)))
        {
            // Retried after it was routed: the route goes, and the task is
            // asked about at its new version on the next poll.
            let t = &task.revision;
            let cleared = clear(state, t.workspace_id, id, t.version, Source::Router).await;
            if let Err(error) = cleared {
                tracing::warn!(task = %id, ?error, "outdated route not cleared");
            }
            out.insert(id, Placement::Wait);
            continue;
        }
        if placement == Placement::Wait {
            wanted(pool, now, project, &task.revision).await?;
        } else if matches!(placement, Placement::Routed(_)) {
            routed_ids.push(id);
        }
        if placement != Placement::Unchanged {
            out.insert(task.revision.task_id, placement);
        }
    }
    // A task seen with a route no longer waits for one.
    if !routed_ids.is_empty() {
        sqlx::query("UPDATE route_needed_emissions SET wanted_at_ms=NULL WHERE task_id=ANY($1) AND wanted_at_ms IS NOT NULL")
            .bind(&routed_ids)
            .execute(pool)
            .await
            .map_err(db)?;
    }
    Ok(out)
}

/// Records that a ready task waits for a route. The playbook owner sends
/// `aq.work.route_needed` from these rows (`collect_route_needed`); a row is
/// rewritten only for a new task version or every 30 s while the task waits.
async fn wanted(
    pool: &PgPool,
    now: i64,
    project: Uuid,
    task: &aq_admission::TaskRevision,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO route_needed_emissions(task_id,task_version,emitted_at_ms,wanted_version,wanted_at_ms,workspace_id,project_id,wanted_since_ms)
         VALUES($1,0,0,$2,$3,$4,$5,$3) ON CONFLICT(task_id) DO UPDATE
         SET wanted_version=$2,wanted_at_ms=$3,workspace_id=$4,project_id=$5,
             wanted_since_ms=CASE WHEN route_needed_emissions.wanted_version IS DISTINCT FROM $2
                 OR route_needed_emissions.wanted_at_ms IS NULL OR route_needed_emissions.wanted_at_ms<$3-$7
                 THEN $3 ELSE COALESCE(route_needed_emissions.wanted_since_ms,$3) END
         WHERE route_needed_emissions.wanted_version IS DISTINCT FROM $2
            OR route_needed_emissions.wanted_at_ms IS NULL OR route_needed_emissions.wanted_at_ms<=$3-$6",
    )
    .bind(task.task_id)
    .bind(task.version as i64)
    .bind(now)
    .bind(task.workspace_id)
    .bind(project)
    .bind(aq_playbook_domain::host9::ROUTE_WANTED_REFRESH_MS)
    .bind(aq_playbook_domain::host9::ROUTE_WANTED_FRESH_MS)
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(())
}

/// A ready task whose route cannot be placed is routed again. The refusal is recorded only
/// against an attempt the task has had, with the attempt count read before the decision
/// (`attempt`). Never fails the poll.
pub async fn reroute(
    state: &AppState,
    project: Uuid,
    ready: &[crate::agent_access::scheduling::ReadyTask],
    id: Uuid,
    attempt: Option<(Uuid, Option<i32>)>,
) {
    let Some(task) = ready
        .iter()
        .find(|r| r.revision.task_id == id)
        .map(|r| &r.revision)
    else {
        return;
    };
    let pool = &state.store.pool;
    match clear(state, task.workspace_id, id, task.version, Source::Router).await {
        Ok(Written::Routed { .. }) => {
            tracing::info!(task = %id, "unplaceable route cleared; routed again")
        }
        Ok(Written::Stale) => {}
        Err(error) => tracing::warn!(task = %id, ?error, "unplaceable route not cleared"),
    }
    if let Some((predecessor, seen)) = attempt {
        let s = aq_machine_control::holds::Successor {
            project,
            task: id,
            predecessor,
            seen,
        };
        let reasons = ["route_class_unassigned".to_owned()];
        aq_machine_control::holds::selection_refused(pool, s, &reasons, (state.clock)()).await;
    }
}
