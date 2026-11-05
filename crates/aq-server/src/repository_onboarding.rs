//! D2 owner repository onboarding: one idempotent operation registers a central
//! aq-git binding, retains the default-branch head, checks the repository fits
//! the v1 profile and creates the project binding with its publication policy.
//!
//! Every step is persisted under the caller's `operation_id` before the next
//! begins, so a failure leaves `incomplete` state that a retry of the same
//! operation resumes. Refusals are terminal, typed and replayable. Callers name
//! a GitHub repository and optionally a vault credential profile; they never
//! supply a URL, endpoint, token or command, and no secret is persisted.
use crate::{
    AppState,
    authz::RequireOwner,
    delivery::{PublicationMode, PublicationPolicy, WaitForChecks},
    error::AppError,
    project_bindings::domain::ProjectBinding,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::Row;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

type Result<T> = std::result::Result<T, AppError>;
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;
const RESERVED: [&str; 5] = [
    "project_id",
    "repository_id",
    "repository_binding_generation",
    "repository_adapter_reference",
    "publication",
];

/// Vault `[onboarding]`. The API base and every credential are server-held.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardingConfig {
    #[serde(default = "default_api")]
    pub github_api_url: String,
    #[serde(default)]
    pub credential_profiles: BTreeMap<String, CredentialProfile>,
}
fn default_api() -> String {
    "https://api.github.com".into()
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialProfile {
    pub kind: CredentialKind,
    /// Vault file holding the deploy key or fine-grained token. Only its
    /// profile name ever leaves the vault; aq-git selects it by that name.
    pub secret_file: PathBuf,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    SshDeployKey,
    HttpsFineGrainedToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Ssh,
    Https,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubRepository {
    pub owner: String,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationRequest {
    pub mode: PublicationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for_checks: Option<WaitForChecks>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardRequest {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub github: GithubRepository,
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_profile: Option<String>,
    /// Owner override; otherwise GitHub's `default_branch` is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    pub publication: PublicationRequest,
    /// Project binding fields other than the server-filled repository fields.
    pub template: Map<String, Value>,
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/repositories/onboard", post(route))
        .route("/api/repositories/onboard/{operation_id}", get(show))
        .layer(DefaultBodyLimit::max(128 * 1024))
}
async fn route(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Json(request): Json<OnboardRequest>,
) -> Result<Json<Value>> {
    onboard(&state, actor.id, request).await.map(Json)
}
async fn show(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(operation_id): Path<Uuid>,
) -> Result<Json<Value>> {
    let row = load(&state, operation_id)
        .await?
        .ok_or_else(|| AppError::NotFound("onboarding operation not found".into()))?;
    Ok(Json(response(&row)))
}

fn bad(reason: &str) -> AppError {
    AppError::BadRequest(reason.into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn slug(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 100
        && v != "."
        && v != ".."
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn label(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@:".contains(&b))
}

struct Row_ {
    actor: Uuid,
    digest: String,
    step: String,
    state: Value,
}
async fn load(state: &AppState, operation_id: Uuid) -> Result<Option<Row_>> {
    let row = sqlx::query(
        "SELECT actor_id,request_digest,step,state FROM repository_onboardings WHERE operation_id=$1",
    )
    .bind(operation_id)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(db)?;
    Ok(row.map(|r| Row_ {
        actor: r.get(0),
        digest: r.get(1),
        step: r.get(2),
        state: r.get(3),
    }))
}
async fn save(state: &AppState, operation_id: Uuid, step: &str, value: &Value) -> Result<()> {
    sqlx::query(
        "UPDATE repository_onboardings SET step=$2,state=$3,updated_at_ms=$4 WHERE operation_id=$1 AND step NOT IN ('bound','refused')",
    )
    .bind(operation_id)
    .bind(step)
    .bind(value)
    .bind((state.clock)().timestamp_millis())
    .execute(&state.store.pool)
    .await
    .map_err(db)?;
    Ok(())
}
fn response(row: &Row_) -> Value {
    let s = &row.state;
    let disposition = match row.step.as_str() {
        "bound" => "bound",
        "refused" => "refused",
        _ => "incomplete",
    };
    let mut out = json!({"operation_id":s["request"]["operation_id"],"step":row.step,"disposition":disposition,
        "project_id":s["request"]["project_id"],"repository_id":s["request"]["repository_id"],
        "remote_url":s["remote_url"],"credential_profile":s["request"]["credential_profile"],
        "default_branch":s["default_branch"],"repository_binding_generation":s["binding_generation"],"base":s["base"]});
    for key in ["refusal", "error", "project"] {
        if !s[key].is_null() {
            out[key] = s[key].clone();
        }
    }
    out
}

/// Deterministic aq-git operation id per onboarding step, so a resumed step is
/// an aq-git idempotent retry rather than a second mutation.
fn step_operation(operation_id: Uuid, step: &str) -> Uuid {
    let hex =
        aq_profiles::digest(&json!({"onboarding":operation_id,"step":step})).unwrap_or_default();
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2).unwrap_or("00"), 16).unwrap_or(0);
    }
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

enum Git {
    Ok(Value),
    Refused { code: String, message: String },
    Unavailable,
}
async fn aq_git(
    state: &AppState,
    operation_id: Uuid,
    operation: &str,
    repository_id: Uuid,
    generation: u64,
    params: Value,
) -> Git {
    let Some(rt) = &state.source_retention else {
        return Git::Unavailable;
    };
    let request_id = operation_id.to_string();
    let request = json!({"protocol_version":1,"request_id":request_id,"operation":operation,"repository_id":repository_id,
        "operation_id":operation_id,"expected_binding_generation":generation,"params":params,"deadline_ms":30000});
    let exchange = async {
        let mut socket = tokio::net::UnixStream::connect(&rt.config.git_socket).await?;
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        socket.write_all(&bytes).await?;
        let mut frame = Vec::new();
        BufReader::new(socket)
            .take(MAX_RESPONSE + 1)
            .read_until(b'\n', &mut frame)
            .await?;
        anyhow::ensure!(
            frame.len() as u64 <= MAX_RESPONSE && frame.last() == Some(&b'\n'),
            "invalid aq-git frame"
        );
        let v: Value = serde_json::from_slice(&frame)?;
        anyhow::ensure!(
            v["protocol_version"] == 1 && v["request_id"] == request_id.as_str(),
            "aq-git response mismatch"
        );
        Ok::<_, anyhow::Error>(v)
    };
    match tokio::time::timeout(Duration::from_secs(35), exchange).await {
        Ok(Ok(v)) if v["ok"] == true && v["result"].is_object() => Git::Ok(v["result"].clone()),
        Ok(Ok(v)) if v["ok"] == false => match v["error"]["code"].as_str() {
            Some(code) => Git::Refused {
                code: code.into(),
                message: v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(512)
                    .collect(),
            },
            None => Git::Unavailable,
        },
        _ => Git::Unavailable,
    }
}

fn refusal(code: &str, reason: &str, source: &str, message: &str) -> Value {
    json!({"code":code,"reason":reason,"source":source,"message":message})
}
fn transient(code: &str, message: &str) -> Value {
    json!({"code":code,"message":message,"retry":true})
}

/// Pre-check the retained head's top-level tree against the v1 checkout
/// profile (`no_attributes_v1`, no submodules, no LFS).
fn tree_refusal(entries: &[Value]) -> Option<(&'static str, String)> {
    for e in entries {
        let path = e["path"]["value"].as_str().unwrap_or("");
        let found = if e["mode"] == "160000" || path == ".gitmodules" {
            Some("submodules")
        } else if path == ".lfsconfig" {
            Some("lfs")
        } else if path == ".gitattributes" {
            Some("gitattributes")
        } else {
            None
        };
        if let Some(reason) = found {
            return Some((reason, format!("repository head contains {path}")));
        }
    }
    None
}

struct Github<'a> {
    config: &'a OnboardingConfig,
    token: Option<String>,
}
enum Meta {
    Branch { branch: String, signing: bool },
    Unavailable(String),
}
impl Github<'_> {
    async fn get(&self, path: &str) -> std::result::Result<Value, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "github client unavailable".to_string())?;
        let mut request = client
            .get(format!(
                "{}{path}",
                self.config.github_api_url.trim_end_matches('/')
            ))
            .header("accept", "application/vnd.github+json")
            .header("user-agent", "aq-server-onboarding")
            .header("x-github-api-version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "github metadata request failed".to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "github metadata returned {}",
                response.status().as_u16()
            ));
        }
        response
            .json()
            .await
            .map_err(|_| "github metadata is not JSON".to_string())
    }
    async fn metadata(&self, repo: &GithubRepository, branch: Option<&str>) -> Meta {
        let base = format!("/repos/{}/{}", repo.owner, repo.name);
        let branch = match branch {
            Some(b) => b.to_string(),
            None => match self.get(&base).await {
                Ok(v) => match v["default_branch"].as_str() {
                    Some(b) if !b.is_empty() => b.to_string(),
                    _ => return Meta::Unavailable("github reported no default branch".into()),
                },
                Err(e) => return Meta::Unavailable(e),
            },
        };
        match self.get(&format!("{base}/rules/branches/{branch}")).await {
            Ok(Value::Array(rules)) => Meta::Branch {
                signing: rules.iter().any(|r| r["type"] == "required_signatures"),
                branch,
            },
            Ok(_) => Meta::Unavailable("github branch rules are malformed".into()),
            Err(e) => Meta::Unavailable(e),
        }
    }
}

