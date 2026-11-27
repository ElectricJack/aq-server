//! Revisioned central project policy. Graph tasks and Git bindings remain external.
pub use aq_project_policy as domain;
mod external;
pub(crate) mod workspace;
use crate::{AppState, authz::RequireOwner, error::AppError};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{get, post},
};
use domain::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
use crate::error::db;
fn bad(reason: &str) -> AppError {
    AppError::BadRequest(reason.into())
}
fn conflict(reason: &str) -> AppError {
    AppError::Conflict(reason.into())
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PutBinding {
    operation_id: Uuid,
    expected_revision: u64,
    binding: ProjectBinding,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Disable {
    operation_id: Uuid,
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    after: Option<Uuid>,
    limit: Option<u32>,
}
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .merge(workspace::owner_routes())
        .route("/api/projects", get(list))
        .route("/api/projects/{id}", get(inspect).post(put))
        .route("/api/projects/{id}/revisions/{revision}", get(history))
        .route("/api/projects/{id}/disable", post(disable))
        .route("/api/projects/{id}/eligibility", post(eligibility))
        .layer(DefaultBodyLimit::max(128 * 1024))
}
fn record(row: sqlx::postgres::PgRow) -> Result<ProjectRecord> {
    Ok(ProjectRecord {
        revision: row.get::<i64, _>("revision") as u64,
        binding: serde_json::from_value(row.get("binding")).map_err(|_| AppError::Internal)?,
        validated_at_ms: row.get::<i64, _>("validated_at_ms") as u64,
        validation: serde_json::from_value(row.get("binding_observation"))
            .map_err(|_| AppError::Internal)?,
    })
}
pub(crate) async fn current(state: &AppState, id: Uuid) -> Result<ProjectRecord> {
    if id.is_nil() {
        return Err(bad("nil project ID"));
    }
    let row=sqlx::query("SELECT revision,binding,validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$1").bind(id).fetch_optional(&state.store.pool).await.map_err(db)?.ok_or_else(||AppError::NotFound("project not found".into()))?;
    record(row)
}
async fn inspect(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(id): Path<Uuid>,
) -> Result<Json<ProjectRecord>> {
    Ok(Json(current(&state, id).await?))
}
async fn history(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path((id, revision)): Path<(Uuid, u64)>,
) -> Result<Json<ProjectRecord>> {
    if id.is_nil() || revision == 0 || revision > i64::MAX as u64 {
        return Err(bad("invalid project revision"));
    }
    let v: Value = sqlx::query_scalar(
        "SELECT record FROM project_execution_binding_history WHERE project_id=$1 AND revision=$2",
    )
    .bind(id)
    .bind(revision as i64)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(db)?
    .ok_or_else(|| AppError::NotFound("project revision not found".into()))?;
    Ok(Json(
        serde_json::from_value(v).map_err(|_| AppError::Internal)?,
    ))
}
async fn list(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Query(page): Query<Page>,
) -> Result<Json<Value>> {
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) || page.after.is_some_and(|id| id.is_nil()) {
        return Err(bad("invalid project page"));
    }
    let rows=sqlx::query("SELECT project_id,revision,binding,validated_at_ms,binding_observation FROM project_execution_bindings WHERE ($1::uuid IS NULL OR project_id>$1) ORDER BY project_id LIMIT $2").bind(page.after).bind(limit as i64+1).fetch_all(&state.store.pool).await.map_err(db)?;
    let more = rows.len() > limit as usize;
    let items: Vec<_> = rows
        .into_iter()
        .take(limit as usize)
        .map(record)
        .collect::<Result<_>>()?;
    let next = if more {
        items.last().map(|r| r.binding.project_id)
    } else {
        None
    };
    let execution: std::collections::BTreeMap<_, _> = items
        .iter()
        .filter_map(|r| {
            state
                .config
                .profiled_execution
                .as_ref()?
                .projects
                .iter()
                .find(|p| p.project_id == r.binding.project_id)
                .map(|p| (r.binding.project_id, p.enabled))
        })
        .collect();
    Ok(Json(
        json!({"items":items,"next_cursor":next,"execution":execution}),
    ))
}

