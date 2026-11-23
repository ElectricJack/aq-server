//! Git grants (spec 0016 §5.2, feature `delivery_playbook`). An offer carries
//! a grant, and the attempt's node fetches from and pushes to central aq-git
//! through the server's `/v1/git/<repository>/…` route with its own node
//! credential, naming the attempt in the `AQ-Attempt` header. This module
//! builds the grant (the lease is the task ref's head from aq-git's
//! `ref.show`), records it with the attempt, confirms a result's pushed ref
//! (spec 0016 §5.1), and forwards
//! one git smart-HTTP request to aq-git's git socket with the grant headers
//! the server sets. Client headers are never forwarded, except the few that
//! describe the request body. The server itself refuses a push that updates
//! any ref but the grant's task ref, from any old head but the lease, or that
//! is larger than Settings `delivery-limits.max_push_bytes`. It decides nothing
//! about the result. With `environment_try` (spec 0017 §6.2), a push may
//! instead update the attempt's own try ref from any old head; aq-git gets it
//! as `AQ-Try-Ref`, and the push is recorded so the ref is deleted when the
//! attempt ends. [`serve_read`] forwards a supervisor's fetch, which the
//! caller has authorized, under a fixed read grant.
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
#[cfg(test)]
mod tests;

use crate::commands::{
    GitFailure, GitTarget, SocketGit, TASK_BRANCH_PREFIX, object_id, task_branch,
};
use aq_admission::SourceAdmission;
use bytes::{Bytes, BytesMut};
use http::{Method, Request, Response, header};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::{
    path::Path,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::Instant;
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// Client headers that describe the request. Every other client header,
/// `AQ-*` grant headers and the node credential included, stays here.
const FORWARDED: [header::HeaderName; 6] = [
    header::ACCEPT,
    header::ACCEPT_ENCODING,
    header::CONTENT_ENCODING,
    header::CONTENT_LENGTH,
    header::CONTENT_TYPE,
    header::USER_AGENT,
];
/// Response headers passed back to the client; framing is the server's own.
const RETURNED: [header::HeaderName; 5] = [
    header::CACHE_CONTROL,
    header::CONTENT_ENCODING,
    header::CONTENT_LENGTH,
    header::CONTENT_TYPE,
    header::EXPIRES,
];

/// The git grant of one attempt, served to its node by
/// `GET /v1/git/grants/<attempt>` (the frozen offer types do not carry it).
/// The node fetches and pushes at `endpoint` on the worker origin with its
/// own node credential and `AQ-Attempt: <attempt>`. A present `lease` means
/// the task branch exists: the attempt starts from it (the task was returned
/// or handed off), else from `base`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitGrant {
    /// `/v1/git/<repository_id>`; git appends `/info/refs` etc.
    pub endpoint: String,
    pub repository_id: Uuid,
    /// The commit the task's first attempt starts from (40 hex digits).
    pub base: String,
    /// `refs/heads/aq/<task-id>`, the only ref the attempt may push.
    pub task_ref: String,
    /// The task ref's head in central aq-git when the offer was made; absent
    /// when the branch did not exist. The push must name it as its old head.
    pub lease: Option<String>,
    /// The task was returned with `update_from_main`: merge the current
    /// `refs/heads/main` into the branch before the agent starts.
    pub update_from_main: bool,
}

/// The request header naming the attempt a `/v1/git` request is for.
pub const ATTEMPT_HEADER: &str = "aq-attempt";
pub use crate::commands::TRY_PREFIX;

/// The attempt's own try ref, the one ref beside the task ref its push may
/// update; `None` without `environment_try`.
pub fn try_ref(attempt: Uuid) -> Option<String> {
    cfg!(feature = "environment_try").then(|| format!("{TRY_PREFIX}{attempt}"))
}

/// The grant's endpoint on the worker API; git appends `/info/refs` etc.
pub fn endpoint(repository: Uuid) -> String {
    format!("/v1/git/{repository}")
}

