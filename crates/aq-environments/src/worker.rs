//! Spec 0017 §6.3, §6.6: the node routes (and, from [`crate::agent`], the
//! agent's `environment.try` routes). Builder nodes (the operator's
//! `template-build` worker group, `[projects.worker_groups]` in the server
//! config) claim and complete build and try jobs; every enrolled node reads
//! the admitted environments and reports which templates it has pulled.
//! Generic over the host server, which authenticates the node.
// Handlers refuse with a ready `Response`, as axum handlers do.
#![allow(clippy::result_large_err)]
use crate::{BootEnvironments, Catalog};
use aq_project_policy::ProjectConfig;
use aq_protocol::{ApiError, BAD_REQUEST, CONFLICT, FORBIDDEN, INTERNAL, NOT_FOUND};
use aq_role_settings::environments::EnvironmentLimits;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// The worker group whose nodes may claim and complete jobs.
pub const BUILDERS: &str = "template-build";
/// The most bytes a Job result may take on the wire, from the Settings: its
/// free text (logs, run output, toolchain output) totals at most `log_bytes`,
/// and the rest it reports comes from its recipes (`max_recipe_bytes`). JSON
/// escapes a byte into at most six (`\u0000`).
fn result_bytes(limits: &EnvironmentLimits) -> usize {
    let bytes = limits.log_bytes.saturating_add(limits.max_recipe_bytes);
    usize::try_from(bytes.saturating_mul(6)).unwrap_or(usize::MAX)
}

/// How the host server authenticates a node: the node's id, or the host's
/// own refusal `E` for the credential.
pub trait Host<E: IntoResponse>: AsRef<PgPool> + Clone + Send + Sync + 'static {
    fn node(&self, headers: &HeaderMap) -> impl Future<Output = Result<Uuid, E>> + Send;
    /// The attempt's project, once its agent credential holds the grant the
    /// agent route `path` needs; otherwise the host's refusal.
    fn agent(
        &self,
        headers: &HeaderMap,
        path: &str,
    ) -> impl Future<Output = Result<Uuid, Response>> + Send;
}

/// The routes, for the builders the operator's `projects` config lists in
/// its `template-build` worker group (read once, as the config is).
pub fn routes<S: Host<E>, E: IntoResponse + Send + 'static>(projects: &ProjectConfig) -> Router<S> {
    let builders: Builders = Arc::from(
        projects
            .worker_groups
            .get(BUILDERS)
            .cloned()
            .unwrap_or_default(),
    );
    Router::new()
        .route("/v1/worker/environments", get(admitted::<S, E>))
        .route("/v1/worker/environments/ready", post(ready::<S, E>))
        .route("/v1/worker/environments/claim", post(claim::<S, E>))
        .route(
            "/v1/worker/environments/complete",
            // Bounded per request by `result_bytes`, read from the Settings.
            post(complete::<S, E>).layer(DefaultBodyLimit::disable()),
        )
        .layer(Extension(builders))
        .merge(crate::agent::routes::<S, E>())
}
type Builders = Arc<[Uuid]>;

/// A job for a builder, queued by `environment.try` or `environment.build`.
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub project_id: Uuid,
    pub commit: &'a str,
    pub recipes: &'a [String],
    pub run: Option<&'a str>,
    pub base_template: &'a str,
}
/// Queues `job` with its project's repository (the binding's, as nodes key
/// it); a project without a binding has no job.
pub async fn enqueue<'e>(db: impl sqlx::PgExecutor<'e>, job: &NewJob<'_>) -> sqlx::Result<Uuid> {
    sqlx::query_scalar(
        "INSERT INTO environment_jobs(job_id,kind,project_id,commit,recipes,run,base_template,repository_id) \
         SELECT $1,$2,$3,$4,$5,$6,$7,(binding->>'repository_id')::uuid \
         FROM project_execution_bindings WHERE project_id=$3 RETURNING job_id",
    )
    .bind(Uuid::new_v4())
    .bind(job.kind)
    .bind(job.project_id)
    .bind(job.commit)
    .bind(json!(job.recipes))
    .bind(job.run)
    .bind(job.base_template)
    .fetch_one(db)
    .await
}