async fn existing_receipt(
    state: &AppState,
    actor: Uuid,
    operation: Uuid,
    digest: &str,
) -> Result<Option<Value>> {
    let row=sqlx::query("SELECT payload_digest,response FROM project_execution_operations WHERE actor_id=$1 AND operation_id=$2").bind(actor).bind(operation).fetch_optional(&state.store.pool).await.map_err(db)?;
    row.map(|r| {
        if r.get::<String, _>("payload_digest") == digest {
            Ok(r.get("response"))
        } else {
            Err(conflict("operation ID reused with changed content"))
        }
    })
    .transpose()
}
async fn put(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path(id): Path<Uuid>,
    Json(request): Json<PutBinding>,
) -> Result<Json<Value>> {
    put_request(&state, actor.id, id, request).await.map(Json)
}
/// The conflict of a binding that names an absent or revoked worker: it never validates.
pub(crate) const ABSENT_WORKER: &str = "project refers to absent or revoked worker identity";
/// Owner project-binding mutation core, shared by the route and D2 onboarding.
pub(crate) async fn put_binding(
    state: &AppState,
    actor: Uuid,
    operation_id: Uuid,
    expected_revision: u64,
    binding: ProjectBinding,
) -> Result<Value> {
    let id = binding.project_id;
    let request = PutBinding {
        operation_id,
        expected_revision,
        binding,
    };
    put_request(state, actor, id, request).await
}
async fn put_request(
    state: &AppState,
    actor: Uuid,
    id: Uuid,
    request: PutBinding,
) -> Result<Value> {
    let actor_id = actor;
    let state = state.clone();
    if request.operation_id.is_nil()
        || id != request.binding.project_id
        || request.expected_revision >= i64::MAX as u64
    {
        return Err(bad("invalid project mutation identity/revision"));
    }
    let digest = aq_profiles::digest(&json!({"kind":"put","project_id":id,"request":request}))
        .map_err(|_| bad("invalid project mutation"))?;
    if let Some(receipt) = existing_receipt(&state, actor_id, request.operation_id, &digest).await?
    {
        install_profiled_policies_after_commit(&state).await;
        return Ok(receipt);
    }
    request
        .binding
        .validate(&state.config.projects)
        .map_err(|_| bad("invalid project execution policy"))?;
    let mut nodes = request.binding.allowed_node_ids.clone();
    for group in &request.binding.allowed_worker_groups {
        nodes.extend(&state.config.projects.worker_groups[group]);
    }
    nodes.sort();
    nodes.dedup();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM nodes WHERE id=ANY($1) AND revoked_at IS NULL")
            .bind(&nodes)
            .fetch_one(&state.store.pool)
            .await
            .map_err(db)?;
    if count != nodes.len() as i64 {
        return Err(conflict(ABSENT_WORKER));
    }
    let observation = external::observe(&state, &request.binding).await;
    if observation.repository != ExternalStatus::Verified
        || observation.graph != ExternalStatus::Verified
    {
        return Err(conflict(&format!(
            "project external binding refused: repository={:?}, graph={:?}",
            observation.repository, observation.graph
        )));
    }
    if (state.clock)().timestamp_millis().max(1) as u64 >= observation.expires_at_ms {
        return Err(conflict("external validation expired before commit"));
    }
    let response = mutate(
        &state,
        actor_id,
        id,
        request.operation_id,
        request.expected_revision,
        &digest,
        Some((request.binding, observation)),
        false,
    )
    .await?;
    install_profiled_policies_after_commit(&state).await;
    Ok(response)
}
async fn disable(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path(id): Path<Uuid>,
    Json(request): Json<Disable>,
) -> Result<Json<Value>> {
    if id.is_nil()
        || request.operation_id.is_nil()
        || request.expected_revision == 0
        || request.expected_revision >= i64::MAX as u64
    {
        return Err(bad("invalid disable identity/revision"));
    }
    let digest = aq_profiles::digest(&json!({"kind":"disable","project_id":id,"request":request}))
        .map_err(|_| bad("invalid project mutation"))?;
    if let Some(receipt) = existing_receipt(&state, actor.id, request.operation_id, &digest).await?
    {
        install_profiled_policies_after_commit(&state).await;
        return Ok(Json(receipt));
    }
    let response = mutate(
        &state,
        actor.id,
        id,
        request.operation_id,
        request.expected_revision,
        &digest,
        None,
        true,
    )
    .await?;
    install_profiled_policies_after_commit(&state).await;
    Ok(Json(response))
}
/// The binding acknowledgment is already immutable. Installation is a separate
/// projection, after the original transaction has released every project lock.
/// Failure leaves scheduling ineligible without changing the accepted receipt.
async fn install_profiled_policies_after_commit(state: &AppState) {
    if state.config.profiled_execution.is_some()
        && crate::profiled_execution::policies::install(state)
            .await
            .is_err()
    {
        tracing::warn!("profiled policy installation pending after binding acknowledgment");
    }
}
#[allow(clippy::too_many_arguments)]
async fn mutate(
    state: &AppState,
    actor: Uuid,
    id: Uuid,
    op: Uuid,
    expected: u64,
    digest: &str,
    new: Option<(ProjectBinding, BindingObservation)>,
    disable: bool,
) -> Result<Value> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-project-binding-write-v1',0))")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    if let Some(row)=sqlx::query("SELECT payload_digest,response FROM project_execution_operations WHERE actor_id=$1 AND operation_id=$2").bind(actor).bind(op).fetch_optional(&mut *tx).await.map_err(db)? {
        if row.get::<String,_>("payload_digest")!=digest{return Err(conflict("operation ID reused with changed content"));}
        return Ok(row.get("response"));
    }
    let old=sqlx::query("SELECT revision,binding,validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$1 FOR UPDATE").bind(id).fetch_optional(&mut *tx).await.map_err(db)?;
    if old
        .as_ref()
        .map_or(0, |r| r.get::<i64, _>("revision") as u64)
        != expected
    {
        return Err(conflict("project revision changed"));
    }
    let fresh_observation = new.as_ref().map(|(_, o)| o.clone());
    let binding = if let Some((binding, _)) = new {
        // Spec 0014 §5.4: the defaults and the role notes name documents of
        // the active role snapshot. The lock is held to commit, so a delete
        // of one of them waits and then sees this binding.
        let (role, level) = (&binding.default_role, &binding.default_intelligence);
        if crate::role_settings::tables(state) {
            aq_role_settings::check_binding(&mut tx, role, level, &binding.role_notes)
                .await
                .map_err(crate::role_settings::error)?;
        }
        binding
    } else {
        let mut binding: ProjectBinding = serde_json::from_value(
            old.as_ref()
                .ok_or_else(|| AppError::NotFound("project not found".into()))?
                .get("binding"),
        )
        .map_err(|_| AppError::Internal)?;
        binding.enabled = false;
        binding
    };
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM project_execution_bindings")
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let operations: i64 = sqlx::query_scalar("SELECT count(*) FROM project_execution_operations")
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let history: i64 = sqlx::query_scalar("SELECT count(*) FROM project_execution_binding_history")
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if (old.is_none() && count >= 256) || operations >= 100_000 || history >= 100_000 {
        return Err(conflict("project policy retention quota reached"));
    }
    let now = (state.clock)().timestamp_millis().max(1);
    let observation = if let Some(observation) = fresh_observation {
        if now as u64 >= observation.expires_at_ms || (now as u64) < observation.observed_at_ms {
            return Err(conflict(
                "external validation expired during project lock wait",
            ));
        }
        observation
    } else {
        serde_json::from_value(old.as_ref().unwrap().get("binding_observation"))
            .map_err(|_| AppError::Internal)?
    };
    let validated = observation.observed_at_ms as i64;
    let record = ProjectRecord {
        revision: expected + 1,
        binding,
        validated_at_ms: validated as u64,
        validation: observation.clone(),
    };
    let value = serde_json::to_value(&record).map_err(|_| AppError::Internal)?;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,$2,$3,$4,$5) ON CONFLICT(project_id) DO UPDATE SET revision=excluded.revision,binding=excluded.binding,validated_at_ms=excluded.validated_at_ms,binding_observation=excluded.binding_observation").bind(id).bind(record.revision as i64).bind(serde_json::to_value(&record.binding).unwrap()).bind(validated).bind(serde_json::to_value(&observation).unwrap()).execute(&mut *tx).await.map_err(db)?;
    sqlx::query("INSERT INTO project_execution_binding_history(project_id,revision,record,actor_id,created_at_ms) VALUES($1,$2,$3,$4,$5)").bind(id).bind(record.revision as i64).bind(&value).bind(actor).bind(now).execute(&mut *tx).await.map_err(db)?;
    let response = json!({"operation_id":op,"record":record,"disposition":if disable{"disabled"}else{"bound"}});
    sqlx::query("INSERT INTO project_execution_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)").bind(actor).bind(op).bind(digest).bind(&response).execute(&mut *tx).await.map_err(db)?;
    let commit_now = (state.clock)().timestamp_millis().max(1) as u64;
    if !disable
        && (commit_now >= observation.expires_at_ms || commit_now < observation.observed_at_ms)
    {
        return Err(conflict(
            "external validation expired before project commit",
        ));
    }
    tx.commit().await.map_err(db)?;
    Ok(response)
}
/// What the bindings name in the role store; it refuses to delete these.
pub(crate) fn role_references(
    conn: &mut sqlx::PgConnection,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = aq_role_settings::Result<Vec<aq_role_settings::Reference>>>
            + Send
            + '_,
    >,
> {
    use aq_profiles::roles::Kind;
    Box::pin(async move {
        let rows: Vec<Value> = sqlx::query_scalar(
            "SELECT binding FROM project_execution_bindings ORDER BY project_id",
        )
        .fetch_all(conn)
        .await?;
        let mut out = Vec::new();
        for row in rows {
            let b: ProjectBinding =
                serde_json::from_value(row).map_err(|_| aq_role_settings::Error::Internal)?;
            let mut add = |kind, id: &String, what: &str| {
                out.push(aq_role_settings::Reference {
                    kind,
                    id: id.clone(),
                    by: format!("{what} project `{}`", b.name),
                })
            };
            add(Kind::Role, &b.default_role, "the default role of");
            let level = &b.default_intelligence;
            add(
                Kind::Intelligence,
                level,
                "the default intelligence profile of",
            );
            for role in b.role_notes.keys() {
                add(Kind::Role, role, "given notes by");
            }
        }
        Ok(out)
    })
}
async fn eligibility(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(id): Path<Uuid>,
    Json(request): Json<EligibilityRequest>,
) -> Result<Json<Eligibility>> {
    Ok(Json(check_eligibility(&state, id, &request).await?))
}
/// Admission seam for AQ-20/AQ-08. Consumers must call again immediately before
/// issuing a new profiled offer; the returned snapshot is never an execution grant.
pub async fn check_eligibility(
    state: &AppState,
    id: Uuid,
    request: &EligibilityRequest,
) -> Result<Eligibility> {
    check_eligibility_current(state, id, request, false, false).await
}
/// For an alternative justified by the scheduler's frozen workspace scope.
pub(crate) async fn check_workspace_eligibility(
    state: &AppState,
    id: Uuid,
    request: &EligibilityRequest,
    workspace_alternative: bool,
) -> Result<Eligibility> {
    check_eligibility_current(state, id, request, false, workspace_alternative).await
}
/// Fresh current binding facts for continuing a pinned execution. This chooses
/// no model and does not reuse an original admission observation or decision.
/// The attempt stays on the role snapshot it launched with (spec 0014 §7.1):
/// its role, level and execution profile are not looked up again.
pub(crate) async fn check_execution_eligibility(
    state: &AppState,
    id: Uuid,
    request: &EligibilityRequest,
) -> Result<Eligibility> {
    check_eligibility_current(state, id, request, true, false).await
}
/// A new launch names documents of the active role snapshot: the role exists
/// and is enabled, the level exists, the execution profile is the role's and
/// runs in the named environment.
async fn launch_reason(
    state: &AppState,
    request: &EligibilityRequest,
    workspace_alternative: bool,
) -> Result<Option<&'static str>> {
    let Some(snapshot) = crate::role_settings::active(&state.store.pool).await? else {
        return Ok(Some("role_snapshot_absent"));
    };
    if snapshot.digest() != request.snapshot_digest {
        return Ok(Some("role_snapshot_changed"));
    }
    let Ok(launch) = snapshot.resolve(&request.role, &request.intelligence_profile) else {
        return Ok(Some("launch_documents_missing"));
    };
    #[cfg(not(feature = "project_environments"))]
    let elsewhere = launch.execution.environment != request.environment;
    // Spec 0017 §5.3: only a pinned environment is fixed by the profile.
    #[cfg(feature = "project_environments")]
    let elsewhere =
        (launch.execution.environment.as_ref()).is_some_and(|e| *e != request.environment);
    Ok(if !launch.role.enabled {
        Some("role_disabled")
    } else if launch.execution.id != request.execution_profile
        || (elsewhere && !workspace_alternative)
    {
        Some("profile_combination_denied")
    } else {
        None
    })
}
async fn check_eligibility_current(
    state: &AppState,
    id: Uuid,
    request: &EligibilityRequest,
    execution_read: bool,
    workspace_alternative: bool,
) -> Result<Eligibility> {
    if request.expected_project_revision == 0 || request.node_id.is_nil() {
        return Err(bad("invalid eligibility identity/revision"));
    }
    let record = current(state, id).await?;
    let initial_node_live = node_current(state, request.node_id).await?;
    let early = local_reasons(&record, request, &state.config.projects, initial_node_live);
    if !early.is_empty() {
        return Ok(Eligibility {
            eligible: false,
            reasons: early,
            frozen_binding: None,
        });
    }
    let observation = if execution_read {
        external::observe_execution(state, &record.binding).await
    } else {
        external::observe(state, &record.binding).await
    };
    // Read local admission-sensitive revisions after external I/O.
    let latest = current(state, id).await?;
    let node_live = node_current(state, request.node_id).await?;
    let mut result = decide(
        &latest,
        request,
        &state.config.projects,
        EligibilityEvidence::new(
            &observation,
            node_live,
            (state.clock)().timestamp_millis().max(1) as u64,
        ),
    );
    if latest.revision != record.revision {
        result
            .reasons
            .push("project_changed_during_external_validation".into());
    }
    if !execution_read
        && let Some(reason) = launch_reason(state, request, workspace_alternative).await?
    {
        result.reasons.push(reason.into());
    }
    if result.reasons.is_empty() {
        let mut tx = state.store.pool.begin().await.map_err(db)?;
        if let Err(error) =
            fence_for_offer(state, &mut tx, result.frozen_binding.as_ref().unwrap()).await
        {
            match error {
                AppError::Conflict(reason) => result.reasons.push(reason),
                other => return Err(other),
            }
        }
        tx.commit().await.map_err(db)?;
    }
    let final_now = (state.clock)().timestamp_millis().max(1) as u64;
    if final_now < observation.observed_at_ms || final_now >= observation.expires_at_ms {
        result.reasons.push("binding_observation_stale".into());
    }
    if !result.reasons.is_empty() {
        result.eligible = false;
        result.frozen_binding = None;
    }
    Ok(result)
}
async fn node_current(state: &AppState, node: Uuid) -> Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND revoked_at IS NULL AND last_seen_at>=$2)").bind(node).bind((state.clock)()-chrono::Duration::seconds(state.config.workers.offline_after_secs as i64)).fetch_one(&state.store.pool).await.map_err(db)
}
/// Final local fence for a future real profiled-offer transaction. This must run
/// INSIDE the same transaction as offer insertion, with these locks retained.
/// The caller still owns current authority/budgets and must check time again at
/// its final effect/commit boundary. This helper never inserts or grants an offer.
pub(crate) async fn fence_for_offer(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    frozen: &FrozenProjectBinding,
) -> Result<()> {
    let node = sqlx::query("SELECT revoked_at,last_seen_at FROM nodes WHERE id=$1 FOR KEY SHARE")
        .bind(frozen.node_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(|| conflict("node_not_eligible"))?;
    let row=sqlx::query("SELECT revision,binding,validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$1 FOR SHARE").bind(frozen.project.binding.project_id).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(||conflict("project_absent"))?;
    let record = record(row)?;
    let request = EligibilityRequest {
        expected_project_revision: frozen.project.revision,
        node_id: frozen.node_id,
        environment: frozen.environment.clone(),
        role: frozen.role.clone(),
        intelligence_profile: frozen.intelligence_profile.clone(),
        execution_profile: frozen.execution_profile.clone(),
        snapshot_digest: frozen.snapshot_digest.clone(),
    };
    let now = (state.clock)();
    let live = node
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("revoked_at")
        .is_none()
        && node
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_seen_at")
            .is_some_and(|at| {
                at >= now
                    - chrono::Duration::seconds(state.config.workers.offline_after_secs as i64)
            });
    let result = decide(
        &record,
        &request,
        &state.config.projects,
        EligibilityEvidence::new(
            &frozen.binding_observation,
            live,
            now.timestamp_millis().max(1) as u64,
        ),
    );
    if aq_profiles::digest(&state.config.projects).map_err(|_| AppError::Internal)?
        != frozen.worker_membership_digest
    {
        return Err(conflict("worker_group_configuration_changed"));
    }
    if !result.eligible {
        return Err(conflict(
            result
                .reasons
                .first()
                .map(String::as_str)
                .unwrap_or("admission_changed"),
        ));
    }
    Ok(())
}