/// Why the `/v1/git` route refuses a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not a git smart-HTTP route.
    NotFound,
    /// No live grant of this attempt for this node and repository, or a push
    /// of another ref or a deletion.
    Forbidden(&'static str),
    /// A push whose old head is not the grant's lease.
    StaleLease,
    /// A push larger than Settings `max_push_bytes`.
    TooLarge,
    BadRequest(&'static str),
    /// The database, Settings or aq-git is unavailable, or the transfer ran
    /// past Settings `git_transfer_timeout_ms`.
    Unavailable,
}

/// The grant of a new attempt's offer: the task ref of `task` on the
/// source's repository from the source's commit. The lease is the task ref's
/// head in central aq-git now, or absent when it has none. `update_from_main`
/// follows the return decision. An authorized retry after the return limit
/// also refreshes a retained head with a recorded PR conflict on that result.
pub async fn offer(
    pool: &PgPool,
    git: &SocketGit,
    source: &SourceAdmission,
    task: Uuid,
) -> Result<GitGrant, GitFailure> {
    let repository = source.repository_id;
    let target = GitTarget {
        repository_id: repository,
        binding_generation: source.repository_binding_generation,
    };
    let task_ref = task_branch(task);
    let lease = git.show_ref(&target, &task_ref).await?;
    let update_from_main: Option<bool> = sqlx::query_scalar(
        "SELECT (decision='returned' AND COALESCE(update_from_main,false)) OR
                (decision='held' AND COALESCE(reason='returns_exhausted',false) AND EXISTS(
                  SELECT 1 FROM playbook_host_operations o
                  WHERE o.subject_id=d.task_id::text AND o.command_name IN ('pull_request.await_checks','pull_request.merge')
                    AND o.status='conflict' AND o.request#>>'{result_ready,result_id}'=d.result_id::text
                    AND COALESCE(o.request->>'result_head',o.request->>'expected_head_sha')=$2
                    AND COALESCE(o.request->'check_target',o.request->'merge_target')->'git'
                      @> $3::jsonb))
         FROM delivery_decisions d WHERE task_id=$1 ORDER BY decided_at_ms DESC,result_id LIMIT 1",
    )
    .bind(task)
    .bind(lease.as_deref())
    .bind(serde_json::json!({"repository_id":repository,"binding_generation":source.repository_binding_generation}))
    .fetch_optional(pool)
    .await
    .map_err(|_| GitFailure::transport())?;
    Ok(GitGrant {
        endpoint: endpoint(repository),
        repository_id: repository,
        base: source.resolved_object.hex.clone(),
        task_ref,
        lease,
        update_from_main: update_from_main.unwrap_or(false),
    })
}

/// Records `grant` for `attempt` in the offer's publication.
pub async fn record_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    grant: &GitGrant,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO delivery_git_grants(attempt_id,repository_id,base,task_ref,lease,\
         update_from_main) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(attempt)
    .bind(grant.repository_id)
    .bind(&grant.base)
    .bind(&grant.task_ref)
    .bind(&grant.lease)
    .bind(grant.update_from_main)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Current report of a stopped attempt whose delivery decision retained it for supervisor handling.
pub async fn held_report<'e>(
    db: impl sqlx::PgExecutor<'e>,
    task: Uuid,
) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar(
        "SELECT h.report_id FROM execution_attempts a JOIN profiled_result_heads h USING(attempt_id)
         JOIN delivery_git_grants g USING(attempt_id) JOIN delivery_decisions d ON d.result_id=h.report_id
         JOIN profiled_result_reports r ON r.report_id=h.report_id
         WHERE a.task_id=$1 AND a.holds_task AND d.task_id=$1 AND d.decision='held'
         AND CASE convert_from(r.report_bytes,'UTF8')::jsonb->>'outcome'
           WHEN 'failed' THEN d.reason IN ('policy','returns_exhausted')
           WHEN 'success' THEN d.reason='returns_exhausted' AND EXISTS(
             SELECT 1 FROM playbook_host_operations o WHERE o.subject_id=a.task_id::text
               AND o.request#>>'{result_ready,result_id}'=r.report_id::text
               AND o.request#>>'{result_ready,attempt_id}'=a.attempt_id::text
               AND ((o.command_name='pull_request.await_checks' AND o.status IN ('failed','conflict'))
                 OR (o.command_name='pull_request.merge' AND o.status='conflict')))
             AND NOT EXISTS(SELECT 1 FROM playbook_host_operations o WHERE o.subject_id=a.task_id::text
               AND o.request#>>'{result_ready,result_id}'=r.report_id::text
               AND o.command_name='pull_request.merge' AND o.status NOT IN ('conflict','refused','not_mergeable'))
           ELSE false END",
    ).bind(task).fetch_optional(db).await
}

