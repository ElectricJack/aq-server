//! A small typed GitHub REST client. It sets GitHub's media type, API version
//! and user agent, and maps error statuses onto [`GitHubApiError`]. It never
//! logs, and no error carries a request header, so tokens cannot leak.
use super::{GitHubApiError, GitHubResult, InstallationToken};
use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use std::time::Duration;

const MEDIA_TYPE: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
const AGENT: &str = "aq-server";

/// The credential a request is sent with.
pub enum Auth<'a> {
    /// The App JWT, for `/app/...` endpoints.
    App(&'a str),
    Installation(&'a InstallationToken),
}

#[derive(Debug, Clone)]
pub struct GitHubClient {
    http: reqwest::Client,
    base: Url,
}

/// An `http(s)` URL without credentials, query or fragment. A path prefix
/// (GitHub Enterprise `/api/v3`) is kept.
pub(super) fn parse_base(raw: &str) -> Result<Url, &'static str> {
    let url = Url::parse(raw).map_err(|_| "api_base is not a URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("api_base must be an http(s) URL without credentials, query or fragment");
    }
    Ok(url)
}

impl GitHubClient {
    pub fn new(api_base: &str, timeout: Duration) -> GitHubResult<Self> {
        let base = parse_base(api_base).map_err(GitHubApiError::InvalidInput)?;
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(MEDIA_TYPE));
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static(AGENT));
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| GitHubApiError::Transport("cannot build the HTTP client".into()))?;
        Ok(Self { http, base })
    }
    pub fn base(&self) -> &Url {
        &self.base
    }
    fn url(&self, path: &str) -> GitHubResult<Url> {
        if !path.starts_with('/') || path.contains("..") || path.contains(['?', '#']) {
            return Err(GitHubApiError::InvalidInput("invalid API path"));
        }
        let joined = format!("{}{path}", self.base.as_str().trim_end_matches('/'));
        Url::parse(&joined).map_err(|_| GitHubApiError::InvalidInput("invalid API path"))
    }

    /// Sends one request and decodes a success body as `T`. An empty body
    /// (204) decodes as JSON `null`, so `()` and `Option<_>` work.
    pub async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        auth: Auth<'_>,
        body: Option<&serde_json::Value>,
    ) -> GitHubResult<T> {
        self.send_query(method, path, &[], auth, body).await
    }

    /// [`send`](Self::send) with query parameters, percent-encoded here.
    pub async fn send_query<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        auth: Auth<'_>,
        body: Option<&serde_json::Value>,
    ) -> GitHubResult<T> {
        let mut url = self.url(path)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let secret = match auth {
            Auth::App(jwt) => jwt,
            Auth::Installation(token) => token.secret(),
        };
        let mut authorization = HeaderValue::try_from(format!("Bearer {secret}"))
            .map_err(|_| GitHubApiError::InvalidInput("credential is not a header value"))?;
        authorization.set_sensitive(true);
        let mut request = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, authorization);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| GitHubApiError::Transport(e.without_url().to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| GitHubApiError::Transport(e.without_url().to_string()))?;
        if status.is_success() {
            let bytes: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
            // serde's message can quote body values (a token); keep only
            // the error class and position.
            return serde_json::from_slice(bytes).map_err(|e| {
                GitHubApiError::Protocol(format!(
                    "unexpected response body ({:?} at line {} column {})",
                    e.classify(),
                    e.line(),
                    e.column()
                ))
            });
        }
        Err(map_error(status, &headers, &bytes, Utc::now()))
    }
}

impl GitHubClient {
    /// The last `keep + 1` bytes (at most) of a text download that `path`
    /// answers with a redirect, such as a job's logs. The redirect target is a
    /// signed URL: it is fetched once, without the installation token, and
    /// read as a stream so that a long log is never held whole.
    pub async fn send_text_tail(
        &self,
        path: &str,
        auth: Auth<'_>,
        keep: usize,
    ) -> GitHubResult<Vec<u8>> {
        let transport = |e: reqwest::Error| GitHubApiError::Transport(e.without_url().to_string());
        let url = self.url(path)?;
        let secret = match auth {
            Auth::App(jwt) => jwt,
            Auth::Installation(token) => token.secret(),
        };
        let mut authorization = HeaderValue::try_from(format!("Bearer {secret}"))
            .map_err(|_| GitHubApiError::InvalidInput("credential is not a header value"))?;
        authorization.set_sensitive(true);
        let first = self
            .http
            .get(url.clone())
            .header(AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(transport)?;
        let status = first.status();
        let response = if status.is_redirection() {
            let target = first
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|location| url.join(location).ok())
                .filter(|t| {
                    matches!(t.scheme(), "http" | "https")
                        && t.username().is_empty()
                        && t.password().is_none()
                })
                .ok_or_else(|| GitHubApiError::Protocol("unusable log redirect".into()))?;
            self.http
                .get(target)
                .header(reqwest::header::RANGE, format!("bytes=-{}", keep + 1))
                .send()
                .await
                .map_err(transport)?
        } else {
            first
        };
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = response.bytes().await.map_err(transport)?;
            return Err(map_error(status, &headers, &body, Utc::now()));
        }
        let mut response = response;
        let window = keep + 1;
        let mut tail: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            tail.extend_from_slice(&chunk);
            if tail.len() > 2 * window {
                tail.drain(..tail.len() - window);
            }
        }
        if tail.len() > window {
            tail.drain(..tail.len() - window);
        }
        Ok(tail)
    }
}

/// GitHub's `message`, followed by any `errors[].message` details.
fn message(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::from_utf8_lossy(body).trim().to_string();
    };
    let mut parts: Vec<String> = v["message"]
        .as_str()
        .map(str::to_string)
        .into_iter()
        .collect();
    if let Some(errors) = v["errors"].as_array() {
        parts.extend(
            errors
                .iter()
                .filter_map(|e| e["message"].as_str().or(e.as_str()))
                .map(str::to_string),
        );
    }
    parts.join("; ")
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// When GitHub says the limit resets: `retry-after` (seconds) takes
/// precedence, then `x-ratelimit-reset` (epoch seconds).
fn reset_at(headers: &HeaderMap, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(secs) = header(headers, "retry-after").and_then(|v| v.parse::<i64>().ok()) {
        return Some(now + chrono::Duration::seconds(secs.max(0)));
    }
    header(headers, "x-ratelimit-reset")
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
}

pub(super) fn map_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    now: DateTime<Utc>,
) -> GitHubApiError {
    let message = message(body);
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && (header(headers, "x-ratelimit-remaining") == Some("0")
                || headers.contains_key("retry-after")
                || {
                    let m = message.to_ascii_lowercase();
                    m.contains("rate limit") || m.contains("abuse")
                }));
    match status {
        _ if rate_limited => GitHubApiError::Retryable {
            reset_at: reset_at(headers, now),
        },
        StatusCode::NOT_FOUND => GitHubApiError::NotFound,
        StatusCode::UNPROCESSABLE_ENTITY => GitHubApiError::Invalid { message },
        _ => GitHubApiError::Status {
            status: status.as_u16(),
            message,
        },
    }
}
