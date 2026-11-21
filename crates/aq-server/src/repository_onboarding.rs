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
    AppState, authz::RequireOwner, delivery::PublicationPolicy, error::AppError,
    project_bindings::domain::ProjectBinding,
};
use aq_delivery::repository_onboarding::{
    self as wire, Adoption, AdoptionError, Git, Github, Meta, Row, label, response, slug,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::{get, post},
};
use serde_json::{Value, json};
use uuid::Uuid;
pub use wire::{
    AdoptRequest, CredentialKind, CredentialProfile, GithubRepository, OnboardRequest,
    OnboardingConfig, PublicationRequest, Transport,
};

type Result<T> = std::result::Result<T, AppError>;
const RESERVED: [&str; 5] = [
    "project_id",
    "repository_id",
    "repository_binding_generation",
    "repository_adapter_reference",
    "publication",
];

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/repositories/onboard", post(route))
        .route("/api/repositories/onboard/{operation_id}", get(show))
        .route("/api/repositories/adopt", post(adopt_route))
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
    let mut conn = state.store.pool.acquire().await.map_err(db)?;
    let row = wire::load(&mut conn, operation_id)
        .await
        .map_err(db)?
        .ok_or_else(|| AppError::NotFound("onboarding operation not found".into()))?;
    Ok(Json(response(&row)))
}

fn bad(reason: &str) -> AppError {
    AppError::BadRequest(reason.into())
}
use crate::error::db;
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

async fn aq_git(
    state: &AppState,
    operation_id: Uuid,
    operation: &str,
    repository_id: Uuid,
    generation: u64,
    params: Value,
) -> Git {
    wire::aq_git(
        git_socket(state),
        operation_id,
        operation,
        repository_id,
        generation,
        params,
    )
    .await
}
fn git_socket(state: &AppState) -> Option<&std::path::Path> {
    state
        .source_retention
        .as_ref()
        .map(|rt| rt.config.git_socket.as_path())
}
async fn save(state: &AppState, operation_id: Uuid, step: &str, value: &Value) -> Result<()> {
    wire::save(
        &state.store.pool,
        operation_id,
        step,
        value,
        (state.clock)().timestamp_millis(),
    )
    .await
    .map_err(db)
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
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    // Coordinate intent creation with adoption's history check for seeded projects.
    sqlx::query("SELECT project_id FROM project_execution_bindings WHERE project_id=$1 FOR UPDATE")
        .bind(request.project_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let op = request.operation_id;
    let row = wire::load(&mut tx, op).await.map_err(db)?;
    if let Some(row) = &row {
        check_operation(row, actor, &digest)?;
        if row.step == "bound" || row.step == "refused" {
            return Ok(response(row));
        }
    }
    let (_, remote_url) = validate(state, &request)?;
    let mut row = match row {
        Some(row) => row,
        None => {
            wire::start(
                &mut tx,
                actor,
                &digest,
                &request,
                remote_url,
                (state.clock)().timestamp_millis(),
            )
            .await
            .map_err(db)?;
            let row = wire::load(&mut tx, op)
                .await
                .map_err(db)?
                .ok_or(AppError::Internal)?;
            check_operation(&row, actor, &digest)?;
            row
        }
    };
    tx.commit().await.map_err(db)?;
    advance(state, actor, &request, &mut row).await?;
    Ok(response(&row))
}

fn check_operation(row: &Row, actor: Uuid, digest: &str) -> Result<()> {
    if row.actor != actor || row.digest != digest {
        return Err(AppError::Conflict(
            "operation_id was used for a different onboarding request".into(),
        ));
    }
    Ok(())
}

async fn advance(state: &AppState, actor: Uuid, r: &OnboardRequest, row: &mut Row) -> Result<()> {
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
                // Spec 0016 §5.2: each task's branch `refs/heads/aq/<task-id>`.
                {
                    params["allowed_target_prefixes"] = json!(["refs/heads/aq/"]);
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

async fn adopt_route(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Json(request): Json<AdoptRequest>,
) -> Result<Json<Value>> {
    adopt(&state, actor.id, request).await.map(Json)
}

/// Shared owner entry point for the admin route and the local CLI.
pub async fn adopt(state: &AppState, actor: Uuid, request: AdoptRequest) -> Result<Value> {
    let digest = aq_profiles::digest(&json!({"kind":"repository.adopt","request":request}))
        .map_err(|_| bad("invalid adoption request"))?;
    Adoption {
        pool: &state.store.pool,
        socket: git_socket(state),
        onboarding: state.config.onboarding.as_ref(),
        github_app: state
            .config
            .delivery
            .as_ref()
            .and_then(|d| d.github.as_ref()),
    }
    .record(actor, digest, request, || {
        (state.clock)().timestamp_millis()
    })
    .await
    .map_err(|e| match e {
        AdoptionError::Invalid(reason) => bad(reason),
        AdoptionError::MissingProject => AppError::NotFound("project not found".into()),
        AdoptionError::Conflict(reason) => AppError::Conflict(reason.into()),
        AdoptionError::Unavailable(source) => AppError::RepositoryAdoptionUnavailable(source),
        AdoptionError::Database(e) => db(e),
    })
}