/// The current failed result an owner can reconcile after a proven cancellation.
/// A stopped attempt with no result needs a separate source-custody proof.
pub async fn cancelled_failed_report<'e>(
    db: impl sqlx::PgExecutor<'e>,
    task: Uuid,
) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar(
        "SELECT h.report_id FROM execution_attempts a
         JOIN profiled_result_heads h USING(attempt_id)
         JOIN profiled_cancelled_settlements c USING(attempt_id)
         JOIN profiled_authority_hot hot USING(attempt_id)
         JOIN delivery_git_grants g USING(attempt_id)
         JOIN profiled_result_reports r ON r.report_id=h.report_id
         JOIN delivery_pushed_results p ON p.report_id=h.report_id AND p.attempt_id=a.attempt_id
         WHERE a.task_id=$1 AND a.holds_task AND p.head IS NOT NULL AND p.task_ref=g.task_ref
         AND hot.state#>>'{stop,kind}'='stopped'
         AND hot.state#>>'{stop,receipt,digest}'=c.stop_receipt_digest
         AND hot.state->>'cancellation_id'=c.cancellation_id::text
         AND convert_from(r.report_bytes,'UTF8')::jsonb->>'outcome'='failed'
         AND NOT EXISTS(SELECT 1 FROM delivery_decisions d WHERE d.result_id=h.report_id
                        AND d.decision='completed')
         AND NOT EXISTS(SELECT 1 FROM profiled_delivery_evidence d WHERE d.attempt_id=a.attempt_id
                        AND d.state IN ('indeterminate','published'))
         AND NOT EXISTS(SELECT 1 FROM delivery_merge_evidence m WHERE m.attempt_id=a.attempt_id)
         AND NOT EXISTS(SELECT 1 FROM playbook_host_operations o
                        WHERE o.subject_id=a.task_id::text
                        AND o.request#>>'{result_ready,attempt_id}'=a.attempt_id::text
                        AND o.command_name='pull_request.merge'
                        AND o.status NOT IN ('conflict','refused','not_mergeable'))",
    )
    .bind(task)
    .fetch_optional(db)
    .await
}

/// The frozen delivery selector, independent of the attempt's lifecycle.
pub async fn exists<'e>(db: impl sqlx::PgExecutor<'e>, attempt: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM delivery_git_grants WHERE attempt_id=$1)")
        .bind(attempt)
        .fetch_one(db)
        .await
}

pub enum Lookup {
    Granted(GitGrant),
    Absent,
    WrongNode,
}

/// The stored grant and its node ownership, including stopped and settled attempts.
pub async fn lookup(pool: &PgPool, attempt: Uuid, node: Uuid) -> sqlx::Result<Lookup> {
    let row = sqlx::query(
        "SELECT a.node_id,g.repository_id,g.base,g.task_ref,g.lease,g.update_from_main \
         FROM execution_attempts a LEFT JOIN delivery_git_grants g USING(attempt_id) \
         WHERE a.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(Lookup::Absent);
    };
    if row.get::<Uuid, _>("node_id") != node {
        return Ok(Lookup::WrongNode);
    }
    Ok(match row.get::<Option<Uuid>, _>("repository_id") {
        Some(repository) => Lookup::Granted(decode_grant(&row, repository)),
        None => Lookup::Absent,
    })
}

