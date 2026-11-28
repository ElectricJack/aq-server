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
    self as wire, Adoption, AdoptionError, Git, Github, Meta, Row, Transition, label, response,
    slug,
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

mod advance;
use advance::advance;

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
        wire::Progress { step, state: value },
        (state.clock)().timestamp_millis(),
    )
    .await
    .map_err(db)
}

fn validate(state: &AppState, r: &OnboardRequest) -> Result<(ProjectBinding, String)> {
    validate_identity(r)?;
    let config = state
        .config
        .onboarding
        .as_ref()
        .ok_or_else(|| bad("repository onboarding is not configured"))?;
    validate_credential(config, r)?;
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
fn validate_identity(r: &OnboardRequest) -> Result<()> {
    if [r.operation_id, r.project_id, r.repository_id]
        .iter()
        .any(Uuid::is_nil)
    {
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
    Ok(())
}

fn validate_credential(config: &OnboardingConfig, r: &OnboardRequest) -> Result<()> {
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
    Ok(())
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
                wire::RequestOwner {
                    actor,
                    digest: &digest,
                },
                wire::NewOnboarding {
                    request: &request,
                    remote_url,
                },
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
    .record(
        wire::RequestOwner {
            actor,
            digest: &digest,
        },
        request,
        || (state.clock)().timestamp_millis(),
    )
    .await
    .map_err(|e| match e {
        AdoptionError::Invalid(reason) => bad(reason),
        AdoptionError::MissingProject => AppError::NotFound("project not found".into()),
        AdoptionError::Conflict(reason) => AppError::Conflict(reason.into()),
        AdoptionError::Unavailable(source) => AppError::RepositoryAdoptionUnavailable(source),
        AdoptionError::Database(e) => db(e),
    })
}
