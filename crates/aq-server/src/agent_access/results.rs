//! A reviewing attempt's read of its project's retained results and where
//! each result's work is kept (`result.read@project:self`). Reads only: it
//! never approves, settles or recreates work, and shows no other project.
use super::{Error as AgentError, authenticate, grant_check, id as field, ok};
use crate::{AppState, error::AppError, profiled_execution::human_results};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use uuid::Uuid;

const LIST: &str = "/v1/agent/results/list";
const SHOW: &str = "/v1/agent/results/show";
static BUDGET: Semaphore = Semaphore::const_new(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    api_version: u32,
    after: Option<Uuid>,
    limit: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Show {
    api_version: u32,
    report_id: Uuid,
    revision: String,
}

pub(crate) fn machine_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/agent/results/list", post(list))
        .route("/v1/agent/results/show", post(show))
}
/// The attempt's project, once the credential holds `result.read` for it. The
/// project comes from the binding and never from the body.
async fn authorize(s: &AppState, h: &HeaderMap, path: &str) -> Result<Uuid, Response> {
    let p = authenticate(s, h)
        .await
        .map_err(IntoResponse::into_response)?;
    grant_check::require(s, h, &p, path, None).await?;
    field(&p.binding, "project_id").map_err(IntoResponse::into_response)
}
fn parse<T: for<'de> Deserialize<'de>>(
    body: &[u8],
    version: impl Fn(&T) -> u32,
) -> Result<T, Response> {
    let v: T =
        serde_json::from_slice(body).map_err(|_| AgentError("invalid_request").into_response())?;
    if version(&v) != 1 {
        return Err(AgentError("unsupported_api_version").into_response());
    }
    Ok(v)
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
/// The bounded read: two at a time and four seconds, as the human routes.
async fn bounded<T>(
    read: impl std::future::Future<Output = Result<T, AppError>>,
) -> Result<T, Response> {
    let _permit = BUDGET
        .try_acquire()
        .map_err(|_| AgentError("overloaded").into_response())?;
    tokio::time::timeout(std::time::Duration::from_secs(4), read)
        .await
        .map_err(|_| AgentError("overloaded").into_response())?
        .map_err(refuse)
}
async fn list(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let project = match authorize(&s, &h, LIST).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = match parse::<List>(&body, |q| q.api_version) {
        Ok(q) => q,
        Err(r) => return r,
    };
    match bounded(human_results::read_project(
        &s, project, q.after, q.limit, None,
    ))
    .await
    {
        Ok(page) => ok(json!({
            "api_version": 1, "project_id": project, "coverage": page.coverage,
            "complete": !page.incomplete, "unavailable": page.unavailable,
            "items": page.items, "next_after": page.next_after,
        })),
        Err(r) => r,
    }
}
async fn show(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let project = match authorize(&s, &h, SHOW).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = match parse::<Show>(&body, |q| q.api_version) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let read = async {
        let page =
            human_results::read_project(&s, project, None, None, Some((q.report_id, &q.revision)))
                .await?;
        let item = page
            .items
            .into_iter()
            .next()
            .ok_or(AppError::NotFound("result revision not found".into()))?;
        let recorded = aq_delivery::result_work::recorded(&s.store.pool, item.report_id)
            .await
            .map_err(|_| AppError::Internal)?;
        let work = aq_delivery::result_work::describe(
            &aq_delivery::result_work::Subject {
                report_id: item.report_id,
                revision: &item.revision,
                attempt_id: item.attempt_id,
                source_repository_id: item.repository_id,
                source_base: item.source_base.hex.as_str(),
                source_receipt_ids: &item.source_receipt_ids,
            },
            &recorded,
        );
        Ok::<Value, AppError>(json!({
            "api_version": 1, "project_id": project, "coverage": page.coverage,
            "complete": true, "result": item, "work": work,
        }))
    };
    match bounded(read).await {
        Ok(v) => ok(v),
        Err(r) => r,
    }
}