/// Smart HTTP authority expires when the attempt releases its hold or
/// records a result. Discovery via `lookup` is independent of this fence.
pub async fn live(pool: &PgPool, attempt: Uuid, node: Uuid) -> sqlx::Result<Option<GitGrant>> {
    let row = sqlx::query(
        "SELECT g.repository_id,g.base,g.task_ref,g.lease,g.update_from_main \
         FROM delivery_git_grants g JOIN execution_attempts a USING(attempt_id) \
         WHERE g.attempt_id=$1 AND a.node_id=$2 AND a.holds_task \
         AND NOT EXISTS(SELECT 1 FROM profiled_result_reports r WHERE r.attempt_id=g.attempt_id)",
    )
    .bind(attempt)
    .bind(node)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| decode_grant(&row, row.get("repository_id"))))
}

fn decode_grant(row: &sqlx::postgres::PgRow, repository_id: Uuid) -> GitGrant {
    GitGrant {
        endpoint: endpoint(repository_id),
        repository_id,
        base: row.get("base"),
        task_ref: row.get("task_ref"),
        lease: row.get("lease"),
        update_from_main: row.get("update_from_main"),
    }
}

/// What a result request says of the task branch (spec 0016 §5.2), beside
/// the frozen report: it pushed the ref at a head (`AQ-Pushed-Ref`,
/// `AQ-Pushed-Head`), or its lease no longer held (`AQ-Push-Conflict: true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pushed {
    At(PushedRef),
    Conflict,
}

/// The task branch a result report names and the commit pushed to it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PushedRef {
    #[serde(rename = "ref")]
    pub reference: String,
    pub head: String,
}

impl Pushed {
    /// The headers of a result request: both `AQ-Pushed-*`, or
    /// `AQ-Push-Conflict: true`, or none of them.
    pub fn from_headers(headers: &http::HeaderMap) -> Result<Option<Self>, PushedRefusal> {
        let get = |name| headers.get(name).map(|v| v.to_str().map(str::to_owned));
        let conflict = get("aq-push-conflict");
        match (get("aq-pushed-ref"), get("aq-pushed-head"), conflict) {
            (None, None, None) => Ok(None),
            (Some(Ok(reference)), Some(Ok(head)), None) => {
                Ok(Some(Self::At(PushedRef { reference, head })))
            }
            (None, None, Some(Ok(flag))) if flag == "true" => Ok(Some(Self::Conflict)),
            _ => Err(PushedRefusal::BadRequest(
                "send AQ-Pushed-Ref with AQ-Pushed-Head, or AQ-Push-Conflict: true, or none",
            )),
        }
    }
}