/// A refusal, in the body every worker route uses.
#[derive(Debug)]
enum Error {
    Forbidden(&'static str),
    NotFound(&'static str),
    BadRequest(String),
    Conflict(&'static str),
    TooLarge(String),
    Internal,
}
/// A store or Settings failure: logged with what it was doing, never the request.
fn internal(what: &str, node: Uuid, job: Option<Uuid>, e: impl std::fmt::Display) -> Error {
    tracing::error!(%node, job_id = ?job, error = %e, "environment {what} failed");
    Error::Internal
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, error, message) = match self {
            Error::Forbidden(m) => (StatusCode::FORBIDDEN, FORBIDDEN, m.to_owned()),
            Error::NotFound(m) => (StatusCode::NOT_FOUND, NOT_FOUND, m.to_owned()),
            Error::BadRequest(m) => (StatusCode::BAD_REQUEST, BAD_REQUEST, m),
            Error::Conflict(m) => (StatusCode::CONFLICT, CONFLICT, m.to_owned()),
            Error::TooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, BAD_REQUEST, m),
            Error::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                INTERNAL,
                "internal server error".into(),
            ),
        };
        let error = error.to_owned();
        (status, Json(ApiError { error, message })).into_response()
    }
}
type Reply = Result<Response, Response>;
fn refused(e: Error) -> Response {
    e.into_response()
}
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body)
        .map_err(|e| refused(Error::BadRequest(format!("request body: {e}"))))
}
async fn node<S: Host<E>, E: IntoResponse>(
    host: &S,
    headers: &HeaderMap,
) -> Result<Uuid, Response> {
    host.node(headers)
        .await
        .map_err(IntoResponse::into_response)
}
async fn builder<S: Host<E>, E: IntoResponse>(
    host: &S,
    builders: &[Uuid],
    headers: &HeaderMap,
) -> Result<Uuid, Response> {
    let node = node(host, headers).await?;
    match builders.contains(&node) {
        true => Ok(node),
        false => Err(refused(Error::Forbidden(
            "this node is not in the template-build worker group",
        ))),
    }
}

