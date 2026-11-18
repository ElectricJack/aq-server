use crate::{Loaded, Mac, StoredGrant, grant_headers, verify};
use axum::{
    Router,
    body::Body,
    extract::{Path as RoutePath, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::Engine;
use http_body_util::Limited;
use sqlx::Row;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use uuid::Uuid;

/// The stored grant of an attempt whose grant is still live.
pub trait GrantSource: Send + Sync + 'static {
    fn live_grant(
        &self,
        attempt_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredGrant>, String>> + Send + '_>>;
}
/// Live grants from the server's attempt records.
pub struct PgGrants {
    pub pool: sqlx::PgPool,
    /// Server time in milliseconds.
    pub now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
}
impl GrantSource for PgGrants {
    fn live_grant(
        &self,
        attempt_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredGrant>, String>> + Send + '_>> {
        Box::pin(async move {
            let row = sqlx::query("SELECT l.graph_task_observation->'git' AS git,l.offer_expires_at_ms,h.state AS hot FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1 AND a.kind='profiled_v1'")
                .bind(attempt_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
            let Some(row) = row else { return Ok(None) };
            let git: Option<serde_json::Value> = row.get("git");
            let Ok(grant) = serde_json::from_value::<StoredGrant>(git.unwrap_or_default()) else {
                return Ok(None);
            };
            let hot: serde_json::Value = row.get("hot");
            // The token expires with the attempt's grant; before one, with the offer.
            let until = hot["execution_deadline_ms"]
                .as_i64()
                .unwrap_or(row.get::<i64, _>("offer_expires_at_ms"));
            let live = hot["revoked"] == false
                && matches!(
                    hot["status"].as_str(),
                    Some("offered" | "preparing" | "granted" | "finished")
                )
                && (self.now_ms)() < until;
            Ok(live.then_some(grant))
        })
    }
}
pub struct Proxy {
    pub loaded: Arc<Loaded>,
    pub mac: Mac,
    pub grants: Arc<dyn GrantSource>,
}
/// `/v1/git/{repository}/{*path}` on the worker API.
pub fn routes<S: Clone + Send + Sync + 'static>(proxy: Proxy) -> Router<S> {
    Router::new()
        .route("/v1/git/{repository}/{*path}", any(handle))
        .with_state(Arc::new(proxy))
}

fn reply(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, message).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"aq-git\""),
        );
    }
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}
/// The bearer token, or the password of HTTP Basic (git credential helpers).
fn presented(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(token) = value.strip_prefix("Bearer ") {
        return Some(token.trim().to_owned());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.strip_prefix("Basic ")?.trim())
        .ok()?;
    Some(
        String::from_utf8(decoded)
            .ok()?
            .split_once(':')?
            .1
            .to_owned(),
    )
}
/// Only git's smart-HTTP service routes.
fn service(method: &Method, path: &str, query: Option<&str>) -> bool {
    match (method, path) {
        (&Method::GET, "info/refs") => matches!(
            query,
            Some("service=git-upload-pack" | "service=git-receive-pack")
        ),
        (&Method::POST, "git-upload-pack" | "git-receive-pack") => true,
        _ => false,
    }
}
/// Client headers git's protocol needs. Everything else, including any
/// `AQ-*` header and the credential, stops here.
const FORWARDED: [HeaderName; 4] = [
    header::CONTENT_TYPE,
    header::CONTENT_ENCODING,
    header::ACCEPT,
    HeaderName::from_static("git-protocol"),
];
const RETURNED: [HeaderName; 2] = [header::CONTENT_TYPE, header::CONTENT_ENCODING];

pub(crate) fn set_grant(
    headers: &mut HeaderMap,
    grant: &StoredGrant,
    max_push_bytes: u64,
) -> Result<(), axum::http::header::InvalidHeaderValue> {
    for (name, value) in grant_headers(grant, max_push_bytes) {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(())
}
/// One HTTP/1.1 exchange on aq-git's git socket. The connection, and with it
/// the response body, ends at the transfer deadline.
pub(crate) async fn send(
    loaded: &Loaded,
    request: axum::http::Request<Body>,
) -> anyhow::Result<axum::http::Response<hyper::body::Incoming>> {
    let deadline = Duration::from_millis(loaded.config.transfer_timeout_ms);
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(&loaded.config.socket).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Ok(Err(error)) = tokio::time::timeout(deadline, connection).await {
                tracing::debug!(%error, "aq-git git connection ended");
            }
        });
        Ok::<_, anyhow::Error>(sender.send_request(request).await?)
    };
    tokio::time::timeout(deadline, exchange)
        .await
        .map_err(|_| anyhow::anyhow!("aq-git git transfer deadline"))?
}