fn validate(state: &AppState, r: &OnboardRequest) -> Result<(ProjectBinding, String)> {
    if r.operation_id.is_nil() || r.project_id.is_nil() || r.repository_id.is_nil() {
        return Err(bad("invalid onboarding identity"));
    }
    if !slug(&r.github.owner) || !slug(&r.github.name) {
        return Err(bad("invalid GitHub repository name"));
    }
    if RESERVED.iter().any(|k| r.template.contains_key(*k)) {
        return Err(bad(
            "onboarding template must not set server-filled repository fields",
        ));
    }
    let config = state
        .config
        .onboarding
        .as_ref()
        .ok_or_else(|| bad("repository onboarding is not configured"))?;
    if let Some(name) = &r.credential_profile {
        let profile = label(name)
            .then(|| config.credential_profiles.get(name))
            .flatten()
            .ok_or_else(|| bad("unknown credential profile"))?;
        let expected = match r.transport {
            Transport::Ssh => CredentialKind::SshDeployKey,
            Transport::Https => CredentialKind::HttpsFineGrainedToken,
        };
        if profile.kind != expected {
            return Err(bad("credential profile does not match the transport"));
        }
    }
    let remote_url = match r.transport {
        Transport::Ssh => format!(
            "ssh://git@github.com/{}/{}.git",
            r.github.owner, r.github.name
        ),
        Transport::Https => format!(
            "https://github.com/{}/{}.git",
            r.github.owner, r.github.name
        ),
    };
    // Validate everything the binding needs before any external mutation.
    let binding = binding(r, 1, r.default_branch.as_deref().unwrap_or("main"))?;
    binding
        .validate(&state.config.projects)
        .map_err(|_| bad("invalid project execution policy"))?;
    Ok((binding, remote_url))
}
fn binding(r: &OnboardRequest, generation: u64, branch: &str) -> Result<ProjectBinding> {
    let policy = PublicationPolicy {
        mode: r.publication.mode,
        target: format!("refs/heads/{branch}"),
        wait_for_checks: r.publication.wait_for_checks.clone(),
    };
    policy
        .validate()
        .map_err(|_| bad("invalid default branch or publication policy"))?;
    let mut body = r.template.clone();
    body.insert("project_id".into(), json!(r.project_id));
    body.insert("repository_id".into(), json!(r.repository_id));
    body.insert("repository_binding_generation".into(), json!(generation));
    body.insert(
        "repository_adapter_reference".into(),
        json!("aq.git.configured"),
    );
    body.insert(
        "publication".into(),
        serde_json::to_value(&policy).map_err(|_| AppError::Internal)?,
    );
    serde_json::from_value(Value::Object(body)).map_err(|_| bad("invalid onboarding template"))
}

