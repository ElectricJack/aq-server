//! Owner launch uses the retained Graph intent and normal native worker admission.
use super::*;

pub(super) fn target(payload: &[u8]) -> Option<Uuid> {
    serde_json::from_slice::<Value>(payload).ok()?["operations"][0]["metadata"]["aq:requested_node_id"]
        .as_str().and_then(|id| id.parse().ok())
}
async fn worker(state: &AppState, node: Uuid) -> Result<()> {
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND revoked_at IS NULL AND last_seen_at > $2)")
        .bind(node).bind((state.clock)() - chrono::Duration::seconds(90))
        .fetch_one(&state.store.pool).await.map_err(db)?;
    if !live {
        return Err(AppError::Conflict(
            "This worker is offline. Select an online worker.".into(),
        ));
    }
    Ok(())
}
pub(in crate::agent_access::human) async fn projects(
    State(state): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(actor): RequireOwner,
    Path(node): Path<Uuid>,
) -> Result<Json<Value>> {
    owner(&r, &actor).await?;
    worker(&state, node).await?;
    let mut projects = Vec::new();
    for id in state
        .config
        .profiled_execution
        .as_ref()
        .map(|c| c.enabled_projects())
        .unwrap_or_default()
    {
        if !crate::project_bindings::current(&state, id)
            .await?
            .binding
            .enabled
        {
            continue;
        }
        let a = access(&state, &r, actor.clone(), id).await?;
        projects.push(json!({"id":id,"revision":a.record.revision.to_string()}));
    }
    Ok(Json(json!({"projects":projects})))
}
pub(in crate::agent_access::human) async fn create(
    State(state): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(actor): RequireOwner,
    Path((project, node)): Path<(Uuid, Uuid)>,
    bytes: Bytes,
) -> Result<Response> {
    owner(&r, &actor).await?;
    let request = Create::parse(&bytes)?;
    if !matches!(request.task_state, TaskState::Active) {
        return Err(invalid());
    }
    // Recovery stays available if the node later goes offline; no new intent is admitted.
    if journal::load(&r, request.operation_id).await?.is_none() {
        worker(&state, node).await?;
        if !crate::project_bindings::current(&state, project)
            .await?
            .binding
            .enabled
        {
            return Err(AppError::Conflict(
                "This project is stopped. Start it in Settings → Projects first.".into(),
            ));
        }
        if !state
            .config
            .profiled_execution
            .as_ref()
            .is_some_and(|c| c.enabled_projects().contains(&project))
        {
            return Err(AppError::Conflict(
                "This project is paused or has no worker execution configured.".into(),
            ));
        }
    }
    outcome(create_on_node(&state, &r, &actor, project, bytes, Some(node)).await?)
}
