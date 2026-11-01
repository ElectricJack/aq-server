//! Online attempt credentials, scoped graph reads, durable graph intent and mail.
mod contract;
mod graph;
pub(crate) mod messaging;
mod mutations;
pub(crate) mod native_binding;
pub mod scheduling;
use crate::{
    AppState,
    authz::RequireOwner,
    execution::authority::{AuthorityError, AuthorityView, Policy},
};
use aq_worker_protocol::{AttemptRef, NodeIdentity};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAccessConfig {
    pub graph_url: String,
    pub graph_token_file: PathBuf,
    #[serde(default)]
    pub mutation_actor_id: Option<String>,
    #[serde(default = "default_ttl")]
    pub credential_ttl_ms: u64,
}
fn default_ttl() -> u64 {
    120_000
}
pub struct Runtime {
    client: reqwest::Client,
    url: reqwest::Url,
    token: String,
    ttl: u64,
    mutation_actor: Option<String>,
}
impl Runtime {
    pub async fn new(config: &AgentAccessConfig) -> anyhow::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(&config.graph_token_file)?;
        anyhow::ensure!(
            meta.is_file() && meta.mode() & 0o077 == 0,
            "graph credential must be a private regular file"
        );
        let file = std::fs::File::open(&config.graph_token_file)?;
        let opened = file.metadata()?;
        anyhow::ensure!(
            opened.is_file()
                && opened.mode() & 0o077 == 0
                && opened.ino() == meta.ino()
                && opened.dev() == meta.dev()
                && opened.len() <= 4096,
            "graph credential file changed or exceeds size limit"
        );
        use std::io::Read;
        let mut contents = String::new();
        file.take(4097).read_to_string(&mut contents)?;
        let token = contents.trim().to_owned();
        anyhow::ensure!(
            token.len() >= 32 && token.len() <= 4096 && !token.chars().any(char::is_whitespace),
            "invalid graph service credential"
        );
        let url = reqwest::Url::parse(&config.graph_url)?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "graph URL must be an HTTP origin"
        );
        anyhow::ensure!(
            (1000..=120_000).contains(&config.credential_ttl_ms),
            "agent credential TTL outside bounds"
        );
        if let Some(actor) = &config.mutation_actor_id {
            anyhow::ensure!(
                !actor.is_empty() && actor.len() <= 128 && !actor.chars().any(char::is_control),
                "invalid mutation actor identity"
            );
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
            url,
            token,
            ttl: config.credential_ttl_ms,
            mutation_actor: config.mutation_actor_id.clone(),
        })
    }
}
/// Public graph reachability/access evidence, not an execution claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceVerificationError {
    NotFound,
    Forbidden,
    Unavailable,
}
impl Runtime {
    pub async fn verify_workspace(
        &self,
        workspace: Uuid,
    ) -> std::result::Result<(), WorkspaceVerificationError> {
        use WorkspaceVerificationError::*;
        let url = self
            .url
            .join(&format!("/v1/workspaces/{workspace}/nodes"))
            .map_err(|_| Unavailable)?;
        let mut response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("X-AQ-Graph-API-Version", "1")
            .query(&[("limit", "1")])
            .send()
            .await
            .map_err(|_| Unavailable)?;
        if response
            .headers()
            .get("X-AQ-Graph-API-Version")
            .is_none_or(|v| v != "1")
        {
            return Err(Unavailable);
        }
        match response.status().as_u16() {
            401 | 403 => return Err(Forbidden),
            404 => return Err(NotFound),
            200 => {}
            _ => return Err(Unavailable),
        }
        if response.content_length().is_some_and(|n| n > 1048576) {
            return Err(Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Unavailable)? {
            if bytes.len() + chunk.len() > 1048576 {
                return Err(Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        let page: Value = serde_json::from_slice(&bytes).map_err(|_| Unavailable)?;
        if !contract::upstream_valid("NodePage", &page)
            || page["items"].as_array().is_none_or(|items| {
                items.len() > 1
                    || items
                        .iter()
                        .any(|node| node["workspace_id"] != json!(workspace))
            })
        {
            return Err(Unavailable);
        }
        Ok(())
    }
}
#[derive(Debug)]
struct Error(&'static str);
type Result<T> = std::result::Result<T, Error>;
impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self("internal")
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self("invalid_request")
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        self.respond(Uuid::new_v4())
    }
}
impl Error {
    fn respond(self, request_id: Uuid) -> Response {
        let status = match self.0 {
            "unauthorized" | "session_expired" | "session_revoked" => StatusCode::UNAUTHORIZED,
            "scope_denied" | "unsupported_capability" => StatusCode::FORBIDDEN,
            "not_found" | "operation_unknown" => StatusCode::NOT_FOUND,
            "unsupported_api_version" => StatusCode::UPGRADE_REQUIRED,
            "internal" => StatusCode::INTERNAL_SERVER_ERROR,
            "overloaded" => StatusCode::SERVICE_UNAVAILABLE,
            "response_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            "stale_generation" | "id_conflict" => StatusCode::CONFLICT,
            _ => StatusCode::BAD_REQUEST,
        };
        response(
            status,
            json!({"api_version":1,"request_id":request_id,"operation_id":null,"code":self.0,"message":"agent access request refused","effect":"not_applied","retryable":self.0=="overloaded","recovery":null}),
        )
    }
}
fn response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            ("X-AQ-Agent-API-Version", "1"),
            ("Cache-Control", "no-store"),
        ],
        Json(body),
    )
        .into_response()
}
fn ok(body: Value) -> Response {
    response(StatusCode::OK, body)
}
fn runtime(state: &AppState) -> Result<&Runtime> {
    state.agent_access.as_deref().ok_or(Error("overloaded"))
}
fn version(headers: &HeaderMap) -> Result<()> {
    if headers
        .get("X-AQ-Agent-API-Version")
        .is_some_and(|h| h != "1")
    {
        Err(Error("unsupported_api_version"))
    } else {
        Ok(())
    }
}
fn bearer(headers: &HeaderMap) -> Result<&str> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(Error("unauthorized"))
}
fn canonical(value: &Value) -> Result<Vec<u8>> {
    aq_agent_api::canonical(value).map_err(|_| Error("invalid_request"))
}
fn digest(value: &Value) -> Result<String> {
    aq_agent_api::digest(value).map_err(|_| Error("invalid_request"))
}
fn secret() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!(
        "aqa_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    )
}
fn id(v: &Value, key: &str) -> Result<Uuid> {
    v[key]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(Error("invalid_request"))
}
fn now(state: &AppState) -> u64 {
    crate::execution::api::now(state)
}

