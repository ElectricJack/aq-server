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
    COVERAGE, NATIVE_V2_COVERAGE, Page, ReadProfile, ResultMetadata, ResultOrigin, ResultPage,
    UnavailableResult,
};
/// A stable, non-sensitive reason for a report that could not be read.
pub fn unavailable_reason(error: &AppError) -> &'static str {
    match error {
        AppError::NotFound(_) => "outside_coverage",
        AppError::Forbidden(_) | AppError::Conflict(_) | AppError::ExecutionCancelled => "refused",
        AppError::Busy | AppError::IdentityUnavailable(_) | AppError::Maintenance(_) => {
            "temporarily_unavailable"
        }
        _ => "validation_failed",
    }
}
use crate::error::db;
fn invalid() -> AppError {
    AppError::BadRequest("invalid result metadata query".into())
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
        return Err(invalid());
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
    let (mut items, mut unavailable, mut last) = (Vec::new(), Vec::new(), None);
    for id in ids.into_iter().take(limit as usize) {
        last = Some(id);
        if let (Some(project), Some(_)) = (page.project_id, exact) {
            // A report of another project is not found before any owner read.
            let inside = sql::in_project(&mut tx, id, project).await.map_err(db)?;
            if !inside {
                return Err(AppError::NotFound("result revision not found".into()));
            }
        }
        // A listed report is read in a savepoint so one refused validation
        // is reported in its place; an exact read still fails as a whole.
        let isolate = exact.is_none();
        if isolate {
            sql::savepoint(&mut tx).await.map_err(db)?;
        }
        let loaded = super::results::load_locked(state, &mut tx, id).await;
        if isolate {
            sql::settle(&mut tx, loaded.is_ok()).await.map_err(db)?;
        }
        let retained = match loaded {
            Ok(v) if v.origin.is_covered_by(page.profile) => v,
            Ok(_) if isolate => {
                unavailable.push(UnavailableResult::new(id, "outside_coverage"));
                continue;
            }
            Ok(_) => {
                let outside = "result origin is outside current metadata coverage";
                return Err(AppError::NotFound(outside.into()));
            }
            Err(e) if isolate => {
                unavailable.push(UnavailableResult::new(id, unavailable_reason(&e)));
                continue;
            }
            Err(e) => return Err(e),
        };
        let origin = page.profile.map(|_| retained.origin);
        let head = (retained.head_report_id, retained.is_current_report_head);
        let (report, receipt, spec) = (retained.report, &retained.receipt, retained.spec);
        let value = ResultMetadata::record(origin, report, receipt, spec, head);
        if value.is_outside(page.project_id, exact) {
            return Err(AppError::NotFound("result revision not found".into()));
        }
        items.push(value);
    }
    let next_after = if more { last } else { None };
    let server_time_ms =
        u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    // B1 takes common Node/attempt locks. Acquire human identity only afterward,
    // and retain the user SHARE lock through commit; never user -> common owner.
    if let Some(pin) = pin {
        pin.fence(&mut tx).await?;
    }
    tx.commit().await.map_err(db)?;
    Ok(ResultPage::new(
        server_time_ms,
        page.profile,
        (items, next_after),
        unavailable,
    ))
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
