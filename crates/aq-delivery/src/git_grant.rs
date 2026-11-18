//! Git grants (spec 0016 §5.2, feature `delivery_playbook`). An offer carries
//! a grant, and the attempt's node fetches from and pushes to central aq-git
//! through the server's `/v1/git/<repository>/…` route. This module builds
//! the grant (the lease is the task ref's head in aq-git's own advertisement),
//! records it with the attempt, mints and checks the attempt's token, and
//! forwards one git smart-HTTP request to aq-git's git socket with the grant
//! headers the server sets. Headers the client sends are never forwarded,
//! except the few that describe the request body. It decides nothing: aq-git
//! enforces what the grant says.
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
#[cfg(test)]
mod tests;

use crate::{DeliveryError, Result};
use aq_admission::{SourceAdmission, SourceSelection, control::NativePollPage, startup::GitGrant};
use aq_role_settings::delivery::DeliveryLimits;
use bytes::Bytes;
use http::{Request, Response, header};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::{path::Path, time::Duration};
use uuid::Uuid;

/// The request header that carries the attempt's git token.
pub const TOKEN_HEADER: &str = "aq-git-token";
/// The routes' prefix on the server's machine listener.
pub const ROUTE_PREFIX: &str = "/v1/git/";
const TOKEN_PREFIX: &str = "aqg_";
/// Client headers that describe the request body. Every other client header,
/// `AQ-*` grant headers and credentials included, stays at the server.
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

pub fn task_ref(task: Uuid) -> String {
    format!("refs/heads/aq/{task}")
}
pub fn endpoint(repository: Uuid) -> String {
    format!("{ROUTE_PREFIX}{repository}")
}
/// The one push operation of an attempt: it names the push receipt, and
/// aq-git accepts at most one push under it.
pub fn push_operation(attempt: Uuid) -> Uuid {
    Uuid::new_v5(&attempt, b"aq.git-push/v1")
}
/// What the server's key authenticates for an attempt's token: the attempt,
/// the node it was offered to and the repository it may reach.
pub fn token_message(attempt: Uuid, node: Uuid, repository: Uuid) -> Vec<u8> {
    [
        b"aq.git-grant.token/v1\0".as_slice(),
        attempt.as_bytes(),
        node.as_bytes(),
        repository.as_bytes(),
    ]
    .concat()
}
/// `aqg_<attempt><mac>`, hex: the token names its attempt, the MAC binds it.
pub fn token(attempt: Uuid, mac: &[u8; 32]) -> String {
    format!("{TOKEN_PREFIX}{}{}", attempt.simple(), hex::encode(mac))
}
/// The attempt a presented token names and its MAC; `None` for any other shape.
pub fn parse_token(presented: &str) -> Option<(Uuid, [u8; 32])> {
    let rest = presented.strip_prefix(TOKEN_PREFIX)?;
    if rest.len() != 96 || !rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let attempt = Uuid::try_parse(&rest[..32]).ok()?;
    let mac = hex::decode(&rest[32..]).ok()?.try_into().ok()?;
    Some((attempt, mac))
}
/// Constant-time equality of two MACs.
pub fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0_u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Why the `/v1/git` route refuses a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No live grant of this token's attempt for this node and repository.
    Forbidden,
    /// The server's key, its database or aq-git is unavailable.
    Unavailable,
}

/// The grant of a new offer whose source follows a named ref, advertised as
/// main; `None` for a fixed commit, which follows no branch.
pub async fn offer_for(
    pool: &PgPool,
    socket: &Path,
    source: &SourceAdmission,
    task: Uuid,
    attempt: Uuid,
) -> Result<Option<GitGrant>> {
    let SourceSelection::NamedRef { reference } = &source.selection else {
        return Ok(None);
    };
    let (repository, base) = (source.repository_id, &source.resolved_object.hex);
    let grant = offer(
        pool,
        socket,
        repository,
        base,
        reference.as_str(),
        task,
        attempt,
    );
    grant.await.map(Some)
}

/// Adds the token to each grant of `page`, delivered to `node`; offers are
/// stored without it. `mac` is the server key's MAC of a [`token_message`].
pub fn deliver(
    page: &mut NativePollPage,
    node: Uuid,
    mac: impl Fn(&[u8]) -> Option<[u8; 32]>,
) -> Option<()> {
    for offer in &mut page.offers {
        let attempt = offer.offered.offer.binding.attempt_id;
        if let Some(grant) = &mut offer.git {
            let message = token_message(attempt, node, grant.repository_id);
            grant.token = Some(token(attempt, &mac(&message)?));
        }
    }
    Some(())
}

