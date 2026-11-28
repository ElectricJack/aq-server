//! Spec 0017 §6.3: a builder fetches its job's
//! commit through the server's `/v1/git/<repository>/…` route with its node
//! credential and `AQ-Environment-Job: <job_id>` instead of `AQ-Attempt`.
//! The job grant is fetch-only, for the job's repository, while the node
//! holds the job's live claim. Every other request is the attempt's
//! (`aq_delivery::git_grant::serve`), which may also force-push the
//! attempt's own try ref (spec 0017 §6.2). [`try_head`] reads that ref for
//! `environment.try`, and [`sweep`] deletes it once the attempt has ended.
use aq_delivery::commands::{GitTarget, SocketGit};
use aq_delivery::git_grant::{self, ATTEMPT_HEADER, GitGrant, Refusal, Served, try_ref};
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
    git_grant::forward(pool, socket, (&grant, None), (repository, path), request).await
}

/// The head of `attempt`'s try ref on aq-git (`socket` is its control
/// socket), or `None` when the attempt never pushed one or it is absent.
pub async fn try_head(
    pool: &PgPool,
    socket: &Path,
    attempt: Uuid,
) -> Result<Option<String>, String> {
    let Some(target) = recorded(pool, attempt).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let reference = try_ref(attempt).unwrap_or_default();
    let head = SocketGit::new(socket).show_ref(&target, &reference).await;
    head.map_err(|e| format!("{}: {}", e.code, e.message))
}

async fn recorded(pool: &PgPool, attempt: Uuid) -> sqlx::Result<Option<GitTarget>> {
    let row: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT repository_id,binding_generation FROM environment_try_refs WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(repository_id, generation)| GitTarget {
        repository_id,
        binding_generation: u64::try_from(generation).unwrap_or_default(),
    }))
}

/// Deletes, on aq-git only, the try ref of every attempt that has ended (no
/// longer holds its task: stopped and settled, abandoned or cancelled). The
/// record stays until Settings `git_transfer_timeout_ms` after its last push,
/// so a push in flight as the attempt ended is deleted too; deleting an
/// absent ref is fine. A transient failure is retried by the next sweep; a
/// permanent one (the binding changed, the repository is gone) drops the
/// record with one warning.
pub async fn sweep(pool: &PgPool, socket: &Path) {
    if let Err(error) = swept(pool, socket).await {
        tracing::warn!(%error, "try ref sweep deferred");
    }
}
/// `tick` and, at the same time, a [`sweep`] on aq-git's control `socket`
/// when the server has one; a slow aq-git delays neither.
pub async fn beside<T>(
    pool: &PgPool,
    socket: Option<&Path>,
    tick: impl std::future::Future<Output = T>,
) -> T {
    let swept = async {
        if let Some(socket) = socket {
            sweep(pool, socket).await;
        }
    };
    tokio::join!(tick, swept).0
}
async fn swept(pool: &PgPool, socket: &Path) -> Result<(), String> {
    let (limits, _) = (aq_role_settings::delivery_limits(pool).await).map_err(|e| e.to_string())?;
    let ended: Vec<(Uuid, Uuid, i64, bool)> = sqlx::query_as(
        "SELECT t.attempt_id,t.repository_id,t.binding_generation,\
         t.pushed_at<now()-make_interval(secs=>$1::double precision/1000) \
         FROM environment_try_refs t LEFT JOIN execution_attempts a USING(attempt_id) \
         WHERE a.attempt_id IS NULL OR NOT a.holds_task",
    )
    .bind(i64::try_from(limits.git_transfer_timeout_ms).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    let git = SocketGit::new(socket);
    for (attempt, repository_id, generation, settled) in ended {
        let target = GitTarget {
            repository_id,
            binding_generation: u64::try_from(generation).unwrap_or_default(),
        };
        let reference = try_ref(attempt).unwrap_or_default();
        let forget = match git.delete_try_ref(&target, &reference).await {
            Ok(_) => settled,
            Err(e) if e.transient => {
                tracing::warn!(%attempt, code = %e.code, "try ref not deleted yet");
                false
            }
            // Retrying cannot help (for example the binding changed): forget it.
            Err(e) => {
                let message = &e.message;
                tracing::warn!(%attempt, code = %e.code, %message, "try ref record dropped undeleted");
                true
            }
        };
        if forget {
            let forget = sqlx::query("DELETE FROM environment_try_refs WHERE attempt_id=$1");
            forget
                .bind(attempt)
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