/// Why a result report's pushed ref is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushedRefusal {
    BadRequest(&'static str),
    Refused(&'static str),
    /// The database or aq-git is unavailable; the same report may be retried.
    Unavailable,
}

/// Confirms and records what a new result report of `node`'s `attempt` says
/// of its task branch. A pushed ref must be the attempt's granted task ref,
/// and central aq-git's `ref.show` must have it at the reported head: the ref
/// is the record of the result, and nothing else is retained. A push conflict
/// must be a fact: the task ref is no longer at the grant's lease. A report
/// already recorded is a replay, checked byte for byte by the result path,
/// and is not asked again.
pub async fn confirm_pushed(
    pool: &PgPool,
    git: &SocketGit,
    (node, attempt, report): (Uuid, Uuid, Uuid),
    pushed: &Pushed,
) -> Result<(), PushedRefusal> {
    let pushed_at = match pushed {
        Pushed::At(at) => Some(at),
        Pushed::Conflict => None,
    };
    if pushed_at
        .is_some_and(|p| !p.reference.starts_with(TASK_BRANCH_PREFIX) || !object_id(&p.head))
    {
        return Err(PushedRefusal::BadRequest(
            "AQ-Pushed-Ref must be a task branch at a commit",
        ));
    }
    let row = sqlx::query(
        "SELECT g.repository_id,g.task_ref,g.lease,\
         (l.execution_spec#>>'{source,repository_binding_generation}')::bigint AS generation,\
         EXISTS(SELECT 1 FROM profiled_result_reports r WHERE r.report_id=$3) AS recorded \
         FROM delivery_git_grants g JOIN execution_attempts a USING(attempt_id) \
         JOIN profiled_launches l USING(attempt_id) WHERE g.attempt_id=$1 AND a.node_id=$2",
    )
    .bind(attempt)
    .bind(node)
    .bind(report)
    .fetch_optional(pool)
    .await
    .map_err(|_| PushedRefusal::Unavailable)?;
    let row = row.ok_or(PushedRefusal::Refused("the attempt has no git grant"))?;
    if row.get::<bool, _>("recorded") {
        return Ok(());
    }
    let task_ref: String = row.get("task_ref");
    if pushed_at.is_some_and(|p| p.reference != task_ref) {
        return Err(PushedRefusal::Refused(
            "the pushed ref is not the attempt's task ref",
        ));
    }
    let generation = row
        .get::<Option<i64>, _>("generation")
        .and_then(|g| u64::try_from(g).ok());
    let target = GitTarget {
        repository_id: row.get("repository_id"),
        binding_generation: generation.ok_or(PushedRefusal::Unavailable)?,
    };
    let head = git.show_ref(&target, &task_ref).await;
    let head = head.map_err(|_| PushedRefusal::Unavailable)?;
    match pushed_at {
        Some(p) if head.as_deref() != Some(p.head.as_str()) => {
            return Err(PushedRefusal::Refused(
                "the task ref is not at the reported head",
            ));
        }
        None if head == row.get::<Option<String>, _>("lease") => {
            return Err(PushedRefusal::Refused("the task ref is still at the lease"));
        }
        _ => {}
    }
    sqlx::query(
        "INSERT INTO delivery_pushed_results(report_id,attempt_id,task_ref,head) \
         VALUES($1,$2,$3,$4) ON CONFLICT(report_id) DO NOTHING",
    )
    .bind(report)
    .bind(attempt)
    .bind(&task_ref)
    .bind(pushed_at.map(|p| &p.head))
    .execute(pool)
    .await
    .map_err(|_| PushedRefusal::Unavailable)?;
    Ok(())
}

/// [`confirm_pushed`] of what a result request's headers name, if anything.
pub async fn confirm_headers(
    pool: &PgPool,
    git: &SocketGit,
    ids: (Uuid, Uuid, Uuid),
    headers: &http::HeaderMap,
) -> Result<(), PushedRefusal> {
    match Pushed::from_headers(headers)? {
        Some(pushed) => confirm_pushed(pool, git, ids, &pushed).await,
        None => Ok(()),
    }
}

/// The confirmed pushed ref of a recorded report, if it named one (a push
/// conflict names no head).
pub async fn pushed_of(
    db: impl sqlx::PgExecutor<'_>,
    report: Uuid,
) -> sqlx::Result<Option<PushedRef>> {
    let row = sqlx::query(
        "SELECT task_ref,head FROM delivery_pushed_results WHERE report_id=$1 AND head IS NOT NULL",
    )
    .bind(report)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| PushedRef {
        reference: row.get("task_ref"),
        head: row.get("head"),
    }))
}

/// aq-git's answer to one forwarded git request.
pub type Served = Response<Incoming>;

/// One `/v1/git/<repository>/<path>` request of the authenticated `node` for
/// the attempt its `AQ-Attempt` header names, forwarded to aq-git's git
/// socket under the attempt's recorded grant.
pub async fn serve<B>(
    pool: &PgPool,
    socket: &Path,
    node: Uuid,
    (repository, path): (Uuid, &str),
    request: Request<B>,
) -> Result<Served, Refusal>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    route(request.method(), path, request.uri().query())?;
    let attempt = (request.headers().get(ATTEMPT_HEADER))
        .and_then(|v| Uuid::try_parse(v.to_str().ok()?).ok())
        .ok_or(Refusal::BadRequest(
            "the AQ-Attempt header must name the attempt",
        ))?;
    let grant = live(pool, attempt, node)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let grant = (grant.filter(|g| g.repository_id == repository))
        .ok_or(Refusal::Forbidden("no live git grant for this request"))?;
    forward(
        pool,
        socket,
        (&grant, Some(attempt)),
        (repository, path),
        request,
    )
    .await
}