pub fn admin_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .merge(messaging::admin_routes())
        .merge(mutations::admin_routes())
        .route("/api/agent-access/provision", post(provision))
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/api/agent-access/{attempt}/revoke", post(revoke))
                .with_state(state.control_view()),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024))
}
pub fn machine_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .merge(messaging::machine_routes())
        .merge(mutations::machine_routes())
        .route("/v1/agent/context", get(context))
        .route("/v1/agent/graph/read", post(read))
        .with_state(state.clone())
        .merge(
            Router::new()
                .merge(messaging::node_routes())
                .merge(mutations::node_routes())
                .route("/v2/worker/agent-session", post(issue))
                .with_state(state.control_view()),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provision {
    attempt_id: Uuid,
    profile: Value,
    context: Value,
}
#[derive(Clone)]
struct Principal {
    created: Vec<Value>,
    binding: Value,
    profile: Value,
    context: Value,
    version: i64,
    revision: i64,
    expires: u64,
}
impl Principal {
    fn context_response(&self) -> Value {
        json!({"api_version":1,"binding":self.binding,"profile":self.profile,"context":self.context,"credential":{"kind":"agent_session","version":self.version.to_string(),"expires_at":chrono::DateTime::from_timestamp_millis(self.expires as i64).expect("bounded credential timestamp").to_rfc3339(),"authority_revision":self.revision.to_string()}})
    }
    fn bounded_context_response(&self) -> Result<Value> {
        let body = self.context_response();
        self.bound(&body)?;
        if !contract::valid("ContextResponse", &body) {
            return Err(Error("invalid_request"));
        }
        Ok(body)
    }
    fn bound(&self, body: &Value) -> Result<()> {
        if canonical(body)?.len()
            > self.profile["max_response_bytes"].as_u64().unwrap_or(0) as usize
        {
            return Err(Error("response_too_large"));
        }
        Ok(())
    }
    fn issuance_response(&self, token: &str) -> Result<Value> {
        let body =
            json!({"api_version":1,"token":token,"context":self.bounded_context_response()?});
        self.bound(&body)?;
        Ok(body)
    }
    fn readable(&self, id: &Value) -> bool {
        self.created.contains(id)
            || self.profile["read_scope"]["mode"] == "workspace"
            || self.profile["read_scope"]["record_ids"]
                .as_array()
                .is_some_and(|ids| ids.contains(id))
    }
    fn require(&self, id: &Value) -> Result<()> {
        if self.readable(id) {
            Ok(())
        } else {
            Err(Error("not_found"))
        }
    }
}
async fn lock_attempt(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    node: Uuid,
) -> Result<AuthorityView> {
    crate::execution::authority::load_attempt_locked(state, tx, node, attempt, Policy::GraphActive)
        .await
        .map_err(|error| match error {
            AuthorityError::NotFound | AuthorityError::Identity => Error("unauthorized"),
            AuthorityError::Revoked => Error("session_revoked"),
            AuthorityError::Expired => Error("session_expired"),
            AuthorityError::StaleBoot => Error("stale_generation"),
            AuthorityError::Unsupported => Error("scope_denied"),
            AuthorityError::InvalidState | AuthorityError::Database(_) => Error("internal"),
        })
}

async fn provision(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    StrictJson(request): StrictJson<Provision>,
) -> Result<Response> {
    runtime(&state)?;
    if !contract::valid("ResolvedProfile", &request.profile)
        || !contract::valid("ContextSnapshot", &request.context)
    {
        return Err(Error("invalid_request"));
    }
    let node: Uuid =
        sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1")
            .bind(request.attempt_id)
            .fetch_optional(&state.store.pool)
            .await?
            .ok_or(Error("not_found"))?;
    let mut tx = state.store.pool.begin().await?;
    let a = lock_attempt(&state, &mut tx, request.attempt_id, node).await?;
    if a.kind != crate::execution::authority::Kind::LegacyFixture {
        return Err(Error("scope_denied"));
    }
    let graph = serde_json::to_value(&a.graph)?;
    if graph.is_null()
        || graph["profile_id"] != request.profile["profile_id"]
        || graph["profile_revision"] != request.profile["revision"]
        || graph["profile_digest"] != digest(&request.profile)?
        || graph["context_digest"] != digest(&request.context)?
    {
        return Err(Error("scope_denied"));
    }
    let binding = json!({"task_id":a.task_id,"attempt_id":a.attempt.attempt_id,"session_id":a.attempt.session_id,"generation":a.attempt.generation,"node_id":node,"boot_id":a.identity.boot_id,"project_id":a.project_id,"workspace_id":graph["workspace_id"],"resolved_spec_digest":a.execution_spec_digest,"profile_digest":graph["profile_digest"],"context_digest":graph["context_digest"]});
    if !contract::valid("Binding", &binding) {
        return Err(Error("invalid_request"));
    }
    let principal = Principal {
        created: Vec::new(),
        binding: binding.clone(),
        profile: request.profile.clone(),
        context: request.context.clone(),
        version: i64::MAX,
        revision: i64::MAX,
        expires: 253402300799999,
    };
    graph::validate_launch(&principal)?;
    // Reserve room for maximum credential/authority version widths and a full
    // timestamp before persisting authority. A response refusal cannot mint a token.
    principal.issuance_response(&"a".repeat(47))?;
    let inserted=sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING").bind(request.attempt_id).bind(node).bind(&binding).bind(&request.profile).bind(&request.context).execute(&mut *tx).await?.rows_affected();
    if inserted == 0 {
        let row = sqlx::query(
            "SELECT binding,profile,context,revoked FROM agent_launch_bindings WHERE attempt_id=$1",
        )
        .bind(request.attempt_id)
        .fetch_one(&mut *tx)
        .await?;
        if row.get::<Value, _>("binding") != binding
            || row.get::<Value, _>("profile") != request.profile
            || row.get::<Value, _>("context") != request.context
            || row.get::<bool, _>("revoked")
        {
            return Err(Error("id_conflict"));
        }
    }
    tx.commit().await?;
    Ok(ok(json!({"api_version":1,"binding":binding})))
}
async fn revoke(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(attempt): Path<Uuid>,
) -> Result<Response> {
    let node: Option<Uuid> =
        sqlx::query_scalar("SELECT node_id FROM agent_launch_bindings WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_optional(&state.store.pool)
            .await?;
    let mut tx = state.store.pool.begin().await?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node.ok_or(Error("not_found"))?)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE,credential_verifier=NULL,authority_revision=authority_revision+1 WHERE attempt_id=$1 AND NOT revoked").bind(attempt).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(ok(json!({"api_version":1,"revoked":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Issue {
    identity: NodeIdentity,
    attempt: AttemptRef,
    expected_credential_version: u64,
}
async fn issue(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(request): StrictJson<Issue>,
) -> Result<Response> {
    version(&headers)?;
    let rt = runtime(&state)?;
    let node = crate::worker_api::authenticate_context(&state, &headers)
        .await
        .map_err(|_| Error("unauthorized"))?;
    if node.id() != request.identity.node_id {
        return Err(Error("unauthorized"));
    }
    let mut tx = state.store.pool.begin().await?;
    crate::worker_api::lock_current_node(&mut tx, &node)
        .await
        .map_err(|error| match error {
            crate::error::AppError::CredentialRevoked => Error("unauthorized"),
            _ => Error("internal"),
        })?;
    let a = lock_attempt(&state, &mut tx, request.attempt.attempt_id, node.id()).await?;
    if a.identity != request.identity || a.attempt != request.attempt {
        return Err(Error("stale_generation"));
    }
    let row=sqlx::query("SELECT binding,profile,context,revoked,credential_version,authority_revision FROM agent_launch_bindings WHERE attempt_id=$1 FOR UPDATE").bind(request.attempt.attempt_id).fetch_optional(&mut *tx).await?.ok_or(Error("scope_denied"))?;
    if row.get::<bool, _>("revoked") {
        return Err(Error("session_revoked"));
    }
    let old: i64 = row.get("credential_version");
    if request.expected_credential_version != old as u64 || old == i64::MAX {
        return Err(Error("id_conflict"));
    }
    let expires = now(&state)
        .saturating_add(rt.ttl)
        .min(a.grant_expires_at_ms);
    if expires <= now(&state) {
        return Err(Error("session_expired"));
    }
    let token = secret();

    let p = Principal {
        created: Vec::new(),
        binding: row.get("binding"),
        profile: row.get("profile"),
        context: row.get("context"),
        version: old + 1,
        revision: row.get("authority_revision"),
        expires,
    };
    let body = p.issuance_response(&token)?;
    sqlx::query("UPDATE agent_launch_bindings SET credential_version=$2,credential_verifier=$3,credential_expires_at_ms=$4 WHERE attempt_id=$1").bind(request.attempt.attempt_id).bind(old+1).bind(crate::tokens::verifier(&token)).bind(expires as i64).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(ok(body))
}
async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal> {
    let mut tx = state.store.pool.begin().await?;
    let p = authenticate_locked(state, headers, &mut tx).await?;
    tx.commit().await?;
    Ok(p)
}
async fn authenticate_locked(
    state: &AppState,
    headers: &HeaderMap,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<Principal> {
    version(headers)?;
    runtime(state)?;
    let token = bearer(headers)?;
    if !token.starts_with("aqa_") || token.len() != 47 {
        return Err(Error("unauthorized"));
    }
    let verifier = crate::tokens::verifier(token);
    let identity: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT attempt_id,node_id FROM agent_launch_bindings WHERE credential_verifier=$1",
    )
    .bind(&verifier)
    .fetch_optional(&mut **tx)
    .await?;
    let (attempt, node) = identity.ok_or(Error("unauthorized"))?;
    let a = lock_attempt(state, tx, attempt, node).await?;
    let row=sqlx::query("SELECT binding,profile,context,credential_version,authority_revision,credential_expires_at_ms,revoked FROM agent_launch_bindings WHERE attempt_id=$1 AND credential_verifier=$2 FOR UPDATE").bind(attempt).bind(verifier).fetch_optional(&mut **tx).await?.ok_or(Error("unauthorized"))?;
    if row.get::<bool, _>("revoked") {
        return Err(Error("session_revoked"));
    }
    let expires = row.get::<i64, _>("credential_expires_at_ms") as u64;
    if expires <= now(state) {
        return Err(Error("session_expired"));
    }
    let mut p = Principal {
        created: Vec::new(),
        binding: row.get("binding"),
        profile: row.get("profile"),
        context: row.get("context"),
        version: row.get("credential_version"),
        revision: row.get("authority_revision"),
        expires,
    };
    if p.binding["generation"] != a.attempt.generation
        || p.binding["session_id"] != a.attempt.session_id.to_string()
        || p.binding["boot_id"] != a.identity.boot_id.to_string()
    {
        return Err(Error("stale_generation"));
    }
    if p.profile["read_scope"]["include_created_by_attempt"] == true {
        let records: Vec<Uuid> = sqlx::query_scalar("SELECT record_id FROM agent_created_records WHERE attempt_id=$1 ORDER BY record_id LIMIT 256").bind(attempt).fetch_all(&mut **tx).await?;
        p.created = records.into_iter().map(|id| json!(id)).collect();
    }
    Ok(p)
}
async fn context(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    let p = authenticate(&state, &headers).await?;
    Ok(ok(p.bounded_context_response()?))
}
async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    StrictJson(request): StrictJson<Value>,
) -> Response {
    let request_id = id(&request, "request_id")
        .ok()
        .filter(|u| !u.is_nil())
        .unwrap_or_else(Uuid::new_v4);
    match read_inner(state, headers, request).await {
        Ok(response) => response,
        Err(error) => error.respond(request_id),
    }
}
async fn read_inner(state: AppState, headers: HeaderMap, request: Value) -> Result<Response> {
    if request
        .get("api_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v != 1)
    {
        return Err(Error("unsupported_api_version"));
    }
    if !contract::valid("ReadRequest", &request) {
        return Err(Error("invalid_request"));
    }
    let p = authenticate(&state, &headers).await?;
    if p.binding != request["binding"] {
        return Err(Error("stale_generation"));
    }
    let q = &request["query"];
    let capability = format!(
        "read.{}",
        q["kind"].as_str().ok_or(Error("invalid_request"))?
    );
    if !p.profile["capabilities"]
        .as_array()
        .is_some_and(|c| c.contains(&json!(capability)))
    {
        return Err(Error("unsupported_capability"));
    }
    if q.get("node_id").is_some() {
        p.require(&q["node_id"])?;
    }
    let result = tokio::time::timeout(Duration::from_secs(15), graph::read(&state, &p, q))
        .await
        .map_err(|_| Error("overloaded"))??;
    // Never serve a cached/in-flight result as proof of current authority.
    let current = authenticate(&state, &headers).await?;
    if current.version != p.version || current.revision != p.revision {
        return Err(Error("session_revoked"));
    }
    let body = json!({"api_version":1,"request_id":request["request_id"],"binding":p.binding,"query_digest":query_digest(&p.binding,q)?,"observed_at":(state.clock)().to_rfc3339(),"projection":"authorized_scope","result":result});
    let max = p.profile["max_response_bytes"]
        .as_u64()
        .unwrap_or(1024)
        .min(q["max_bytes"].as_u64().unwrap_or(u64::MAX));
    if canonical(&body)?.len() > max as usize {
        return Err(Error("response_too_large"));
    }
    if !contract::valid("ReadResponse", &body) {
        return Err(Error("internal"));
    }
    Ok(ok(body))
}

struct StrictJson<T>(T);
impl<T: serde::de::DeserializeOwned> axum::extract::FromRequest<AppState> for StrictJson<T> {
    type Rejection = Error;
    async fn from_request(req: axum::extract::Request, _: &AppState) -> Result<Self> {
        let bytes = axum::body::to_bytes(req.into_body(), 1_048_576)
            .await
            .map_err(|_| Error("invalid_request"))?;
        let contract::StrictValue(value) =
            serde_json::from_slice(&bytes).map_err(|_| Error("invalid_request"))?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(|_| Error("invalid_request"))
    }
}

fn query_digest(binding: &Value, q: &Value) -> Result<String> {
    let mut q = q.clone();
    if let Some(o) = q.as_object_mut() {
        o.remove("cursor");
    }
    digest(&json!({"binding":binding,"query":q}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap()
    }
    #[test]
    fn canonical_digest_matches_published_profile_and_context() {
        let f = fixture();
        let c = &f["context"];
        assert_eq!(
            digest(&c["profile"]).unwrap(),
            c["binding"]["profile_digest"]
        );
        assert_eq!(
            digest(&c["context"]).unwrap(),
            c["binding"]["context_digest"]
        );
        assert!(contract::valid("ContextResponse", c));
        assert!(canonical(&json!({"fraction":1.5})).is_err());
    }
    #[test]
    fn context_and_issuance_budgets_include_profile_binding_and_credentials() {
        let c = fixture()["context"].clone();
        let mut p = Principal {
            created: Vec::new(),
            binding: c["binding"].clone(),
            profile: c["profile"].clone(),
            context: c["context"].clone(),
            version: 1,
            revision: 1,
            expires: 1,
        };
        let snapshot_bytes = canonical(&p.context).unwrap().len();
        p.profile["max_response_bytes"] = json!(snapshot_bytes.max(1024) + 10);
        assert!(graph::validate_launch(&p).is_ok());
        assert_eq!(
            p.bounded_context_response().unwrap_err().0,
            "response_too_large"
        );
        p.profile["max_response_bytes"] = json!(1048576);
        let context_bytes = canonical(&p.context_response()).unwrap().len();
        p.profile["max_response_bytes"] = json!(context_bytes + 5);
        assert!(p.bounded_context_response().is_ok());
        assert_eq!(
            p.issuance_response(&"a".repeat(47)).unwrap_err().0,
            "response_too_large"
        );
    }
    #[test]
    fn strict_json_rejects_duplicate_and_fractional_values() {
        for s in [r#"{"query":{"x":1,"x":2}}"#, r#"{"number":1.5}"#] {
            assert!(serde_json::from_str::<contract::StrictValue>(s).is_err());
        }
        assert!(
            serde_json::from_str::<contract::StrictValue>(r#"{"x":[1,true,null,"s"]}"#).is_ok()
        );
    }
    #[test]
    fn contract_enforces_unknown_fields_uuid_integer_and_conditional_rules() {
        let mut f = fixture()["context"].clone();
        f["extra"] = json!(true);
        assert!(!contract::valid("ContextResponse", &f));
        let mut f = fixture()["context"].clone();
        f["profile"]["revision"] = json!("9223372036854775808");
        assert!(!contract::valid("ContextResponse", &f));
        let mut f = fixture()["context"].clone();
        f["binding"]["attempt_id"] = json!(Uuid::nil());
        assert!(!contract::valid("ContextResponse", &f));
        assert!(!contract::valid(
            "Coverage",
            &json!({"status":"partial","complete_for":"authorized_projection","reasons":[]})
        ));
    }
    #[test]
    fn query_digest_omits_cursor_but_binds_authority() {
        let binding = fixture()["context"]["binding"].clone();
        let q = json!({"kind":"history","node_id":binding["task_id"],"limit":2,"cursor":null});
        let mut next = q.clone();
        next["cursor"] = json!("opaque");
        assert_eq!(
            query_digest(&binding, &q).unwrap(),
            query_digest(&binding, &next).unwrap()
        );
        let mut changed = binding.clone();
        changed["generation"] = json!(2);
        assert_ne!(
            query_digest(&binding, &q).unwrap(),
            query_digest(&changed, &q).unwrap()
        );
    }
}