async fn admitted<S: Host<E>, E: IntoResponse>(State(host): State<S>, headers: HeaderMap) -> Reply {
    let node = node(&host, &headers).await?;
    let boot = BootEnvironments::new([], None);
    let catalog = Catalog::load(host.as_ref(), &boot)
        .await
        .map_err(|e| refused(internal("read", node, None, e)))?;
    Ok(Json(json!({"environments": catalog.admitted_for_nodes()})).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    ready: Vec<String>,
}
async fn ready<S: Host<E>, E: IntoResponse>(
    State(host): State<S>,
    headers: HeaderMap,
    body: Bytes,
) -> Reply {
    let node = node(&host, &headers).await?;
    let report: Ready = parse(&body)?;
    let kept: Value = sqlx::query_scalar(
        "INSERT INTO environment_ready(node_id,digests,reported_at) \
         SELECT $1,COALESCE(jsonb_agg(DISTINCT template_digest ORDER BY template_digest),'[]'),now() \
         FROM environment_versions WHERE template_digest=ANY($2) \
         ON CONFLICT(node_id) DO UPDATE SET digests=excluded.digests,reported_at=excluded.reported_at \
         RETURNING digests",
    )
    .bind(node)
    .bind(&report.ready)
    .fetch_one(host.as_ref())
    .await
    .map_err(|e| refused(internal("ready report", node, None, e)))?;
    Ok(Json(json!({"ready": kept})).into_response())
}

type Claimed = (
    Uuid,
    String,
    Uuid,
    String,
    Uuid,
    String,
    Value,
    Option<String>,
    String,
);
async fn claim<S: Host<E>, E: IntoResponse>(
    State(host): State<S>,
    Extension(builders): Extension<Builders>,
    headers: HeaderMap,
) -> Reply {
    let node = builder(&host, &builders, &headers).await?;
    let pool = host.as_ref();
    let limits = aq_role_settings::environment_limits(pool).await;
    let (limits, _) = limits.map_err(|e| refused(internal("claim", node, None, e)))?;
    let lease_ms = i64::try_from(limits.build_wall_ms).unwrap_or(i64::MAX);
    let max_claims = i64::try_from(limits.max_job_claims).unwrap_or(i64::MAX);
    (exhausted(pool, &limits).await).map_err(|e| refused(internal("claim", node, None, e)))?;
    // The oldest job that is queued or whose lease ran out, below its claim
    // cap; a job whose project has no binding (so no name) is never handed out.
    let job: Option<Claimed> = sqlx::query_as(
        "WITH next AS (SELECT j.job_id,b.binding->>'name' AS project FROM environment_jobs j \
         JOIN project_execution_bindings b USING(project_id) \
         WHERE (j.state='queued' OR (j.state='claimed' AND j.lease_until<=now())) AND j.claims<$3 \
         ORDER BY j.created_at,j.job_id LIMIT 1 FOR UPDATE OF j SKIP LOCKED) \
         UPDATE environment_jobs j SET state='claimed',claimed_by=$1,claimed_at=now(),claims=j.claims+1,\
         lease_until=now()+make_interval(secs=>$2::double precision/1000) FROM next WHERE j.job_id=next.job_id \
         RETURNING j.job_id,j.kind,j.project_id,next.project,j.repository_id,j.commit,j.recipes,j.run,j.base_template",
    )
    .bind(node)
    .bind(lease_ms)
    .bind(max_claims)
    .fetch_optional(pool)
    .await
    .map_err(|e| refused(internal("claim", node, None, e)))?;
    let Some((
        job_id,
        kind,
        project_id,
        project,
        repository_id,
        commit,
        recipes,
        run,
        base_template,
    )) = job
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    Ok(Json(json!({
        "job_id": job_id, "kind": kind, "project_id": project_id, "project": project,
        "repository_id": repository_id, "commit": commit, "recipes": recipes, "run": run,
        "base_template": base_template, "limits": limits,
    }))
    .into_response())
}
/// Completes as failed, at `fetch`, every job whose lease ran out after
/// `max_job_claims` claims: none of its builders reported a result, so one
/// more claim would not either.
async fn exhausted(pool: &PgPool, limits: &EnvironmentLimits) -> sqlx::Result<()> {
    let max = i64::try_from(limits.max_job_claims).unwrap_or(i64::MAX);
    let mut tx = pool.begin().await?;
    let failed: Vec<(Uuid, Uuid, String, i64)> = sqlx::query_as(
        "UPDATE environment_jobs SET state='completed',completed_at=now(),result=jsonb_build_object(\
         'job_id',job_id,'outcome','failed','builds','[]'::jsonb,'run',NULL,'failure',jsonb_build_object(\
         'recipe',COALESCE(recipes->>0,''),'step','fetch','log',format(\
         'the job was claimed %s times without a result (environment_limits.max_job_claims is %s)',claims,$1::bigint))) \
         WHERE state='claimed' AND lease_until<=now() AND claims>=$1 \
         RETURNING job_id,project_id,kind,claims::bigint",
    )
    .bind(max)
    .fetch_all(&mut *tx)
    .await?;
    for (job_id, project, kind, claims) in failed {
        tracing::warn!(%job_id, claims, "environment job claimed {claims} times without a result: completed as failed");
        prune(&mut tx, project, &kind, limits).await?;
    }
    tx.commit().await
}
/// Rolling records: the project keeps its newest `keep_versions` completed
/// jobs of `kind`.
async fn prune(
    tx: &mut sqlx::PgConnection,
    project: Uuid,
    kind: &str,
    limits: &EnvironmentLimits,
) -> sqlx::Result<()> {
    let keep = i64::try_from(limits.keep_versions).unwrap_or(i64::MAX);
    sqlx::query(
        "DELETE FROM environment_jobs WHERE job_id IN (SELECT job_id FROM (SELECT job_id,\
         row_number() OVER (ORDER BY completed_at DESC,job_id DESC) AS n FROM environment_jobs \
         WHERE project_id=$1 AND kind=$2 AND state='completed') o WHERE n>$3)",
    )
    .bind(project)
    .bind(kind)
    .bind(keep)
    .execute(tx)
    .await?;
    Ok(())
}

/// The Job result shape (`POST /v1/worker/environments/complete`).
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct JobResult {
    job_id: Uuid,
    outcome: Outcome,
    builds: Vec<Build>,
    failure: Option<Failure>,
    run: Option<Run>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Built,
    Failed,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Build {
    name: String,
    digest: String,
    image: String,
    recipe_sha256: String,
    provides: Vec<String>,
    toolchain: Vec<Toolchain>,
    warm: Vec<String>,
    harnesses: Vec<Harness>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Toolchain {
    command: String,
    output: String,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Harness {
    harness: String,
    version: String,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Failure {
    recipe: String,
    step: Step,
    log: String,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Step {
    /// Written by the server for a job claimed past `max_job_claims`.
    Fetch,
    Parse,
    Install,
    Check,
    Scan,
    Publish,
    Run,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Run {
    exit: i64,
    output: String,
}
/// Whether `result` is a possible answer to a job of `kind`.
fn check(
    result: &JobResult,
    (kind, run): (&str, bool),
    limits: &EnvironmentLimits,
) -> Result<(), String> {
    let digest = |d: &str| {
        d.strip_prefix("sha256:").is_some_and(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    };
    match (&result.outcome, &result.failure) {
        (Outcome::Built, Some(_)) => return Err("a built outcome has no failure".into()),
        (Outcome::Failed, None) => return Err("a failed outcome names its failure".into()),
        _ => {}
    }
    if (kind == "try" || result.outcome == Outcome::Failed) && !result.builds.is_empty() {
        return Err("only a built build job has builds".into());
    }
    // A built try ran its command, when the job has one.
    if kind == "try" && result.outcome == Outcome::Built && result.run.is_some() != run {
        return Err("a built try job reports the run of its command, and only then".into());
    }
    if kind == "build" && result.run.is_some() {
        return Err("a build job has no run".into());
    }
    if kind == "build" && result.outcome == Outcome::Built && result.builds.is_empty() {
        return Err("a built build job has builds".into());
    }
    if !result.builds.iter().all(|b| digest(&b.digest)) {
        return Err("a build digest is sha256:<64 hex>".into());
    }
    let toolchain = (result.builds.iter()).flat_map(|b| b.toolchain.iter().map(|t| t.output.len()));
    let text = (result.failure.iter().map(|f| f.log.len()))
        .chain(result.run.iter().map(|r| r.output.len()))
        .chain(toolchain);
    if !u64::try_from(text.sum::<usize>()).is_ok_and(|n| n <= limits.log_bytes) {
        return Err("the logs, run output and toolchain output are longer than environment_limits.log_bytes together".into());
    }
    Ok(())
}
async fn complete<S: Host<E>, E: IntoResponse>(
    State(host): State<S>,
    Extension(builders): Extension<Builders>,
    headers: HeaderMap,
    body: Body,
) -> Reply {
    let node = builder(&host, &builders, &headers).await?;
    let limits = aq_role_settings::environment_limits(host.as_ref()).await;
    let (limits, _) = limits.map_err(|e| refused(internal("complete", node, None, e)))?;
    let bound = result_bytes(&limits);
    let body = axum::body::to_bytes(body, bound).await.map_err(|_| {
        refused(Error::TooLarge(format!(
            "a job result is at most {bound} bytes (from environment_limits)"
        )))
    })?;
    let result: JobResult = parse(&body)?;
    recorded(host.as_ref(), node, &result, &limits)
        .await
        .map_err(refused)?;
    Ok(Json(json!({"job_id": result.job_id, "recorded": true})).into_response())
}
/// Record `result` once; the same result again is the same answer.
async fn recorded(
    pool: &PgPool,
    node: Uuid,
    result: &JobResult,
    limits: &EnvironmentLimits,
) -> Result<(), Error> {
    let failed = |e| internal("complete", node, Some(result.job_id), e);
    let mut tx = pool.begin().await.map_err(failed)?;
    type Row = (
        String,
        Option<String>,
        Uuid,
        String,
        Option<Uuid>,
        Option<Value>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT kind,run,project_id,state,claimed_by,result FROM environment_jobs WHERE job_id=$1 FOR UPDATE",
    )
    .bind(result.job_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(failed)?;
    let (kind, run, project, state, claimed_by, stored) =
        row.ok_or(Error::NotFound("no such environment job"))?;
    if claimed_by != Some(node) {
        return Err(Error::Conflict("this node does not hold the job's claim"));
    }
    if state == "completed" {
        let same = stored
            .and_then(|s| serde_json::from_value::<JobResult>(s).ok())
            .as_ref()
            == Some(result);
        return match same {
            true => Ok(()),
            false => Err(Error::Conflict("the job was completed with another result")),
        };
    }
    check(result, (&kind, run.is_some()), limits).map_err(Error::BadRequest)?;
    let value = serde_json::to_value(result);
    let value = value.map_err(|e| internal("complete", node, Some(result.job_id), e))?;
    sqlx::query("UPDATE environment_jobs SET state='completed',completed_at=now(),result=$2 WHERE job_id=$1")
        .bind(result.job_id)
        .bind(value)
        .execute(&mut *tx)
        .await
        .map_err(failed)?;
    // This job is the project's newest completed one of its kind.
    prune(&mut tx, project, &kind, limits)
        .await
        .map_err(failed)?;
    tx.commit().await.map_err(failed)?;
    Ok(())
}