/// Forwards one `/v1/git/<repository>/<path>` request to aq-git's git socket
/// under `grant`, which the caller found live for the node and repository.
/// A push may update only the grant's task ref from its lease, or, for an
/// `attempt` (not a builder's job), that attempt's [`try_ref`] from any head.
pub async fn forward<B>(
    pool: &PgPool,
    socket: &Path,
    (grant, attempt): (&GitGrant, Option<Uuid>),
    (repository, path): (Uuid, &str),
    request: Request<B>,
) -> Result<Served, Refusal>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    let push = route(request.method(), path, request.uri().query())?;
    let try_ref = attempt.and_then(try_ref);
    let (limits, _) = aq_role_settings::delivery_limits(pool)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let deadline = Instant::now() + Duration::from_millis(limits.git_transfer_timeout_ms);
    let (parts, body) = request.into_parts();
    let declared = (parts.headers.get(header::CONTENT_LENGTH))
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limits.max_push_bytes) {
        return Err(Refusal::TooLarge);
    }
    let mut body = Counted {
        prefix: None,
        inner: body,
        left: limits.max_push_bytes,
    };
    if push {
        if parts.headers.contains_key(header::CONTENT_ENCODING) {
            return Err(Refusal::BadRequest(
                "an encoded push body cannot be checked",
            ));
        }
        let commands = commands(&mut body, grant, try_ref.as_deref());
        let (read, tried) = (tokio::time::timeout_at(deadline, commands).await)
            .map_err(|_| Refusal::Unavailable)??;
        body.prefix = Some(read);
        if let Some(attempt) = attempt.filter(|_| tried) {
            record_try(pool, attempt, repository).await?;
        }
    }
    let grant = (
        grant.base.as_str(),
        grant.task_ref.as_str(),
        limits.max_push_bytes,
    );
    let to = (socket, repository, path);
    to_aq_git(to, parts, body, (grant, try_ref.as_deref()), deadline).await
}

/// The grant a supervisor's fetch is forwarded under: aq-git checks only its
/// syntax, and it constrains nothing but a push, which is never forwarded.
pub const READ_BASE: &str = ZERO_OID;
pub const READ_TASK_REF: &str = "refs/heads/aq/supervisor-read";
const READ_GRANT: (&str, &str, u64) = (READ_BASE, READ_TASK_REF, 1);

/// One `/v1/supervisor/git/<repository>/<path>` fetch, which the caller has
/// authorized for `repository`, forwarded to aq-git's git socket under
/// [`READ_GRANT`] within Settings `git_transfer_timeout_ms`. Anything but the
/// two fetch routes is refused; the body passes through unread and unlimited.
pub async fn serve_read<B>(
    pool: &PgPool,
    socket: &Path,
    repository: Uuid,
    path: &str,
    request: Request<B>,
) -> Result<Response<Incoming>, Refusal>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    let fetch = matches!(
        (request.method().as_str(), path, request.uri().query()),
        ("GET", "info/refs", Some("service=git-upload-pack")) | ("POST", "git-upload-pack", None)
    );
    if !fetch {
        return Err(Refusal::Forbidden("Supervisors can only fetch"));
    }
    let (limits, _) = aq_role_settings::delivery_limits(pool)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let deadline = Instant::now() + Duration::from_millis(limits.git_transfer_timeout_ms);
    let (parts, body) = request.into_parts();
    to_aq_git(
        (socket, repository, path),
        parts,
        body,
        (READ_GRANT, None),
        deadline,
    )
    .await
}

