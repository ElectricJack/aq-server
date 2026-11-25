//! Fleet performance reads reuse the dashboard's human authorization fence and
//! bounded read pool. Independent telemetry sources can have different coverage.
use super::{contract::*, cursor, service, *};
use serde_json::{Value, json};

pub(crate) mod capacity;
mod outcomes;
mod scheduling;
mod usage;

pub(crate) use outcomes::settings_routes;

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    project_id: Option<Uuid>,
    cutoff_ms: Option<i64>,
    cursor: Option<String>,
    limit: Option<u32>,
}

struct ReportQuery(Filters);
impl FromRequestParts<AppState> for ReportQuery {
    type Rejection = Error;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self> {
        Query::<Filters>::from_request_parts(parts, state)
            .await
            .map(|Query(q)| Self(q))
            .map_err(|_| Error::Invalid)
    }
}

#[derive(Clone, Copy)]
enum Report {
    Summary,
    Machines,
    Holds,
    Task(Uuid),
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/performance/summary", get(summary))
        .route("/api/v1/performance/machines", get(machines))
        .route("/api/v1/performance/holds", get(holds))
        .route("/api/v1/performance/tasks/{id}", get(task))
}

type Reply = Result<Json<Envelope<Value>>>;

async fn summary(r: DashboardRequest, ReportQuery(q): ReportQuery) -> Reply {
    report(r, q, Report::Summary).await
}
async fn machines(r: DashboardRequest, ReportQuery(q): ReportQuery) -> Reply {
    report(r, q, Report::Machines).await
}
async fn holds(r: DashboardRequest, ReportQuery(q): ReportQuery) -> Reply {
    report(r, q, Report::Holds).await
}
async fn task(r: DashboardRequest, ReportQuery(q): ReportQuery, Path(id): Path<Uuid>) -> Reply {
    report(r, q, Report::Task(id)).await
}

async fn section(read: impl std::future::Future<Output = anyhow::Result<Value>>) -> Value {
    match tokio::time::timeout(Duration::from_secs(4), read).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            tracing::debug!(%error, "performance source read failed");
            json!({"state":"unavailable","reason":"source_unavailable"})
        }
        Err(_) => json!({"state":"unavailable","reason":"source_timeout"}),
    }
}

async fn report(r: DashboardRequest, q: Filters, kind: Report) -> Reply {
    r.reply(6, || Ok(read(&r, q, kind))).await
}

async fn read(r: &DashboardRequest, q: Filters, kind: Report) -> Result<Envelope<Value>> {
    let (state, runtime, user) = r.parts();
    let auth = service::auth(state, runtime, user).await?;
    // AQ's human project and task inspection currently require the owner role.
    // A grant on one session must never grant project-level statistics.
    if !auth.owner {
        return Err(
            AppError::Forbidden("Performance requires project inspection access".into()).into(),
        );
    }
    let now = service::stamp(state)? as i64;
    let cutoff = q.cutoff_ms.unwrap_or(now);
    if cutoff <= 0
        || cutoff > now
        || now - cutoff > WEEK_MS
        || q.project_id.is_some_and(|id| id.is_nil())
        || matches!(kind, Report::Machines) && q.project_id.is_some()
        || !matches!(kind, Report::Holds) && (q.limit.is_some() || q.cursor.is_some())
    {
        return Err(Error::Invalid);
    }
    let scope = q
        .project_id
        .map_or(Scope::AllAuthorized, |project_id| Scope::Project {
            project_id,
        });
    let mut view = state.clone();
    let pool = &runtime.performance_pool;
    view.store.observation_pool = pool.clone();
    let mut data = json!({"definition_version":1,"cutoff_ms":cutoff});
    match kind {
        Report::Summary => {
            let (outcomes, usage) = tokio::join!(
                section(outcomes::summary(pool, q.project_id, cutoff)),
                section(usage::summary(&view, q.project_id, cutoff))
            );
            data["outcomes"] = outcomes;
            data["usage"] = usage;
        }
        Report::Machines => {
            let (machines, scheduling) = tokio::join!(
                section(capacity::machines(pool, cutoff)),
                section(scheduling::diagnostics(pool, cutoff))
            );
            data["machines"] = machines;
            data["scheduling"] = scheduling;
        }
        Report::Task(id) => {
            if id.is_nil() {
                return Err(Error::Invalid);
            }
            let (usage, first_pass) = tokio::join!(
                section(usage::task(&view, q.project_id, id, cutoff)),
                section(outcomes::first_pass(pool, q.project_id, id, cutoff))
            );
            data["usage"] = usage;
            data["first_pass"] = first_pass;
            if data["usage"].is_null() && data["first_pass"].is_null() {
                return Err(AppError::NotFound("task usage not found".into()).into());
            }
        }
        Report::Holds => {
            let limit = q.limit.unwrap_or(25);
            if !(1..=100).contains(&limit) || q.cursor.is_some() && q.cutoff_ms.is_none() {
                return Err(Error::Invalid);
            }
            let binding = cursor::Binding {
                principal: user.id,
                authorization_partition: auth.partition.clone(),
                scope: json!(scope),
                filters: json!({"cutoff_ms":cutoff}),
                sort: "since,project,task".into(),
                page_size: limit as u16,
                resource: "performance.holds".into(),
            };
            let key = q
                .cursor
                .as_deref()
                .map(|token| runtime.codec.decode(token, &binding, now as u64))
                .transpose()
                .map_err(|err| match err {
                    cursor::CursorError::AuthorizationChanged => Error::AuthorizationChanged,
                    _ => Error::Restart,
                })?;
            let position = key.as_ref().map(|k| k.position.as_str());
            if position.is_some_and(|p| !capacity::valid_hold_cursor(p)) {
                return Err(Error::Invalid);
            }
            let mut holds =
                section(capacity::holds(pool, q.project_id, now, position, limit)).await;
            if let Some(next) = holds["next_cursor"].as_str() {
                let token = runtime
                    .codec
                    .encode(binding.position("holds", next, now as u64..now as u64 + 300_000))
                    .map_err(|_| Error::Unavailable)?;
                holds["next_cursor"] = json!(token);
            }
            data["holds"] = holds;
        }
    }
    service::fence(state, runtime, user, &auth).await?;
    let partial = data.as_object().is_some_and(|obj| {
        obj.values().any(|v| {
            v["state"] == "unavailable" || v["state"] == "partial" || v["status"] == "partial"
        })
    });
    service::envelope(
        state,
        scope,
        data,
        partial.then_some("performance_coverage_partial"),
    )
}
