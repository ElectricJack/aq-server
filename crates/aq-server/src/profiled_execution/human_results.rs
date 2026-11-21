//! Owner-authorized metadata over exact retained native result claims.
//! No claim, stop receipt or process outcome establishes acceptance/settlement.
use crate::{AppState, authz::RequireOwner, error::AppError};
use crate::{profiled_execution::settlement::actor::ActorPin, store::User};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use std::sync::Arc;
use tokio::sync::Semaphore;
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
pub use aq_dashboard_contract::result_metadata::{
    COVERAGE, Collector, NATIVE_V2_COVERAGE, Page, ReadProfile, ResultMetadata, ResultOrigin,
    ResultPage,
};
/// A stable, non-sensitive reason for a report that could not be read.
pub fn unavailable_reason(error: &AppError) -> &'static str {
    use AppError::{Busy, Conflict, ExecutionCancelled, Forbidden, NotFound};
    use AppError::{IdentityUnavailable as Unavailable, Maintenance};
    match error {
        NotFound(_) => "outside_coverage",
        Forbidden(_) | Conflict(_) | ExecutionCancelled => "refused",
        Busy | Unavailable(_) | Maintenance(_) => "temporarily_unavailable",
        _ => "validation_failed",
    }
}
use crate::error::db;
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
    read_inner(state, Some(actor), page, exact, None).await
}
/// Project-scoped read for an attempt that holds `result.read` at project
/// scope. The caller has authorized `project`; the filter is forced to it, so
/// a report of another project is never loaded or revealed.
pub async fn read_project(
    state: &AppState,
    project: Uuid,
    after: Option<Uuid>,
    limit: Option<u32>,
    exact: Option<(Uuid, &str)>,
) -> Result<ResultPage> {
    let page = Page {
        profile: Some(ReadProfile::RecordedNativeV2),
        project_id: Some(project),
        after,
        limit,
    };
    read_inner(state, None, &page, exact, None).await
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
    read_inner(state, None, page, exact, Some(pin)).await
}
async fn read_inner(
    state: &AppState,
    owner_check: Option<Uuid>,
    page: &Page,
    exact: Option<(Uuid, &str)>,
    pin: Option<ActorPin>,
) -> Result<ResultPage> {
    use aq_profiled_state::result_reads as sql;
    let limit = page.limit.unwrap_or(sql::DEFAULT_LIMIT);
    if !sql::query_is_valid(limit, page.project_id, page.after, exact) {
        return Err(AppError::BadRequest("invalid result metadata query".into()));
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sql::bound_waits(&mut tx).await.map_err(db)?;
    if let Some(actor) = owner_check
        && !sql::is_owner(&mut tx, actor).await.map_err(db)?
    {
        return Err(AppError::Forbidden("result metadata requires Owner".into()));
    }
    let ids: Vec<Uuid> = if let Some((id, _)) = exact {
        vec![id]
    } else {
        let (v2, after) = (page.profile.is_some(), page.after);
        sql::page_ids(&mut tx, v2, page.project_id, after, i64::from(limit) + 1)
            .await
            .map_err(db)?
    };
    let more = ids.len() > limit as usize;
    let mut found = Collector::new(page.profile, page.project_id, exact.map(|(_, r)| r));
    for id in ids.into_iter().take(limit as usize) {
        if let (Some(project), Some(_)) = (page.project_id, exact)
            && !sql::in_project(&mut tx, id, project).await.map_err(db)?
        {
            // A report of another project is not found before any owner read.
            return Err(AppError::NotFound("result revision not found".into()));
        }
        // A listed report is read in a savepoint so one refused validation
        // is reported in its place; an exact read still fails as a whole.
        let isolate = exact.is_none();
        sql::savepoint(&mut tx, isolate).await.map_err(db)?;
        let loaded = super::results::load_locked(state, &mut tx, id).await;
        sql::settle(&mut tx, isolate, loaded.is_ok())
            .await
            .map_err(db)?;
        let outside =
            || AppError::NotFound("result origin is outside current metadata coverage".into());
        let retained = match loaded.and_then(|v| {
            v.origin
                .is_covered_by(page.profile)
                .then_some(v)
                .ok_or_else(outside)
        }) {
            Ok(v) => v,
            Err(e) if isolate => {
                found.unavailable(id, unavailable_reason(&e));
                continue;
            }
            Err(e) => return Err(e),
        };
        let head = (retained.head_report_id, retained.is_current_report_head);
        let recorded = (
            retained.origin,
            retained.report,
            &retained.receipt,
            retained.spec,
        );
        if !found.record(id, recorded, head) {
            return Err(AppError::NotFound("result revision not found".into()));
        }
    }
    let server_time_ms =
        u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    // B1 takes common Node/attempt locks. Acquire human identity only afterward,
    // and retain the user SHARE lock through commit; never user -> common owner.
    if let Some(pin) = pin {
        pin.fence(&mut tx).await?;
    }
    tx.commit().await.map_err(db)?;
    Ok(found.finish(server_time_ms, more))
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