/// Sends the request to aq-git at `/<repository>/<path>` with only the
/// [`FORWARDED`] client headers and the grant headers of `(base, task_ref,
/// max_push_bytes)` (and `AQ-Try-Ref`, if any), and returns aq-git's
/// response with only [`RETURNED`].
async fn to_aq_git<B>(
    (socket, repository, path): (&Path, Uuid, &str),
    parts: http::request::Parts,
    body: B,
    ((base, task_ref, max_push_bytes), try_ref): ((&str, &str, u64), Option<&str>),
    deadline: Instant,
) -> Result<Response<Incoming>, Refusal>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    let mut uri = format!("/{repository}/{path}");
    if let Some(query) = parts.uri.query() {
        uri = format!("{uri}?{query}");
    }
    let mut out = Request::builder().method(parts.method).uri(uri);
    for name in FORWARDED {
        for value in parts.headers.get_all(&name) {
            out = out.header(&name, value);
        }
    }
    out = (out.header(header::HOST, "aq-git"))
        .header("aq-repository", repository.to_string())
        .header("aq-base", base)
        .header("aq-task-ref", task_ref)
        .header("aq-max-push-bytes", max_push_bytes.to_string())
        .header("aq-operation", Uuid::new_v4().to_string());
    if let Some(try_ref) = try_ref {
        out = out.header("aq-try-ref", try_ref);
    }
    let request = out
        .body(body)
        .map_err(|_| Refusal::BadRequest("invalid git request"))?;
    let mut response = exchange(socket, request, deadline).await?;
    let headers = std::mem::take(response.headers_mut());
    for name in RETURNED {
        for value in headers.get_all(&name) {
            response.headers_mut().append(&name, value.clone());
        }
    }
    Ok(response)
}

/// Whether `method path?query` is a smart-HTTP route, and a push.
pub fn route(method: &Method, path: &str, query: Option<&str>) -> Result<bool, Refusal> {
    match (method.as_str(), path, query) {
        ("GET", "info/refs", Some("service=git-upload-pack" | "service=git-receive-pack"))
        | ("POST", "git-upload-pack", None) => Ok(false),
        ("POST", "git-receive-pack", None) => Ok(true),
        _ => Err(Refusal::NotFound),
    }
}

/// Records, before it reaches aq-git, that `attempt` pushes its try ref, so
/// the sweep deletes the ref once the attempt ends. Only while the attempt
/// holds its task: a push that passed the grant check as the attempt ended
/// records nothing and is refused.
async fn record_try(pool: &PgPool, attempt: Uuid, repository: Uuid) -> Result<(), Refusal> {
    let recorded = sqlx::query(
        "INSERT INTO environment_try_refs(attempt_id,repository_id,binding_generation) \
         SELECT a.attempt_id,$2,(l.execution_spec#>>'{source,repository_binding_generation}')::bigint \
         FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) \
         WHERE a.attempt_id=$1 AND a.holds_task \
         ON CONFLICT(attempt_id) DO UPDATE SET pushed_at=now()",
    )
    .bind(attempt)
    .bind(repository)
    .execute(pool)
    .await
    .map_err(|_| Refusal::Unavailable)?;
    match recorded.rows_affected() {
        0 => Err(Refusal::Forbidden("no live git grant for this request")),
        _ => Ok(()),
    }
}

/// Reads a receive-pack request's command list (pkt-lines up to the first
/// flush) and checks it against `grant`: no command (git's probe), or one
/// update of the task ref from the lease, or of `try_ref` from any head, to a
/// commit. Returns the bytes read and whether the update is of `try_ref`.
async fn commands<B>(
    body: &mut B,
    grant: &GitGrant,
    try_ref: Option<&str>,
) -> Result<(Bytes, bool), Refusal>
where
    B: Body<Data = Bytes> + Unpin,
{
    let malformed = Refusal::BadRequest("malformed receive-pack commands");
    let (mut read, mut at, mut seen, mut tried) = (BytesMut::new(), 0, 0, false);
    loop {
        while let Some(length) = read.get(at..at + 4) {
            let length = std::str::from_utf8(length).ok();
            let length = length.and_then(|l| usize::from_str_radix(l, 16).ok());
            match length.ok_or(malformed)? {
                0 => return Ok((read.freeze(), tried)),
                1..=4 => return Err(malformed),
                length if read.len() < at + length => break,
                length => {
                    seen += 1;
                    if seen > 1 {
                        return Err(Refusal::Forbidden("a push may update only the task ref"));
                    }
                    tried = check_command(&read[at + 4..at + length], grant, try_ref)?;
                    at += length;
                }
            }
        }
        match body.frame().await {
            Some(Ok(frame)) => read.extend_from_slice(&frame.into_data().unwrap_or_default()),
            Some(Err(_)) => return Err(Refusal::TooLarge),
            None => return Err(malformed),
        }
    }
}

