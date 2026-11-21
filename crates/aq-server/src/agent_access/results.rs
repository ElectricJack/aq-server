//! A reviewing attempt's reads of its project's retained results and their
//! exact patch (`result.read`). Reads only: never approves, settles or recreates work.
use super::{Error as AgentError, authenticate, grant_check, id as field, ok};
use crate::{AppState, error::AppError, profiled_execution::human_results};
use aq_agent_api::result_requests::{List, Patch, Show, Versioned};
use aq_delivery::{diff_patch::MAX_PATCH_BYTES, result_patch::Retained};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Semaphore;
use uuid::Uuid;
const LIST: &str = "/v1/agent/results/list";
const SHOW: &str = "/v1/agent/results/show";
const PATCH: &str = "/v1/agent/results/patch";
static BUDGET: Semaphore = Semaphore::const_new(2);
/// One patch at a time: each holds up to 4 MiB raw and its base64 text.
static PATCH_BUDGET: Semaphore = Semaphore::const_new(1);
pub(crate) fn machine_routes() -> Router<AppState> {
    Router::new()
        .route(LIST, post(list))
        .route(SHOW, post(show))
        .route(PATCH, post(patch))
}
/// One authorized read: the credential must hold `result.read` for the
/// attempt's project (which comes from the binding, never the body), the
/// request is parsed at `api_version` 1, and `run` is held to `budget` and
/// `seconds`.
async fn serve<
    Q: for<'de> Deserialize<'de> + Versioned,
    F: std::future::Future<Output = Result<Value, AppError>>,
>(
    (s, h, body): (&AppState, &HeaderMap, &[u8]),
    path: &str,
    (budget, seconds): (&Semaphore, u64),
    run: impl FnOnce(Uuid, Q) -> F,
) -> Response {
    let go = async {
        let p = authenticate(s, h)
            .await
            .map_err(IntoResponse::into_response)?;
        grant_check::require(s, h, &p, path, None).await?;
        let project = field(&p.binding, "project_id").map_err(IntoResponse::into_response)?;
        let q: Q = serde_json::from_slice(body)
            .map_err(|_| AgentError("invalid_request").into_response())?;
        if q.api_version() != 1 {
            return Err(AgentError("unsupported_api_version").into_response());
        }
        let _permit = budget
            .try_acquire()
            .map_err(|_| AgentError("overloaded").into_response())?;
        let read = run(project, q);
        tokio::time::timeout(std::time::Duration::from_secs(seconds), read)
            .await
            .map_err(|_| AgentError("overloaded").into_response())?
            .map_err(refuse)
    };
    go.await.map_or_else(|r: Response| r, ok)
}
fn refuse(error: AppError) -> Response {
    AgentError(match error {
        AppError::NotFound(_) => "not_found",
        AppError::BadRequest(_) => "invalid_request",
        AppError::Busy | AppError::IdentityUnavailable(_) | AppError::Maintenance(_) => {
            "overloaded"
        }
        _ => "internal",
    })
    .into_response()
}
/// The listing: two reads at a time and four seconds, as the human routes.
async fn list(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let s = &s;
    serve::<List, _>(
        (s, &h, &body),
        LIST,
        (&BUDGET, 4),
        |project, q| async move {
            let page = human_results::read_project(s, project, q.after, q.limit, None).await?;
            Ok(page.agent_json(project))
        },
    )
    .await
}
/// One exact revision of the attempt's project. A report of another project,
/// a revision that is not the report's current digest and an unknown report
/// are all `NotFound`.
async fn retained(
    s: &AppState,
    project: Uuid,
    report: Uuid,
    revision: &str,
) -> Result<Retained, AppError> {
    let page =
        human_results::read_project(s, project, None, None, Some((report, revision))).await?;
    let found = Retained::from_page(&s.store.pool, page).await;
    (found.map_err(|_| AppError::Internal)?)
        .ok_or(AppError::NotFound("result revision not found".into()))
}
async fn show(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let s = &s;
    serve::<Show, _>(
        (s, &h, &body),
        SHOW,
        (&BUDGET, 4),
        |project, q| async move {
            Ok(retained(s, project, q.report_id, &q.revision)
                .await?
                .show(project))
        },
    )
    .await
}
/// The answer of `result.review.patch` for `project`'s exact `revision` of
/// `report_id`: [`AppError::NotFound`] for another project's report, an unknown
/// report or a revision that is not the report's digest.
pub async fn read_patch(
    s: &AppState,
    project: Uuid,
    report_id: Uuid,
    revision: &str,
    max_bytes: u64,
) -> Result<Value, AppError> {
    let socket = (s.source_retention.as_ref()).map(|r| r.config.git_socket.as_path());
    let retained = retained(s, project, report_id, revision).await?;
    (retained.patch(project, socket, max_bytes).await).map_err(|_| AppError::Busy)
}
/// One patch at a time and 25 seconds: each holds up to 4 MiB and its base64.
async fn patch(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let s = &s;
    serve::<Patch, _>(
        (s, &h, &body),
        PATCH,
        (&PATCH_BUDGET, 25),
        |project, q| async move {
            let max = q
                .bound(MAX_PATCH_BYTES)
                .ok_or(AppError::BadRequest("max_bytes".into()))?;
            read_patch(s, project, q.report_id, &q.revision, max).await
        },
    )
    .await
}
