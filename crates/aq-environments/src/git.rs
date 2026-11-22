//! Spec 0017 §6.3 (with `delivery_playbook`): a builder fetches its job's
//! commit through the server's `/v1/git/<repository>/…` route with its node
//! credential and `AQ-Environment-Job: <job_id>` instead of `AQ-Attempt`.
//! The job grant is fetch-only, for the job's repository, while the node
//! holds the job's live claim. Every other request is the attempt's
//! (`aq_delivery::git_grant::serve`).
use aq_delivery::git_grant::{self, ATTEMPT_HEADER, GitGrant, Refusal, Served};
use axum::extract::Request;
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

/// The request header naming the environment job a `/v1/git` fetch is for.
pub const JOB_HEADER: &str = "aq-environment-job";

/// One `/v1/git` request of the authenticated `node`: under the job grant
/// when it names a job, else under the attempt's git grant.
pub async fn serve(
    pool: &PgPool,
    socket: &Path,
    node: Uuid,
    (repository, path): (Uuid, &str),
    request: Request,
) -> Result<Served, Refusal> {
    let Some(job) = request.headers().get(JOB_HEADER) else {
        return git_grant::serve(pool, socket, node, (repository, path), request).await;
    };
    if request.headers().contains_key(ATTEMPT_HEADER) {
        let why = "name an attempt or an environment job, not both";
        return Err(Refusal::BadRequest(why));
    }
    let job = (job.to_str().ok()).and_then(|v| Uuid::try_parse(v).ok());
    let job = job.ok_or(Refusal::BadRequest(
        "the AQ-Environment-Job header must name the job",
    ))?;
    let query = request.uri().query();
    if git_grant::route(request.method(), path, query)? || query == Some("service=git-receive-pack")
    {
        return Err(Refusal::Forbidden("an environment job may only fetch"));
    }
    let commit: Option<String> = sqlx::query_scalar(
        "SELECT commit FROM environment_jobs WHERE job_id=$1 AND state='claimed' \
         AND claimed_by=$2 AND lease_until>now() AND repository_id=$3",
    )
    .bind(job)
    .bind(node)
    .bind(repository)
    .fetch_optional(pool)
    .await
    .map_err(|_| Refusal::Unavailable)?;
    let base = commit.ok_or(Refusal::Forbidden(
        "no live environment job claim for this request",
    ))?;
    let grant = GitGrant {
        endpoint: git_grant::endpoint(repository),
        repository_id: repository,
        base,
        // aq-git requires a task ref; nothing is ever pushed under this one.
        task_ref: format!("refs/heads/aq/environment-job-{job}"),
        lease: None,
        update_from_main: false,
    };
    git_grant::forward(pool, socket, &grant, (repository, path), request).await
}
