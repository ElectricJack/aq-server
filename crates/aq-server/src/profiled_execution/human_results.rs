//! Owner-authorized metadata over exact retained native result claims.
//! No claim, stop receipt or process outcome establishes acceptance/settlement.
use crate::{AppState, authz::RequireOwner, error::AppError};
use crate::{profiled_execution::settlement::actor::ActorPin, store::User};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Semaphore;
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
pub const COVERAGE: &str = "recorded_legacy_native_only";
pub const NATIVE_V2_COVERAGE: &str = "recorded_native_v2";
#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
pub enum ReadProfile {
    #[serde(rename = "recorded_native_v2")]
    RecordedNativeV2,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultOrigin {
    LegacyPrepareV1,
    PhaseExecutionV2,
    PhaseRefreshExecutionV1,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultMetadata {
    /// Only opt-in v2 pages expose origin. Default v1 retains its old field set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ResultOrigin>,
    pub report_id: Uuid,
    pub revision: String,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub attempt_id: Uuid,
    pub recorded_at_ms: u64,
    pub conclusion: String,
    pub process_outcome: String,
    pub current_report_head: bool,
    pub head_report_id: Uuid,
    pub supersedes: Option<Uuid>,
    pub repository_id: Uuid,
    pub source_base: aq_admission::ObjectId,
    pub source_receipt_ids: Vec<Uuid>,
    pub pending_effect_ids: Vec<Uuid>,
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultPage {
    pub schema_version: u32,
    pub server_time_ms: u64,
    pub coverage: String,
    pub items: Vec<ResultMetadata>,
    pub next_after: Option<Uuid>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub profile: Option<ReadProfile>,
    pub project_id: Option<Uuid>,
    pub after: Option<Uuid>,
    pub limit: Option<u32>,
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn invalid() -> AppError {
    AppError::BadRequest("invalid result metadata query".into())
}
async fn owner(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, actor: Uuid) -> Result<()> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    if role.as_deref() != Some("owner") {
        return Err(AppError::Forbidden("result metadata requires Owner".into()));
    }
    Ok(())
}
fn metadata(v: super::results::RecordedResult, profile: Option<ReadProfile>) -> ResultMetadata {
    ResultMetadata {
        origin: profile.map(|_| match v.origin {
            super::results::RecordedOrigin::LegacyPrepareV1 => ResultOrigin::LegacyPrepareV1,
            super::results::RecordedOrigin::PhaseExecutionV2 => ResultOrigin::PhaseExecutionV2,
            super::results::RecordedOrigin::PhaseRefreshExecutionV1 => {
                ResultOrigin::PhaseRefreshExecutionV1
            }
        }),
        report_id: v.report.report_id,
        revision: v.receipt.report_digest.as_str().into(),
        project_id: v.spec.lifecycle.project_id,
        task_id: v.spec.task.task_id,
        attempt_id: v.report.binding.attempt_id,
        recorded_at_ms: v.receipt.recorded_at_ms,
        conclusion: v.report.conclusion.as_str().into(),
        process_outcome: match v.report.outcome {
            aq_admission::transport::ProcessOutcome::Success => "success",
            aq_admission::transport::ProcessOutcome::Failed => "failed",
            aq_admission::transport::ProcessOutcome::Cancelled => "cancelled",
            aq_admission::transport::ProcessOutcome::Expired => "expired",
            aq_admission::transport::ProcessOutcome::Unknown => "unknown",
        }
        .into(),
        current_report_head: v.is_current_report_head,
        head_report_id: v.head_report_id,
        supersedes: v.report.supersedes,
        repository_id: v.spec.source.repository_id,
        source_base: v.spec.source.resolved_object,
        source_receipt_ids: v.report.source_receipt_ids,
        pending_effect_ids: v.report.pending_effect_ids,
    }
}
/// Validated keyset discovery. UUID order is stable; it is not chronological Latest.
pub async fn read(
    state: &AppState,
    actor: Uuid,
    page: &Page,
    exact: Option<(Uuid, &str)>,
) -> Result<ResultPage> {
    if page.profile.is_some() {
        return Err(AppError::Forbidden(
            "native v2 result reads require the authenticated full principal".into(),
        ));
    }
    read_inner(state, actor, page, exact, None).await
}
/// The new profile retains the exact HTTP principal across common B1 lock waits.
/// The old UUID-only wrapper remains legacy-only for existing internal callers.
pub async fn read_as(
    state: &AppState,
    actor: &User,
    page: &Page,
    exact: Option<(Uuid, &str)>,
) -> Result<ResultPage> {
    if page.profile.is_none() {
        return read(state, actor.id, page, exact).await;
    }
    let pin = ActorPin::observe(state, actor).await?;
    read_inner(state, actor.id, page, exact, Some(pin)).await
}
async fn read_inner(
    state: &AppState,
    actor: Uuid,
    page: &Page,
    exact: Option<(Uuid, &str)>,
    pin: Option<ActorPin>,
) -> Result<ResultPage> {
    let limit = page.limit.unwrap_or(10);
    if !(1..=25).contains(&limit)
        || page.project_id.is_some_and(|v| v.is_nil())
        || page.after.is_some_and(|v| v.is_nil())
    {
        return Err(invalid());
    }
    if exact.is_some_and(|(id, revision)| {
        id.is_nil()
            || revision.len() != 64
            || !revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(invalid());
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    if pin.is_none() {
        owner(&mut tx, actor).await?;
    }
    let ids: Vec<Uuid> = if let Some((id, _)) = exact {
        vec![id]
    } else {
        sqlx::query_scalar("SELECT r.report_id FROM profiled_result_reports r JOIN profiled_launches l USING(attempt_id) WHERE ($4 OR NOT EXISTS(SELECT 1 FROM profiled_v2_accepts a WHERE a.attempt_id=r.attempt_id)) AND ($1::uuid IS NULL OR (l.execution_spec#>>'{lifecycle,project_id}')=$1::text) AND ($2::uuid IS NULL OR r.report_id>$2) ORDER BY r.report_id LIMIT $3")
            .bind(page.project_id).bind(page.after).bind(i64::from(limit)+1).bind(page.profile.is_some()).fetch_all(&mut *tx).await.map_err(db)?
    };
    let more = ids.len() > limit as usize;
    let mut items = Vec::new();
    for id in ids.into_iter().take(limit as usize) {
        let retained = super::results::load_locked(state, &mut tx, id).await?;
        if page.profile.is_none()
            && retained.origin != super::results::RecordedOrigin::LegacyPrepareV1
        {
            return Err(AppError::NotFound(
                "result origin is outside current metadata coverage".into(),
            ));
        }
        let value = metadata(retained, page.profile);
        if page.project_id.is_some_and(|p| p != value.project_id)
            || exact.is_some_and(|(_, r)| r != value.revision)
        {
            return Err(AppError::NotFound("result revision not found".into()));
        }
        items.push(value);
    }
    let next_after = if more {
        items.last().map(|v| v.report_id)
    } else {
        None
    };
    let server_time_ms =
        u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    // B1 takes common Node/attempt locks. Acquire human identity only afterward,
    // and retain the user SHARE lock through commit; never user -> common owner.
    if let Some(pin) = pin {
        pin.fence(&mut tx).await?;
    }
    tx.commit().await.map_err(db)?;
    Ok(ResultPage {
        schema_version: 1,
        server_time_ms,
        coverage: if page.profile.is_some() {
            NATIVE_V2_COVERAGE
        } else {
            COVERAGE
        }
        .into(),
        items,
        next_after,
    })
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/human/results", get(list))
        .route("/api/human/results/{id}/revisions/{revision}", get(detail))
        .layer(Extension(Arc::new(Semaphore::new(2))))
}
async fn bounded(
    state: &AppState,
    actor: User,
    page: Page,
    exact: Option<(Uuid, &str)>,
    budget: Arc<Semaphore>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    let _permit = budget
        .try_acquire()
        .map_err(|_| AppError::IdentityUnavailable("result reads busy".into()))?;
    let value = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        read_as(state, &actor, &page, exact),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("result read unavailable".into()))??;
    Ok(([("cache-control", "no-store")], Json(value)).into_response())
}
async fn list(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Query(page): Query<Page>,
    Extension(budget): Extension<Arc<Semaphore>>,
) -> Result<axum::response::Response> {
    bounded(&state, user, page, None, budget).await
}
async fn detail(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Path((id, revision)): Path<(Uuid, String)>,
    Query(page): Query<Page>,
    Extension(budget): Extension<Arc<Semaphore>>,
) -> Result<axum::response::Response> {
    bounded(&state, user, page, Some((id, &revision)), budget).await
}
