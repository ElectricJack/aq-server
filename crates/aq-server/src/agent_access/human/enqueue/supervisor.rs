//! F1b credentials authorize only creation, with a session-private journal.
use super::super::graph_actions::GraphAction;
use super::*;
use crate::agent_access::supervisor_conversation::reply::{
    SupervisorPrincipal, authenticate_in_transaction,
};
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Outcome, Registry},
    contract::Scope,
    errors::ApiError,
};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
struct SingleGraph(GraphCreate);
impl GraphPayload for SingleGraph {
    const KIND: &'static str = "task.create";
    const GRANT: &'static str = aq_agent_grants::op::TASK_CREATE_SUBTASK;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::ParentTask {
        field: "parent_id",
        unparented: aq_agent_grants::op::TASK_CREATE_PROJECT,
    };
    const ENTITY: &'static str = "project";
    fn operation_id(&self) -> Uuid {
        self.0.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.0.project_id
    }
    fn expected(&self) -> &str {
        &self.0.expected_version
    }
    fn valid(&self) -> bool {
        self.0.valid()
    }
    fn tasks(&self) -> Vec<Uuid> {
        self.0.tasks()
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        self.0.operations(c)
    }
}
struct Single(GraphAction<SingleGraph>);
fn single(request: &ActionRequest, p: &super::super::commands::Create) -> ApiResult<SingleGraph> {
    let project = request
        .subject
        .id
        .parse()
        .map_err(|_| ApiError::invalid())?;
    Ok(SingleGraph(GraphCreate {
        operation_id: p.operation_id,
        project_id: project,
        expected_version: p.expected_project_revision.clone(),
        tasks: vec![Task {
            id: p.operation_id,
            title: p.title.clone(),
            body: p.body.clone(),
            acceptance_criteria: p.acceptance_criteria.clone(),
            labels: p.labels.clone(),
            task_state: serde_json::from_value(json!(p.task_state))
                .map_err(|_| ApiError::invalid())?,
            parent_id: p.parent_id,
            priority: p.priority,
            profile_hint: p.profile_hint.clone(),
            intelligence_hint: p.intelligence_hint.clone(),
            task_type: p.task_type.clone(),
        }],
        edges: vec![],
    }))
}
#[async_trait]
impl ActionKind for Single {
    type Payload = super::super::commands::Create;
    const KIND: &'static str = "task.create";
    const GRANT: &'static str = aq_agent_grants::op::TASK_CREATE_SUBTASK;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::ParentTask {
        field: "parent_id",
        unparented: aq_agent_grants::op::TASK_CREATE_PROJECT,
    };
    async fn authorize(
        &self,
        s: &AppState,
        u: &User,
        r: &ActionRequest,
        p: &Self::Payload,
    ) -> ApiResult<Scope> {
        self.0.authorize(s, u, r, &single(r, p)?).await
    }
    async fn authorize_lookup(
        &self,
        s: &AppState,
        u: &User,
        scope: &Scope,
        op: Option<&actions::Operation>,
    ) -> ApiResult<()> {
        self.0.authorize_lookup(s, u, scope, op).await
    }
    async fn execute(
        &self,
        s: &AppState,
        u: &User,
        r: &ActionRequest,
        p: Self::Payload,
    ) -> Outcome {
        match single(r, &p) {
            Ok(p) => self.0.execute(s, u, r, p).await,
            Err(_) => Outcome::unknown(),
        }
    }
    async fn reconcile(
        &self,
        s: &AppState,
        u: &User,
        op: &actions::Operation,
    ) -> ApiResult<Option<Outcome>> {
        self.0.reconcile(s, u, op).await
    }
}
fn registry(state: &AppState, p: SupervisorPrincipal) -> ApiResult<Registry> {
    let mut r = Registry::default();
    r.register(GraphAction::<GraphCreate>::for_supervisor(state, p.clone()))?;
    r.register(Single(GraphAction::<SingleGraph>::for_supervisor(
        state,
        p.clone(),
    )))?;
    use super::super::graph_actions::{
        DependencyAdd, DependencyRemove, TaskEdit, TaskReprioritize,
    };
    r.register(GraphAction::<TaskEdit>::for_supervisor(state, p.clone()))?;
    r.register(GraphAction::<TaskReprioritize>::for_supervisor(
        state,
        p.clone(),
    ))?;
    r.register(GraphAction::<DependencyAdd>::for_supervisor(
        state,
        p.clone(),
    ))?;
    r.register(GraphAction::<DependencyRemove>::for_supervisor(state, p))?;
    Ok(r)
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/actions", post(create))
        .route("/api/v1/operations/{id}", get(lookup))
        .layer(axum::extract::DefaultBodyLimit::max(65536))
}
fn bearer(headers: &HeaderMap) -> ApiResult<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::for_status(StatusCode::UNAUTHORIZED))
}
async fn principal(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    headers: &HeaderMap,
) -> ApiResult<SupervisorPrincipal> {
    let p = authenticate_in_transaction(
        &state.store.pool,
        tx,
        bearer(headers)?,
        "enqueue",
        (state.clock)()
            .timestamp_millis()
            .try_into()
            .map_err(|_| ApiError::unavailable())?,
    )
    .await
    .map_err(ApiError::from)?;
    let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
        .bind(p.session_id)
        .fetch_one(&mut **tx)
        .await?;
    if collision {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    Ok(p)
}
fn journal_user(p: &SupervisorPrincipal) -> User {
    // This is only an opaque journal identity. No Owner adapter is installed.
    User {
        id: p.session_id,
        email: String::new(),
        role: crate::store::Role::Viewer,
        issuer: None,
        subject: None,
    }
}
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: std::result::Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> ApiResult<(StatusCode, Json<actions::OperationResponse>)> {
    let bytes = bytes.map_err(|e| ApiError::for_status(e.status()))?;
    let mut tx = state.store.pool.begin().await?;
    // Keep lifecycle and credential locks through protected admission/dispatch.
    let p = principal(&state, &mut tx, &headers).await?;
    let user = journal_user(&p);
    let request: ActionRequest = serde_json::from_value(
        crate::agent_access::scheduling::parse::parse(&bytes).map_err(|_| ApiError::invalid())?,
    )
    .map_err(|_| ApiError::invalid())?;
    let op = actions::submit_scoped(&state, &registry(&state, p)?, &user, request).await?;
    let response = actions::response(&state, op);
    tx.commit().await?;
    Ok((actions::operation_status(&response.data), Json(response)))
}
async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: std::result::Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> ApiResult<Json<actions::OperationResponse>> {
    let mut tx = state.store.pool.begin().await?;
    let p = principal(&state, &mut tx, &headers).await?;
    let user = journal_user(&p);
    let Path(id) = path.map_err(|_| ApiError::invalid())?;
    let op = actions::lookup_scoped(&state, &registry(&state, p)?, &user, id).await?;
    let response = actions::response(&state, op);
    tx.commit().await?;
    Ok(Json(response))
}
/// An agent's single-task create (spec 0006 phase 2), journaled under the
/// attempt's own identity. Its grant, scope and quota are already checked.
pub(crate) async fn agent_create(
    state: &AppState,
    ids: aq_agent_grants::AttemptIds,
    request: ActionRequest,
) -> std::result::Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let (attempt, task, project) = (ids.attempt_id, ids.task_id, ids.project_id);
    let run = async {
        let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
            .bind(attempt)
            .fetch_one(&state.store.pool)
            .await?;
        if collision {
            return Err(ApiError::for_status(StatusCode::FORBIDDEN));
        }
        let mut r = Registry::default();
        r.register(Single(GraphAction::<SingleGraph>::for_agent(
            state, attempt, task, project,
        )))?;
        let user = User {
            id: attempt,
            email: String::new(),
            role: crate::store::Role::Viewer,
            issuer: None,
            subject: None,
        };
        let op = actions::submit_scoped(state, &r, &user, request).await?;
        ApiResult::Ok(actions::response(state, op))
    };
    run.await
        .map(|response| (actions::operation_status(&response.data), Json(response)).into_response())
}