/// The owner a CLI invocation acts as; viewers and unknown emails are `None`.
pub async fn owner_by_email(state: &AppState, email: &str) -> Result<Option<Uuid>> {
    sqlx::query_scalar("SELECT id FROM users WHERE lower(email)=lower($1) AND role='owner'")
        .bind(email)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(db)
}

static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The single onboarding operation shared by the admin route and the CLI.
pub async fn onboard(state: &AppState, actor: Uuid, request: OnboardRequest) -> Result<Value> {
    let digest = aq_profiles::digest(&json!({"kind":"repository.onboard","request":request}))
        .map_err(|_| bad("invalid onboarding request"))?;
    // Serialise onboarding steps so a concurrent retry cannot race a step.
    let _gate = GATE.lock().await;
    if let Some(row) = load(state, request.operation_id).await? {
        if row.actor != actor || row.digest != digest {
            return Err(AppError::Conflict(
                "operation_id was used for a different onboarding request".into(),
            ));
        }
        if row.step == "bound" || row.step == "refused" {
            return Ok(response(&row));
        }
    }
    let (_, remote_url) = validate(state, &request)?;
    let now = (state.clock)().timestamp_millis();
    sqlx::query(
        "INSERT INTO repository_onboardings (operation_id,actor_id,request_digest,project_id,repository_id,step,state,created_at_ms,updated_at_ms)
         VALUES ($1,$2,$3,$4,$5,'started',$6,$7,$7) ON CONFLICT (operation_id) DO NOTHING",
    )
    .bind(request.operation_id)
    .bind(actor)
    .bind(&digest)
    .bind(request.project_id)
    .bind(request.repository_id)
    .bind(json!({"request":request,"remote_url":remote_url}))
    .bind(now)
    .execute(&state.store.pool)
    .await
    .map_err(db)?;
    let mut row = load(state, request.operation_id)
        .await?
        .ok_or(AppError::Internal)?;
    advance(state, actor, &request, &mut row).await?;
    Ok(response(&row))
}

