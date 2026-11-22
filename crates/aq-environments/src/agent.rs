//! Spec 0017 §6.2: `environment.try`, an agent's build of one recipe from its
//! task's pushed head, run on a builder (the `try` job of [`crate::worker`]).
//!
//! The agent cannot push: its node's channel proxy commits the workspace,
//! pushes it to the task's branch `refs/heads/aq/<task>` and adds the pushed
//! commit to the body as `"commit"` before it forwards the request. The node
//! credential stands behind the channel, so the server takes that commit as
//! attested and reads no repository; a commit aq-git lacks fails the job at
//! the builder's fetch. A request without one (the command surface, an older
//! node) is refused with `workspace_not_pushed`.
use crate::worker::{Host, NewJob, enqueue};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) fn routes<S: Host<E>, E: IntoResponse + Send + 'static>() -> Router<S> {
    Router::new()
        .route("/v1/agent/environments/try", post(queue::<S, E>))
        .route("/v1/agent/environments/try/{job_id}", get(job::<S, E>))
}

/// A refusal in the agent-access-1 error body.
fn refuse(status: StatusCode, code: &str, message: &str, retryable: bool) -> Response {
    let body = json!({"api_version": 1, "code": code, "message": message,
        "effect": "not_applied", "retryable": retryable});
    (status, Json(body)).into_response()
}
fn invalid(message: &str) -> Response {
    refuse(StatusCode::BAD_REQUEST, "invalid_request", message, false)
}
fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "environment try failed");
    let status = StatusCode::INTERNAL_SERVER_ERROR;
    refuse(status, "internal", "internal server error", true)
}

/// The agent's body; the node's proxy adds `commit`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Try {
    api_version: u32,
    name: String,
    run: Option<String>,
    commit: Option<String>,
}
/// `[a-z0-9][a-z0-9-]{0,63}`, the file `.aq/environments/<name>.md`.
fn recipe_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=64).contains(&b.len())
        && b[0] != b'-'
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}
fn sha1_hex(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

async fn queue<S: Host<E>, E: IntoResponse>(
    State(host): State<S>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    queued(&host, &headers, &body).await.unwrap_or_else(|r| r)
}
async fn queued<S: Host<E>, E: IntoResponse>(
    host: &S,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, Response> {
    let project = host.agent(headers, "/v1/agent/environments/try").await?;
    let request: Try = serde_json::from_slice(body)
        .map_err(|_| invalid("the body is {api_version, name, run?, commit}"))?;
    if request.api_version != 1 {
        let message = "only api_version 1 is served";
        return Err(refuse(
            StatusCode::UPGRADE_REQUIRED,
            "unsupported_api_version",
            message,
            false,
        ));
    }
    if !recipe_name(&request.name) {
        return Err(invalid("name is a recipe name, [a-z0-9][a-z0-9-]{0,63}"));
    }
    let pool = host.as_ref();
    let (limits, _) = aq_role_settings::environment_limits(pool)
        .await
        .map_err(internal)?;
    let run = request.run.as_deref();
    if run.is_some_and(|r| r.is_empty() || r.len() as u64 > limits.max_arg_bytes) {
        return Err(invalid(
            "run is one command of 1 to environment_limits.max_arg_bytes bytes",
        ));
    }
    let commit = match request.commit.as_deref() {
        Some(c) if sha1_hex(c) => c,
        Some(_) => return Err(invalid("commit is 40 lowercase hex digits")),
        None => {
            let message = "no pushed commit came with the request: the node's channel pushes the workspace for `aq environment try`, and this request did not pass through it";
            return Err(refuse(
                StatusCode::CONFLICT,
                "workspace_not_pushed",
                message,
                false,
            ));
        }
    };
    let mut tx = pool.begin().await.map_err(internal)?;
    // One project's tries are counted and queued one request at a time.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('environment.try:'||$1::text,0))")
        .bind(project)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    let Some(base) = crate::store::base(&mut *tx).await.map_err(internal)? else {
        let message = "no AQ base template is admitted yet, so there is nothing to build on";
        return Err(refuse(
            StatusCode::CONFLICT,
            "no_base_template",
            message,
            false,
        ));
    };
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM environment_jobs WHERE project_id=$1 AND kind='try' AND state IN ('queued','claimed')",
    )
    .bind(project)
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    if u64::try_from(active).is_ok_and(|n| n >= limits.try_concurrency) {
        let message = format!(
            "the project has {active} try jobs queued or running, the most environment_limits.try_concurrency allows; retry when one completes"
        );
        let status = StatusCode::TOO_MANY_REQUESTS;
        return Err(refuse(status, "try_concurrency_reached", &message, true));
    }
    let recipes = [request.name];
    let job = NewJob {
        kind: "try",
        project_id: project,
        commit,
        recipes: &recipes,
        run,
        base_template: &base,
    };
    let job_id = enqueue(&mut *tx, &job).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(json!({"job_id": job_id})).into_response())
}

/// One of the project's try jobs: its state and, once completed, the
/// builder's Job result. Another project's job is not found.
async fn job<S: Host<E>, E: IntoResponse>(
    State(host): State<S>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let read = async {
        let path = "/v1/agent/environments/try/{job_id}";
        let project = host.agent(&headers, path).await?;
        let id: Uuid = id.parse().map_err(|_| invalid("job_id is a UUID"))?;
        let row: Option<(String, Option<Value>)> = sqlx::query_as(
            "SELECT state,result FROM environment_jobs WHERE job_id=$1 AND project_id=$2 AND kind='try'",
        )
        .bind(id)
        .bind(project)
        .fetch_optional(host.as_ref())
        .await
        .map_err(internal)?;
        let Some((state, result)) = row else {
            let message = "no such try job in this project";
            return Err(refuse(StatusCode::NOT_FOUND, "not_found", message, false));
        };
        Ok(Json(json!({"job_id": id, "state": state, "result": result})).into_response())
    };
    read.await.unwrap_or_else(|r: Response| r)
}

#[cfg(test)]
mod tests {
    #[test]
    fn recipe_names_and_commits_are_checked() {
        for good in ["web", "a", "0-x", &"w".repeat(64)] {
            assert!(super::recipe_name(good), "{good}");
        }
        for bad in ["", "-web", "Web", "we_b", "w.b", &"w".repeat(65)] {
            assert!(!super::recipe_name(bad), "{bad}");
        }
        assert!(super::sha1_hex(&"0a".repeat(20)));
        assert!(!super::sha1_hex(&"0A".repeat(20)));
        assert!(!super::sha1_hex(&"0".repeat(41)));
    }
}