/// One `/v1/git/<repository>/…` request of the authenticated `node`. Its
/// token must name an attempt that still holds its task on that node and
/// whose grant is for `repository`; the request is then forwarded under the
/// grant recorded with the offer.
pub async fn serve<B>(
    pool: &PgPool,
    socket: &Path,
    node: Uuid,
    repository: Uuid,
    mac: impl Fn(&[u8]) -> Option<[u8; 32]>,
    request: Request<B>,
) -> std::result::Result<Response<Incoming>, Refusal>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let presented = (request.headers().get(TOKEN_HEADER))
        .and_then(|v| v.to_str().ok())
        .and_then(parse_token);
    let (attempt, presented) = presented.ok_or(Refusal::Forbidden)?;
    let expected = mac(&token_message(attempt, node, repository)).ok_or(Refusal::Unavailable)?;
    if !same(&presented, &expected) {
        return Err(Refusal::Forbidden);
    }
    let grant = live(pool, attempt, node, repository).await;
    let grant = grant
        .map_err(|_| Refusal::Unavailable)?
        .ok_or(Refusal::Forbidden)?;
    let limits = crate::main_sync::limits(pool).await;
    match forward(
        socket,
        &grant,
        &limits.map_err(|_| Refusal::Unavailable)?,
        request,
    )
    .await
    {
        Ok(response) => Ok(response),
        Err(DeliveryError::InvalidInput(_)) => Err(Refusal::Forbidden),
        Err(_) => Err(Refusal::Unavailable),
    }
}

/// The grant of a new attempt's offer: `task` on `repository` from `base`,
/// following `main_ref`. The lease is the task ref's head as aq-git
/// advertises it now, or absent when it advertises no such ref.
pub async fn offer(
    pool: &PgPool,
    socket: &Path,
    repository: Uuid,
    base: &str,
    main_ref: &str,
    task: Uuid,
    attempt: Uuid,
) -> Result<GitGrant> {
    let limits = crate::main_sync::limits(pool).await?;
    let mut grant = GitGrant {
        endpoint: endpoint(repository),
        repository_id: repository,
        base: base.to_owned(),
        task_ref: task_ref(task),
        lease: None,
        main_ref: main_ref.to_owned(),
        operation_id: push_operation(attempt),
        token: None,
    };
    grant
        .validate()
        .map_err(|_| DeliveryError::InvalidInput("invalid git grant"))?;
    grant.lease = observe_lease(socket, &grant, &limits).await?;
    Ok(grant)
}

/// Records `grant` for `attempt` on `node` in the offer's publication.
pub async fn record_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    node: Uuid,
    grant: &GitGrant,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO delivery_git_grants(attempt_id,node_id,repository_id,base,task_ref,lease,\
         main_ref,operation_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(attempt)
    .bind(node)
    .bind(grant.repository_id)
    .bind(&grant.base)
    .bind(&grant.task_ref)
    .bind(&grant.lease)
    .bind(&grant.main_ref)
    .bind(grant.operation_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The recorded grant of `attempt` while the attempt still holds its task on
/// `node`, and only for `repository`. `None` refuses the request.
pub async fn live(
    pool: &PgPool,
    attempt: Uuid,
    node: Uuid,
    repository: Uuid,
) -> Result<Option<GitGrant>> {
    let row = sqlx::query(
        "SELECT g.base,g.task_ref,g.lease,g.main_ref,g.operation_id FROM delivery_git_grants g \
         JOIN execution_attempts a USING(attempt_id) WHERE g.attempt_id=$1 AND g.node_id=$2 \
         AND a.node_id=$2 AND g.repository_id=$3 AND a.holds_task",
    )
    .bind(attempt)
    .bind(node)
    .bind(repository)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| GitGrant {
        endpoint: endpoint(repository),
        repository_id: repository,
        base: row.get("base"),
        task_ref: row.get("task_ref"),
        lease: row.get("lease"),
        main_ref: row.get("main_ref"),
        operation_id: row.get("operation_id"),
        token: None,
    }))
}

/// The `AQ-*` headers aq-git trusts, all from the server.
pub fn grant_headers(grant: &GitGrant, max_push_bytes: u64) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("aq-repository", grant.repository_id.to_string()),
        ("aq-base", grant.base.clone()),
        ("aq-task-ref", grant.task_ref.clone()),
        ("aq-main-ref", grant.main_ref.clone()),
        ("aq-max-push-bytes", max_push_bytes.to_string()),
        ("aq-operation", grant.operation_id.to_string()),
    ];
    if let Some(lease) = &grant.lease {
        headers.push(("aq-task-ref-lease", lease.clone()));
    }
    headers
}

