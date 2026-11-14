//! The route on a task (spec 0011 §3) and the routing switch (§4). The current
//! route is `aq:route` in the task's graph metadata, written with the task's
//! expected version; `task_routes` keeps every route the task has had.
use crate::{
    AppState,
    agent_access::{self, human::graph_actions::current},
    error::{AppError, db},
    profile_catalog::settings::error as settings_error,
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

/// Routing turns on only while a routing playbook is active (spec 0013 §7). None can be active
/// until playbook authoring lands, so turning it on is refused; turning it off always works.
pub async fn set_enabled(pool: &PgPool, on: bool) -> Result<(), AppError> {
    if on {
        return Err(AppError::Conflict(
            "Routing can be turned on once a routing playbook is active".into(),
        ));
    }
    sqlx::query("UPDATE routing_state SET enabled=FALSE WHERE singleton")
        .execute(pool)
        .await
        .map_err(db)?;
    Ok(())
}

/// What Settings offers: each class with an assigned profile in the active catalog, and every
/// (provider, harness) pair of that profile's candidates. Worker qualification is not read: a
/// route may name what no worker can run now, and placement makes such a route wait.
pub async fn options(state: &AppState) -> Result<Options, AppError> {
    let (_, digest) = crate::project_bindings::active_catalog(state).await?;
    let Some(digest) = digest else {
        return Ok(Options::default());
    };
    let catalog = crate::project_bindings::catalog(state, &digest).await?;
    let pool = &state.store.pool;
    let (document, _, _) = aq_profile_settings::structure::document(pool)
        .await
        .map_err(settings_error)?;
    let profiles = &catalog.artifact().catalog.intelligence;
    let mut options = Options::default();
    for a in &document.assignments {
        let class = format!("{}-{}", a.parent_id, a.effort).parse::<Class>();
        let profile =
            (profiles.iter()).find(|p| p.id == a.profile_id && p.automatic_routing != Some(true));
        if let (Ok(class), Some(profile)) = (class, profile) {
            let candidates =
                (profile.candidates.iter()).map(|c| (c.provider.as_str(), c.harnesses.as_slice()));
            options.classes.insert(class, classify::offers(candidates));
        }
    }
    Ok(options)
}

/// The profile Settings assigns to the class's parent and effort, as `id@version`.
/// The automatic router is never a class's profile.
pub async fn class_profile(
    pool: &PgPool,
    catalog: &aq_profiles::ValidatedCatalog,
    class: Class,
) -> Result<Option<String>, AppError> {
    let (document, _, _) = aq_profile_settings::structure::document(pool)
        .await
        .map_err(settings_error)?;
    let Some(assignment) = document
        .assignments
        .iter()
        .find(|a| a.parent_id == class.parent.as_str() && a.effort == class.effort.as_str())
    else {
        return Ok(None);
    };
    Ok(catalog
        .artifact()
        .catalog
        .intelligence
        .iter()
        .find(|p| p.id == assignment.profile_id && p.automatic_routing != Some(true))
        .map(|p| aq_profiles::key(&p.id, p.version)))
}

/// Whether Settings assigns a profile to the class in the active catalog.
pub async fn class_assigned(state: &AppState, class: Class) -> Result<bool, AppError> {
    let (_, digest) = crate::project_bindings::active_catalog(state).await?;
    let Some(digest) = digest else {
        return Ok(false);
    };
    let catalog = crate::project_bindings::catalog(state, &digest).await?;
    Ok(class_profile(&state.store.pool, &catalog, class)
        .await?
        .is_some())
}

fn router_agent(catalog: &aq_profiles::ValidatedCatalog, agent: &str) -> bool {
    let artifact = &catalog.artifact().catalog;
    (artifact.agents.iter())
        .find(|a| aq_profiles::key(&a.id, a.version) == agent)
        .is_some_and(|a| aq_selection::is_automatic_router(artifact, &a.intelligence))
}

/// Whether `record`'s binding runs its agent on an automatic router.
pub(crate) async fn on_router(
    state: &AppState,
    record: &crate::project_bindings::domain::ProjectRecord,
) -> Result<bool, AppError> {
    let [agent] = record.binding.allowed_agent_profiles.as_slice() else {
        return Ok(false);
    };
    let catalog = crate::project_bindings::catalog(state, &record.binding.catalog_digest).await?;
    Ok(router_agent(&catalog, agent))
}

/// Spec 0011 §4: how placement treats each ready task of `record`'s project, keyed by
/// task; a task left out is placed unchanged. Empty unless the binding's agent is on an
/// automatic router, so a route is ignored for a binding pinned to a named profile.
pub async fn placements(
    state: &AppState,
    record: &crate::project_bindings::domain::ProjectRecord,
    ready: &[crate::agent_access::scheduling::ReadyTask],
) -> Result<std::collections::BTreeMap<Uuid, Placement>, AppError> {
    let mut out = std::collections::BTreeMap::new();
    let [agent] = record.binding.allowed_agent_profiles.as_slice() else {
        return Ok(out);
    };
    if ready.is_empty() {
        return Ok(out);
    }
    let catalog = crate::project_bindings::catalog(state, &record.binding.catalog_digest).await?;
    if !router_agent(&catalog, agent) {
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
    let mut assigned = std::collections::BTreeMap::new();
    let mut routed_ids = vec![];
    for task in ready {
        let (id, metadata) = (task.revision.task_id, &task.node["metadata"]);
        let attempts = ended.get(&id).map_or(0, |n| *n as u64);
        let mut placement = Placement::of(true, on, metadata, attempts);
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
        if let Placement::Routed(route) = &placement {
            let known = assigned.get(&route.c).copied();
            let has = match known {
                Some(has) => has,
                None => class_profile(pool, &catalog, route.c).await?.is_some(),
            };
            assigned.insert(route.c, has);
            if !has {
                placement = Placement::Reroute;
            }
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