async fn handle(
    State(proxy): State<Arc<Proxy>>,
    RoutePath((repository, path)): RoutePath<(String, String)>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let query = parts.uri.query();
    let Ok(repository_id) = Uuid::try_parse(&repository) else {
        return reply(StatusCode::NOT_FOUND, "no such repository\n");
    };
    if !service(&parts.method, &path, query) {
        return reply(StatusCode::NOT_FOUND, "not a git smart-HTTP route\n");
    }
    let Some(attempt_id) =
        presented(&parts.headers).and_then(|t| verify(&proxy.mac, &t, repository_id))
    else {
        return reply(
            StatusCode::UNAUTHORIZED,
            "a git grant token for this repository is required\n",
        );
    };
    let Ok(_slot) = proxy.loaded.slots.try_acquire() else {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            "git transfer capacity exhausted\n",
        );
    };
    let grant = match proxy.grants.live_grant(attempt_id).await {
        Ok(Some(grant)) if grant.repository_id == repository_id => grant,
        Ok(_) => {
            return reply(
                StatusCode::FORBIDDEN,
                "the git grant is not live for this repository\n",
            );
        }
        Err(error) => {
            tracing::warn!(%error, %attempt_id, "git grant lookup failed");
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                "git grant lookup unavailable\n",
            );
        }
    };
    let uri = match query {
        Some(q) => format!("/{repository_id}/{path}?{q}"),
        None => format!("/{repository_id}/{path}"),
    };
    let max = proxy.loaded.config.max_push_bytes;
    let limited = Body::new(Limited::new(
        body,
        usize::try_from(max).unwrap_or(usize::MAX),
    ));
    let Ok(mut upstream) = axum::http::Request::builder()
        .method(parts.method.clone())
        .uri(uri)
        .header(header::HOST, "aq-git")
        .body(limited)
    else {
        return reply(StatusCode::BAD_REQUEST, "invalid git request\n");
    };
    for name in FORWARDED {
        if let Some(value) = parts.headers.get(&name) {
            upstream.headers_mut().insert(name, value.clone());
        }
    }
    if set_grant(upstream.headers_mut(), &grant, max).is_err() {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid stored git grant\n",
        );
    }
    let response = match send(&proxy.loaded, upstream).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, %attempt_id, "aq-git git transfer failed");
            return reply(StatusCode::BAD_GATEWAY, "aq-git git transfer failed\n");
        }
    };
    let (head, incoming) = response.into_parts();
    let mut out = Response::new(Body::new(incoming));
    *out.status_mut() = head.status;
    for name in RETURNED {
        if let Some(value) = head.headers.get(&name) {
            out.headers_mut().insert(name, value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_git_service_routes_are_proxied() {
        let q = |s| Some(s);
        assert!(service(
            &Method::GET,
            "info/refs",
            q("service=git-upload-pack")
        ));
        assert!(service(
            &Method::GET,
            "info/refs",
            q("service=git-receive-pack")
        ));
        assert!(service(&Method::POST, "git-upload-pack", None));
        assert!(service(&Method::POST, "git-receive-pack", None));
        assert!(!service(&Method::GET, "info/refs", None));
        assert!(!service(&Method::GET, "HEAD", None));
        assert!(!service(&Method::POST, "objects/info/packs", None));
    }
    #[test]
    fn credential_from_bearer_or_basic() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(presented(&h).as_deref(), Some("abc"));
        let basic = base64::engine::general_purpose::STANDARD.encode("git:xyz");
        let value = HeaderValue::from_str(&format!("Basic {basic}")).unwrap();
        h.insert(header::AUTHORIZATION, value);
        assert_eq!(presented(&h).as_deref(), Some("xyz"));
        h.remove(header::AUTHORIZATION);
        assert_eq!(presented(&h), None);
    }
}