/// `<old> <new> <ref>[\0<capabilities>]\n`; whether it updates `try_ref`.
fn check_command(line: &[u8], grant: &GitGrant, try_ref: Option<&str>) -> Result<bool, Refusal> {
    let malformed = Refusal::BadRequest("malformed receive-pack command");
    let line = line.split(|b| *b == 0).next().unwrap_or_default();
    let line = std::str::from_utf8(line).map_err(|_| malformed)?;
    let fields: Vec<&str> = line.trim_end_matches('\n').split(' ').collect();
    let [old, new, reference] = fields[..] else {
        return Err(malformed);
    };
    if !object_id(old) || !object_id(new) {
        return Err(malformed);
    }
    let tried = try_ref == Some(reference);
    if reference != grant.task_ref && !tried {
        return Err(Refusal::Forbidden("a push may update only the task ref"));
    }
    if new == ZERO_OID && tried {
        return Err(Refusal::Forbidden("deleting the try ref is not allowed"));
    }
    if new == ZERO_OID {
        return Err(Refusal::Forbidden("deleting the task ref is not allowed"));
    }
    // A try ref moves from any head: tries are siblings, not a chain.
    if !tried && old != grant.lease.as_deref().unwrap_or(ZERO_OID) {
        return Err(Refusal::StaleLease);
    }
    Ok(tried)
}

/// The request body: the bytes [`commands`] already read, then the rest,
/// failing once more than `left` bytes came in total.
struct Counted<B> {
    prefix: Option<Bytes>,
    inner: B,
    left: u64,
}
impl<B> Body for Counted<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let Some(prefix) = this.prefix.take() {
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
        {
            let Some(left) = this.left.checked_sub(data.len() as u64) else {
                return Poll::Ready(Some(Err("git request body exceeds max_push_bytes".into())));
            };
            this.left = left;
        }
        polled.map_err(Into::into)
    }
    fn is_end_stream(&self) -> bool {
        self.prefix.is_none() && self.inner.is_end_stream()
    }
}

async fn exchange<B>(
    socket: &Path,
    request: Request<B>,
    deadline: Instant,
) -> Result<Response<Incoming>, Refusal>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    let stream = tokio::time::timeout_at(deadline, tokio::net::UnixStream::connect(socket)).await;
    let stream = stream
        .map_err(|_| Refusal::Unavailable)?
        .map_err(|_| Refusal::Unavailable)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| Refusal::Unavailable)?;
    // The connection carries the whole transfer, the response body included,
    // so the deadline ends it at whatever stage the transfer has reached.
    tokio::spawn(async move {
        let _ = tokio::time::timeout_at(deadline, connection).await;
    });
    tokio::time::timeout_at(deadline, sender.send_request(request))
        .await
        .map_err(|_| Refusal::Unavailable)?
        .map_err(|_| Refusal::Unavailable)
}

/// Fence a selected result before recovering it for a resumed owner authorization.
pub async fn is_retry_report(
    pool: &sqlx::PgPool,
    report: Uuid,
    attempt: Option<Uuid>,
) -> sqlx::Result<bool> {
    let Some(attempt) = attempt else {
        return Ok(true);
    };
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_result_reports WHERE report_id=$1 AND attempt_id=$2)",
    )
    .bind(report)
    .bind(attempt)
    .fetch_one(pool)
    .await
}