async fn advance(state: &AppState, actor: Uuid, r: &OnboardRequest, row: &mut Row_) -> Result<()> {
    let op = r.operation_id;
    let config = state.config.onboarding.as_ref().ok_or(AppError::Internal)?;
    let profile = r
        .credential_profile
        .as_ref()
        .and_then(|n| config.credential_profiles.get(n));
    macro_rules! finish {
        ($step:expr, $key:expr, $value:expr) => {{
            row.state[$key] = $value;
            row.step = $step.into();
            save(state, op, &row.step, &row.state).await?;
            return Ok(());
        }};
    }
    loop {
        let s = row.state.clone();
        match row.step.as_str() {
            "started" => {
                let branch = match s["default_branch"].as_str() {
                    Some(b) => b.to_string(),
                    None => {
                        let token = match profile {
                            Some(p) if p.kind == CredentialKind::HttpsFineGrainedToken => {
                                match tokio::fs::read_to_string(&p.secret_file).await {
                                    Ok(t) => Some(t.trim().to_string()),
                                    Err(_) => finish!(
                                        "started",
                                        "error",
                                        transient(
                                            "credential_unreadable",
                                            "vault credential profile is unreadable"
                                        )
                                    ),
                                }
                            }
                            _ => None,
                        };
                        let github = Github { config, token };
                        match github
                            .metadata(&r.github, r.default_branch.as_deref())
                            .await
                        {
                            Meta::Branch { branch, signing } => {
                                row.state["default_branch"] = json!(branch);
                                if signing {
                                    finish!(
                                        "refused",
                                        "refusal",
                                        refusal(
                                            "unsupported_repo_profile",
                                            "required_signing",
                                            "precheck",
                                            "the default branch requires signed commits"
                                        )
                                    );
                                }
                                // The template is re-validated with the real branch.
                                binding(r, 1, &branch)?;
                                save(state, op, "started", &row.state).await?;
                                branch
                            }
                            Meta::Unavailable(e) => {
                                finish!(
                                    "started",
                                    "error",
                                    transient("github_metadata_unavailable", &e)
                                )
                            }
                        }
                    }
                };
                let target = format!("refs/heads/{branch}");
                let mut params = json!({"identity":format!("github.com/{}/{}", r.github.owner, r.github.name),
                    "mode":"central","remote_url":s["remote_url"],"allowed_refs":[target],"allowed_targets":[target]});
                if let Some(name) = &r.credential_profile {
                    params["credential_profile"] = json!(name);
                }
                match aq_git(
                    state,
                    step_operation(op, "repo.register"),
                    "repo.register",
                    r.repository_id,
                    0,
                    params,
                )
                .await
                {
                    Git::Ok(v) => match v["binding_generation"].as_u64() {
                        Some(g)
                            if g > 0 && v["object_format"] == "sha1" && v["mode"] == "central" =>
                        {
                            row.state["binding_generation"] = json!(g);
                            row.state["error"] = Value::Null;
                            row.step = "registered".into();
                            save(state, op, "registered", &row.state).await?;
                        }
                        _ => finish!(
                            "refused",
                            "refusal",
                            refusal(
                                "unsupported_repo_profile",
                                "object_format",
                                "aq-git",
                                "aq-git binding is not central SHA-1"
                            )
                        ),
                    },
                    Git::Refused { code, message } => {
                        let (kind, reason) = match code.as_str() {
                            "unsupported_feature"
                                if r.credential_profile.is_some()
                                    && message.contains("credential") =>
                            {
                                (
                                    "credential_profile_unavailable",
                                    "credential_profile_unavailable",
                                )
                            }
                            "unsupported_feature" => {
                                ("unsupported_repo_profile", "unsupported_feature")
                            }
                            "binding_conflict" => {
                                ("repository_already_registered", "binding_conflict")
                            }
                            "invalid_request" => ("invalid_repository", "invalid_request"),
                            _ => finish!("started", "error", transient(&code, &message)),
                        };
                        finish!(
                            "refused",
                            "refusal",
                            refusal(kind, reason, "aq-git", &message)
                        )
                    }
                    Git::Unavailable => finish!(
                        "started",
                        "error",
                        transient("aq_git_unavailable", "aq-git is unavailable")
                    ),
                }
            }
            "registered" => {
                let target = format!("refs/heads/{}", s["default_branch"].as_str().unwrap_or(""));
                let g = s["binding_generation"].as_u64().unwrap_or(0);
                match aq_git(
                    state,
                    step_operation(op, "repo.sync"),
                    "repo.sync",
                    r.repository_id,
                    g,
                    json!({"refs":[target]}),
                )
                .await
                {
                    Git::Ok(v) => match v["refs"][&target].as_str() {
                        Some(oid)
                            if oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()) =>
                        {
                            row.state["base"] = json!({"ref":target,"oid":oid,"observation_id":v["observation_id"]});
                            row.state["error"] = Value::Null;
                            row.step = "synced".into();
                            save(state, op, "synced", &row.state).await?;
                        }
                        _ => finish!(
                            "refused",
                            "refusal",
                            refusal(
                                "unsupported_repo_profile",
                                "default_branch_absent",
                                "aq-git",
                                "the default branch has no retained head"
                            )
                        ),
                    },
                    Git::Refused { code, message } if code == "unsupported_feature" => finish!(
                        "refused",
                        "refusal",
                        refusal(
                            "unsupported_repo_profile",
                            "unsupported_feature",
                            "aq-git",
                            &message
                        )
                    ),
                    Git::Refused { code, message } => {
                        finish!("registered", "error", transient(&code, &message))
                    }
                    Git::Unavailable => finish!(
                        "registered",
                        "error",
                        transient("aq_git_unavailable", "aq-git is unavailable")
                    ),
                }
            }
            "synced" => {
                let g = s["binding_generation"].as_u64().unwrap_or(0);
                let oid = json!({"algorithm":"sha1","hex":s["base"]["oid"]});
                match aq_git(
                    state,
                    step_operation(op, "tree"),
                    "tree",
                    r.repository_id,
                    g,
                    json!({"oid":oid,"limit":1000}),
                )
                .await
                {
                    Git::Ok(v) => {
                        let entries = v["entries"].as_array().cloned().unwrap_or_default();
                        if let Some((reason, message)) = tree_refusal(&entries) {
                            finish!(
                                "refused",
                                "refusal",
                                refusal("unsupported_repo_profile", reason, "precheck", &message)
                            );
                        }
                        row.state["error"] = Value::Null;
                        row.step = "checked".into();
                        save(state, op, "checked", &row.state).await?;
                    }
                    Git::Refused { code, message } if code == "unsupported_feature" => finish!(
                        "refused",
                        "refusal",
                        refusal(
                            "unsupported_repo_profile",
                            "unsupported_feature",
                            "aq-git",
                            &message
                        )
                    ),
                    Git::Refused { code, message } => {
                        finish!("synced", "error", transient(&code, &message))
                    }
                    Git::Unavailable => finish!(
                        "synced",
                        "error",
                        transient("aq_git_unavailable", "aq-git is unavailable")
                    ),
                }
            }
            "checked" => {
                let g = s["binding_generation"].as_u64().unwrap_or(0);
                let binding = binding(r, g, s["default_branch"].as_str().unwrap_or(""))?;
                match crate::project_bindings::put_binding(state, actor, op, 0, binding).await {
                    Ok(project) => {
                        row.state["error"] = Value::Null;
                        finish!("bound", "project", project)
                    }
                    Err(AppError::Conflict(m) | AppError::BadRequest(m)) => {
                        finish!("checked", "error", transient("project_binding_refused", &m))
                    }
                    Err(_) => finish!(
                        "checked",
                        "error",
                        transient("project_binding_unavailable", "project binding failed")
                    ),
                }
            }
            _ => return Ok(()),
        }
    }
}
