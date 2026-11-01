//! Typed human task creation. The journal is immutable intent, never task truth.
mod dispatch;
mod journal;
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
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    operation_id: Uuid,
    expected_project_revision: String,
    title: String,
    body: String,
    acceptance_criteria: String,
    labels: Vec<String>,
    task_state: TaskState,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskState {
    Draft,
    Active,
}
impl Create {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > 65536 {
            return Err(invalid());
        }
        let super::super::contract::StrictValue(raw) =
            serde_json::from_slice(bytes).map_err(|_| invalid())?;
        let v: Self = serde_json::from_value(raw).map_err(|_| invalid())?;
        if v.operation_id.is_nil()
            || v.revision().is_err()
            || v.title.trim().is_empty()
            || v.title.chars().count() > 512
            || v.title.chars().any(char::is_control)
            || v.body.contains('\0')
            || v.acceptance_criteria.contains('\0')
            || v.body.len() > 32768
            || v.acceptance_criteria.len() > 8192
            || v.labels.len() > 16
            || v.labels
                .iter()
                .any(|s| s.trim().is_empty() || s.len() > 128 || s.chars().any(char::is_control))
            || v.labels
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != v.labels.len()
        {
            return Err(invalid());
        }
        Ok(v)
    }
    fn revision(&self) -> Result<i64> {
        let s = &self.expected_project_revision;
        if s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        s.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(invalid)
    }
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
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Receipt {
    workspace_id: Uuid,
    change_set_id: Uuid,
    task_id: Uuid,
    version: String,
    content_revision: String,
}
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
    fn initial(request: &Create, project: Uuid, actor: Uuid, digest: String, now: i64) -> Self {
        Self {
            contract: CONTRACT.into(),
            operation_id: request.operation_id,
            project_id: project,
            actor_id: actor,
            task_id: request.operation_id,
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
        let request = Create::parse(&value.request)?;
        if value.id != request.operation_id
            || value.id != value.status.task_id
            || value.id != value.status.operation_id
            || value.actor != value.status.actor_id
            || value.project != value.status.project_id
            || value.revision != request.revision()?
            || hash(&value.request) != value.request_digest
            || value.status.request_digest != value.request_digest
            || hash(&value.payload) != value.payload_digest
            || value.status.contract != CONTRACT
            || value.status.created_at_ms != value.created as u64
        {
            return Err(unavailable());
        }
        if let Some(raw) = &value.raw_receipt {
            let r = dispatch::receipt(&value, raw)?;
            if value.status.state != OperationState::Confirmed
                || value.status.effect != Effect::Confirmed
                || value.status.receipt.as_ref() != Some(&r)
            {
                return Err(unavailable());
            }
        } else if value.status.state == OperationState::Confirmed || value.status.receipt.is_some()
        {
            return Err(unavailable());
        }
        Ok(value)
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
pub(super) async fn create(
    State(state): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(actor): RequireOwner,
    Path(project): Path<Uuid>,
    bytes: Bytes,
) -> Result<Response> {
    let request = Create::parse(&bytes)?;
    if project.is_nil() {
        return Err(invalid());
    }
    if let Some(old) = journal::load(&r, request.operation_id).await? {
        journal::owns(&old, &actor)?;
        if old.project != project || old.request != bytes.as_ref() {
            return Err(conflict());
        }
        return outcome(dispatch::run(&state, &r, &actor, old.id).await?);
    }
    let a = access(&state, &r, actor.clone(), project).await?;
    let rt = runtime(&state)?;
    let service =
        super::super::mutations::service_identity(rt, a.record.binding.graph_workspace_id)
            .map_err(|_| unavailable())?;
    let payload = json!({"idempotency_key":format!("aq-human-task-create:{}",request.operation_id),"reason":format!("AQ human task create {}",request.operation_id),"operations":[{"op":"create_node","id":request.operation_id,"kind":"task","title":request.title,"body":request.body,"acceptance_criteria":request.acceptance_criteria,"labels":request.labels,"task_state":request.task_state,"metadata":{"aq:human_actor_id":actor.id,"aq:human_operation_id":request.operation_id,"aq:project_id":project}}]});
    if !contract::upstream_valid("MutationRequest", &payload) {
        return Err(unavailable());
    }
    let payload = super::super::canonical(&payload).map_err(|_| unavailable())?;
    journal::admit(
        &state, &r, &actor, &a.record, &request, &bytes, &payload, &service,
    )
    .await?;
    outcome(dispatch::run(&state, &r, &actor, request.operation_id).await?)
}
pub(super) async fn lookup(
    State(state): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(actor): RequireOwner,
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
    State(state): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(actor): RequireOwner,
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
