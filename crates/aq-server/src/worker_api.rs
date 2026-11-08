//! Machine routes: enrollment, heartbeat and probe. Bearer auth only; no
//! cookies, so no CSRF guard. Mounted on the worker listener only.

use aq_protocol::{
    CompatibilityError, EnrollRequest, EnrollResponse, HeartbeatRequest, HeartbeatResponse,
    ProbeRequest, ProbeResponse, SUPPORTED_WORKER_PROTOCOLS, UNSUPPORTED_PROTOCOL,
};
use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, header};
use axum::routing::post;

use crate::admin_api::ApiJson;
use crate::error::AppError;
use crate::store::{NewNode, Node};
use crate::tokens::{new_node_credential, verifier};
use crate::{AppState, version_info};

const MAX_BODY_BYTES: usize = 16 * 1024;

pub fn worker_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/enroll", post(enroll))
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/v1/worker/heartbeat", post(heartbeat))
                .route("/v1/worker/probe", post(probe))
                .route("/v1/worker/machine", post(machine))
                .with_state(state.control_view()),
        )
        .merge(crate::machine_api::machine_routes(state.clone()))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

fn check_protocol(version: u32) -> Result<(), AppError> {
    let (min, max) = SUPPORTED_WORKER_PROTOCOLS;
    if (min..=max).contains(&version) {
        return Ok(());
    }
    Err(AppError::UnsupportedProtocol(CompatibilityError {
        error: UNSUPPORTED_PROTOCOL.to_string(),
        message: format!(
            "worker protocol {version} is not supported; this server accepts {min}\u{2013}{max}. \
             Install a compatible aq-worker release."
        ),
        supported_min: min,
        supported_max: max,
    }))
}

fn bad(msg: &str) -> AppError {
    AppError::BadRequest(msg.to_string())
}

/// Opaque server authentication evidence, never deserialized from a request.
/// A snapshot alone is insufficient after a wait: call `lock_current_node` in
/// the transaction that authorizes effects, before acquiring attempt locks.
#[derive(Clone)]
pub struct AuthenticatedNode {
    node: Node,
    verifier: zeroize::Zeroizing<String>,
    delegated: Option<crate::delegated_permits::runtime::Authentication>,
    pub(crate) issuance: Option<aq_admission_sandbox::sandbox::PermitIssuanceRequest>,
    pub(crate) upgrade: Option<aq_admission_sandbox::sandbox::PermitUpgradeRequest>,
}
impl AuthenticatedNode {
    pub(crate) fn from_delegated(
        node: Node,
        evidence: crate::delegated_permits::runtime::Authentication,
    ) -> Self {
        Self {
            node,
            verifier: zeroize::Zeroizing::new(String::new()),
            delegated: Some(evidence),
            issuance: None,
            upgrade: None,
        }
    }
    pub fn delegated(&self) -> Option<&crate::delegated_permits::runtime::Authentication> {
        self.delegated.as_ref()
    }
    pub fn permitted_attempt(&self) -> Option<uuid::Uuid> {
        self.delegated
            .as_ref()
            .map(|a| a.current.record.binding.attempt_id)
    }
    pub fn id(&self) -> uuid::Uuid {
        self.node.id
    }
    pub fn node(&self) -> &Node {
        &self.node
    }
}
impl std::fmt::Debug for AuthenticatedNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticatedNode")
            .field("id", &self.node.id)
            .finish_non_exhaustive()
    }
}
/// Authenticate the bearer once without retaining its raw bytes. Consumers must
/// recheck this verifier under the current node lock after file/RPC/poll waits.
pub async fn authenticate_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedNode, AppError> {
    let invalid = || AppError::InvalidCredential("missing or invalid worker credential".into());
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| t.starts_with("aqn_"))
        .ok_or_else(invalid)?;
    let verifier = zeroize::Zeroizing::new(verifier(token));
    let node = state
        .store
        .find_node_by_credential(&verifier)
        .await?
        .ok_or_else(invalid)?;
    if node.revoked_at.is_some() {
        return Err(AppError::CredentialRevoked);
    }
    Ok(AuthenticatedNode {
        node,
        verifier,
        delegated: None,
        issuance: None,
        upgrade: None,
    })
}
/// Lock and recheck the exact credential and current revocation. Caller-owned
/// service/owner locks precede this node lock; attempt/session locks follow it.
/// A rotated/deleted/revoked credential is refused even if the initial lookup
/// succeeded. No authority is extended, renewed or installed by this check.
pub async fn lock_current_node(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    context: &AuthenticatedNode,
) -> Result<uuid::Uuid, AppError> {
    if let Some(delegate) = &context.delegated {
        return crate::delegated_permits::runtime::lock_authenticated(tx, delegate).await;
    }
    let current: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL AND credential_hash=$2 FOR UPDATE",
    )
    .bind(context.node.id)
    .bind(context.verifier.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    current.ok_or(AppError::CredentialRevoked)
}
/// Existing callers keep the Node snapshot API and its authentication behavior.
pub(crate) async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Node, AppError> {
    Ok(authenticate_context(state, headers).await?.node)
}

async fn enroll(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<EnrollRequest>,
) -> Result<Json<EnrollResponse>, AppError> {
    check_protocol(req.protocol_version)?;
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(bad("name must be 1-64 characters"));
    }
    if req.worker_version.is_empty() || req.worker_version.chars().count() > 64 {
        return Err(bad("worker_version must be 1-64 characters"));
    }
    let credential = new_node_credential();
    let node = state
        .store
        .redeem_enrollment(
            &verifier(&req.code),
            NewNode {
                name: name.to_string(),
                credential_hash: verifier(&credential),
                worker_version: req.worker_version,
                protocol_version: req.protocol_version,
            },
            (state.clock)(),
        )
        .await?
        .ok_or(AppError::EnrollmentInvalid)?;
    tracing::info!(node_id = %node.id, "worker enrolled");
    Ok(Json(EnrollResponse {
        node_id: node.id,
        credential,
        heartbeat_interval_secs: state.config.workers.heartbeat_interval_secs,
    }))
}

async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, AppError> {
    check_protocol(req.protocol_version)?;
    if req.worker_version.is_empty() || req.worker_version.chars().count() > 64 {
        return Err(bad("worker_version must be 1-64 characters"));
    }
    // Revocation is checked inside `authenticate`, before anything is written.
    let node = authenticate(&state, &headers).await?;
    state
        .store
        .record_heartbeat(
            node.id,
            &req.worker_version,
            req.protocol_version,
            (state.clock)(),
        )
        .await?;
    Ok(Json(HeartbeatResponse {
        server_version: version_info().server_version,
        heartbeat_interval_secs: state.config.workers.heartbeat_interval_secs,
    }))
}

async fn machine(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(report): ApiJson<aq_swarm::machine::MachineReport>,
) -> Result<Json<serde_json::Value>, AppError> {
    report
        .validate()
        .map_err(|r| bad(&format!("invalid machine report: {r}")))?;
    let node = authenticate(&state, &headers).await?;
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    aq_swarm::machine::record(&state.store.pool, node.id, &report, now)
        .await
        .map_err(|_| AppError::Internal)?;
    Ok(Json(serde_json::json!({"recorded": true})))
}

async fn probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ProbeRequest>,
) -> Result<Json<ProbeResponse>, AppError> {
    authenticate(&state, &headers).await?;
    if req.nonce.chars().count() > 64 {
        return Err(bad("nonce must be at most 64 characters"));
    }
    Ok(Json(ProbeResponse {
        nonce: req.nonce,
        server_time: (state.clock)(),
    }))
}