/// Forwards one client request for `/v1/git/<repository>/…` to aq-git's git
/// socket under `grant`, streaming both bodies. The whole transfer, response
/// body included, ends at Settings `git_transfer_timeout_ms`.
pub async fn forward<B>(
    socket: &Path,
    grant: &GitGrant,
    limits: &DeliveryLimits,
    request: Request<B>,
) -> Result<Response<Incoming>>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (parts, body) = request.into_parts();
    let rest = (parts
        .uri
        .path()
        .strip_prefix(&endpoint(grant.repository_id)))
    .filter(|rest| rest.starts_with('/'))
    .ok_or(DeliveryError::InvalidInput(
        "git route outside the granted repository",
    ))?;
    let mut uri = format!("/{}{rest}", grant.repository_id);
    if let Some(query) = parts.uri.query() {
        uri = format!("{uri}?{query}");
    }
    let mut out = Request::builder().method(parts.method).uri(uri);
    for name in FORWARDED {
        for value in parts.headers.get_all(&name) {
            out = out.header(&name, value);
        }
    }
    out = out.header(header::HOST, "aq-git");
    for (name, value) in grant_headers(grant, limits.max_push_bytes) {
        out = out.header(name, value);
    }
    let request = out
        .body(body)
        .map_err(|_| DeliveryError::InvalidInput("invalid git request"))?;
    let limit = Duration::from_millis(limits.git_transfer_timeout_ms);
    let mut response = exchange(socket, request, limit).await?;
    let headers = std::mem::take(response.headers_mut());
    for name in RETURNED {
        for value in headers.get_all(&name) {
            response.headers_mut().append(&name, value.clone());
        }
    }
    Ok(response)
}

/// The task ref's head in aq-git's upload-pack advertisement under `grant`.
pub async fn observe_lease(
    socket: &Path,
    grant: &GitGrant,
    limits: &DeliveryLimits,
) -> Result<Option<String>> {
    use http_body_util::BodyExt;
    let mut request = Request::get(format!(
        "/{}/info/refs?service=git-upload-pack",
        grant.repository_id
    ))
    .header(header::HOST, "aq-git");
    for (name, value) in grant_headers(
        &GitGrant {
            lease: None,
            ..grant.clone()
        },
        1,
    ) {
        request = request.header(name, value);
    }
    let request = request
        .body(http_body_util::Empty::<Bytes>::new())
        .map_err(|_| DeliveryError::InvalidInput("invalid git request"))?;
    let limit = Duration::from_millis(limits.git_transfer_timeout_ms);
    let response = exchange(socket, request, limit).await?;
    if !response.status().is_success() {
        return Err(DeliveryError::Protocol);
    }
    let body = tokio::time::timeout(limit, response.into_body().collect())
        .await
        .map_err(|_| DeliveryError::Transport)?
        .map_err(|_| DeliveryError::Transport)?
        .to_bytes();
    advertised(&body, &grant.task_ref)
}

/// The OID advertised for `reference` in a smart-HTTP ref advertisement.
fn advertised(mut body: &[u8], reference: &str) -> Result<Option<String>> {
    let mut found = None;
    while !body.is_empty() {
        let length = body
            .get(..4)
            .and_then(|l| std::str::from_utf8(l).ok())
            .and_then(|l| usize::from_str_radix(l, 16).ok())
            .ok_or(DeliveryError::Protocol)?;
        if length < 4 {
            body = &body[4..];
            continue;
        }
        let line = body.get(4..length).ok_or(DeliveryError::Protocol)?;
        body = &body[length..];
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.split(|b| *b == 0).next().unwrap_or_default();
        if let Some((oid, name)) = std::str::from_utf8(line)
            .ok()
            .and_then(|l| l.split_once(' '))
            && name == reference
        {
            if oid.len() != 40 || !oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(DeliveryError::Protocol);
            }
            found = Some(oid.to_owned());
        }
    }
    Ok(found)
}

async fn exchange<B>(
    socket: &Path,
    request: Request<B>,
    limit: Duration,
) -> Result<Response<Incoming>>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    let stream = tokio::time::timeout_at(deadline, tokio::net::UnixStream::connect(socket))
        .await
        .map_err(|_| DeliveryError::Transport)??;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| DeliveryError::Transport)?;
    // The connection carries the whole transfer, the response body included,
    // so the deadline ends it at whatever stage the transfer has reached.
    tokio::spawn(async move {
        let _ = tokio::time::timeout_at(deadline, connection).await;
    });
    tokio::time::timeout_at(deadline, sender.send_request(request))
        .await
        .map_err(|_| DeliveryError::Transport)?
        .map_err(|_| DeliveryError::Transport)
}
