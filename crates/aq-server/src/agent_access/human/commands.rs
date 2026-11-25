//! Typed human task creation. The journal is immutable intent, never task truth.
pub(crate) mod action;
mod dispatch;
mod journal;
pub(super) mod launch;
use super::*;
use axum::{body::Bytes, http::StatusCode};
use sha2::Digest as _;
const CONTRACT: &str = "human-task-create-1";
const MAX_ATTEMPTS: i32 = 8;
const BUDGET: i64 = 300000;
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}
fn db(_: sqlx::Error) -> AppError {
    unavailable()
}
pub(crate) use aq_human_work::task_graph::single::Create;
use aq_human_work::task_graph::single::TaskState;
fn parse(bytes: &[u8]) -> Result<Create> {
    if bytes.is_empty() || bytes.len() > 65536 {
        return Err(invalid());
    }
    let super::super::contract::StrictValue(raw) =
        serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let v: Create = serde_json::from_value(raw).map_err(|_| invalid())?;
    if !v.valid() {
        return Err(invalid());
    }
    Ok(v)
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationState {
    Pending,
    Unknown,
    Confirmed,
    Conflict,
    Rejected,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Effect {
    NotDispatched,
    Possible,
    Confirmed,
    NotApplied,
}
use aq_human_work::task_graph::creation_receipt::{Creation, Receipt};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    allowed: bool,
    reason: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Status {
    contract: String,
    operation_id: Uuid,
    project_id: Uuid,
    actor_id: Uuid,
    task_id: Uuid,
    request_digest: String,
    state: OperationState,
    effect: Effect,
    receipt: Option<Receipt>,
    recovery: Recovery,
    created_at_ms: u64,
}
impl Status {
    fn initial(admission: &journal::Admission<'_>, actor: Uuid, digest: String, now: i64) -> Self {
        Self {
            contract: CONTRACT.into(),
            operation_id: admission.request.operation_id,
            project_id: admission.record.binding.project_id,
            actor_id: actor,
            task_id: admission.request.operation_id,
            request_digest: digest,
            state: OperationState::Pending,
            effect: Effect::NotDispatched,
            receipt: None,
            recovery: Recovery {
                allowed: true,
                reason: None,
            },
            created_at_ms: now as u64,
        }
    }
    fn terminal(&self) -> bool {
        matches!(
            self.state,
            OperationState::Confirmed | OperationState::Conflict | OperationState::Rejected
        )
    }
}
struct Operation {
    id: Uuid,
    actor: Uuid,
    project: Uuid,
    revision: i64,
    binding: Value,
    workspace: Uuid,
    request: Vec<u8>,
    request_digest: String,
    payload: Vec<u8>,
    payload_digest: String,
    service: Value,
    status: Status,
    raw_receipt: Option<Value>,
    created: i64,
    expires: i64,
    count: i32,
    lease: Option<Uuid>,
    lease_until: Option<i64>,
    stop: Option<String>,
}
impl Operation {
    fn decode(row: sqlx::postgres::PgRow) -> Result<Self> {
        let status: Status =
            serde_json::from_value(row.get("status")).map_err(|_| unavailable())?;
        let value = Self {
            id: row.get("operation_id"),
            actor: row.get("actor_id"),
            project: row.get("project_id"),
            revision: row.get("project_revision"),
            binding: row.get("project_binding"),
            workspace: row.get("workspace_id"),
            request: row.get("request_bytes"),
            request_digest: row.get("request_digest"),
            payload: row.get("payload_bytes"),
            payload_digest: row.get("payload_digest"),
            service: row.get("service_identity"),
            status,
            raw_receipt: row.get("raw_receipt"),
            created: row.get("created_at_ms"),
            expires: row.get("expires_at_ms"),
            count: row.get("dispatch_count"),
            lease: row.get("lease_id"),
            lease_until: row.get("lease_expires_at_ms"),
            stop: row.get("stop_reason"),
        };
        let request = parse(&value.request)?;
        if !value.matches_request(&request)? || !value.matches_status() {
            return Err(unavailable());
        }
        value.verify_receipt()?;
        Ok(value)
    }
    fn matches_intent(&self, project: Uuid, bytes: &[u8]) -> bool {
        self.project == project && self.request == bytes
    }
    fn matches_request(&self, request: &Create) -> Result<bool> {
        Ok(self.id == request.operation_id
            && self.revision == request.revision().ok_or_else(invalid)?
            && hash(&self.request) == self.request_digest
            && hash(&self.payload) == self.payload_digest)
    }
    fn matches_status(&self) -> bool {
        let status = &self.status;
        let same_identity = self.id == status.task_id
            && self.id == status.operation_id
            && self.actor == status.actor_id
            && self.project == status.project_id;
        same_identity
            && status.request_digest == self.request_digest
            && status.contract == CONTRACT
            && status.created_at_ms == self.created as u64
    }
    fn verify_receipt(&self) -> Result<()> {
        let confirmed = self.status.state == OperationState::Confirmed;
        match &self.raw_receipt {
            Some(raw) => {
                let proof = dispatch::receipt(self, raw)?;
                let confirmed_receipt = confirmed && self.status.effect == Effect::Confirmed;
                if confirmed_receipt && self.status.receipt.as_ref() == Some(&proof) {
                    return Ok(());
                }
            }
            None if !confirmed && self.status.receipt.is_none() => return Ok(()),
            None => {}
        }
        Err(unavailable())
    }
}
fn outcome(status: Status) -> Result<Response> {
    let code = match status.state {
        OperationState::Confirmed => StatusCode::OK,
        OperationState::Conflict => StatusCode::CONFLICT,
        OperationState::Rejected => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::ACCEPTED,
    };
    Ok((code, Json(status)).into_response())
}
type OwnerRoute = (State<AppState>, Extension<Arc<ReadRuntime>>, RequireOwner);
pub(super) async fn create(
    (State(state), Extension(r), RequireOwner(actor)): OwnerRoute,
    Path(project): Path<Uuid>,
    bytes: Bytes,
) -> Result<Response> {
    let request = parse(&bytes)?;
    if let Some(old) = journal::load(&r, request.operation_id).await? {
        journal::owns(&old, &actor)?;
        if !old.matches_intent(project, &bytes) {
            return Err(conflict());
        }
    }
    // This route's action errors carry fixed text; an unknown role says so.
    if let Some(role) = &request.role {
        crate::role_settings::named(&state, &[role])
            .await?
            .map_err(|unknown| AppError::BadRequest(unknown.to_string()))?;
    }
    let adapter = action::TaskCreate {
        runtime: r.clone(),
        original: Some(bytes.clone()),
    };
    let action = action::request(project, &request);
    let mut registry = crate::dashboard_api::actions::Registry::default();
    registry
        .register(adapter)
        .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    crate::dashboard_api::actions::submit_legacy(&state, &registry, &actor, action)
        .await
        .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    let op = journal::load(&r, request.operation_id)
        .await?
        .ok_or_else(unavailable)?;
    outcome(journal::status(&state, &r, &actor, &op).await?)
}
struct Owner<'a> {
    state: &'a AppState,
    runtime: &'a ReadRuntime,
    actor: &'a User,
}
impl Owner<'_> {
    /// Parent authority includes project membership, even for a shared workspace.
    async fn authorize_parent(&self, project: Uuid, parent: Uuid) -> Result<()> {
        let Self {
            state,
            runtime: r,
            actor,
        } = *self;
        let a = access(state, r, actor.clone(), project).await?;
        let node = super::graph_actions::current(
            runtime(state)?,
            a.record.binding.graph_workspace_id,
            parent,
            "task",
        )
        .await?;
        if node["metadata"]["aq:project_id"] != json!(project) {
            return Err(AppError::Forbidden(
                "parent belongs to another project".into(),
            ));
        }
        Ok(())
    }
    async fn create(&self, project: Uuid, bytes: Bytes, target: Option<Uuid>) -> Result<Status> {
        let Self {
            state,
            runtime: r,
            actor,
        } = *self;
        let request = parse(&bytes)?;
        if project.is_nil() {
            return Err(invalid());
        }
        if let Some(old) = journal::load(r, request.operation_id).await? {
            journal::owns(&old, actor)?;
            if !old.matches_intent(project, &bytes) || launch::target(&old.payload) != target {
                return Err(conflict());
            }
            return journal::status(state, r, actor, &old).await;
        }
        let a = access(state, r, actor.clone(), project).await?;
        let rt = runtime(state)?;
        let service =
            super::super::mutations::service_identity(rt, a.record.binding.graph_workspace_id)
                .map_err(|_| unavailable())?;
        if let Some(parent) = request.parent_id {
            self.authorize_parent(project, parent).await?;
        }
        let payload = request.mutation(project, actor.id, target);
        if !contract::upstream_valid("MutationRequest", &payload) {
            return Err(unavailable());
        }
        let payload = super::super::canonical(&payload).map_err(|_| unavailable())?;
        journal::Admission {
            record: &a.record,
            request: &request,
            bytes: &bytes,
            payload: &payload,
            service: &service,
        }
        .admit(self)
        .await?;
        dispatch::run(state, r, actor, request.operation_id).await
    }
}
pub(super) async fn lookup(
    (State(state), Extension(r), RequireOwner(actor)): OwnerRoute,
    Path(id): Path<Uuid>,
) -> Result<Json<Status>> {
    let op = journal::load(&r, id).await?.ok_or_else(|| {
        AppError::NotFound("operation not found; prior submission may still be in flight".into())
    })?;
    journal::owns(&op, &actor)?;
    owner(&r, &actor).await?;
    Ok(Json(journal::status(&state, &r, &actor, &op).await?))
}
pub(super) async fn recover(
    (State(state), Extension(r), RequireOwner(actor)): OwnerRoute,
    Path(id): Path<Uuid>,
    bytes: Bytes,
) -> Result<Response> {
    let super::super::contract::StrictValue(body) =
        serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if body != json!({}) {
        return Err(invalid());
    }
    outcome(dispatch::run(&state, &r, &actor, id).await?)
}
