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

impl Auth<'_> {
    fn header(self) -> GitHubResult<HeaderValue> {
        let secret = match self {
            Self::App(jwt) => jwt,
            Self::Installation(token) => token.secret(),
        };
        let mut value = HeaderValue::try_from(format!("Bearer {secret}"))
            .map_err(|_| GitHubApiError::InvalidInput("credential is not a header value"))?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// The method, repository-relative path and payload for one GitHub API call.
pub struct ApiRequest<'a> {
    method: Method,
    path: std::borrow::Cow<'a, str>,
    query: &'a [(&'a str, &'a str)],
    body: Option<&'a serde_json::Value>,
}

impl<'a> ApiRequest<'a> {
    pub fn new(method: Method, path: &'a str) -> Self {
        Self {
            method,
            path: path.into(),
            query: &[],
            body: None,
        }
    }
    pub fn query(mut self, query: &'a [(&'a str, &'a str)]) -> Self {
        self.query = query;
        self
    }
    pub fn json(mut self, value: &'a serde_json::Value) -> Self {
        self.body = Some(value);
        self
    }
    pub(super) fn under(mut self, prefix: &str) -> Self {
        self.path = format!("{prefix}{}", self.path).into();
        self
    }
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
    let plain_path = url.query().is_none() && url.fragment().is_none();
    if !valid_http_endpoint(&url) || !plain_path {
        return Err("api_base must be an http(s) URL without credentials, query or fragment");
    }
    Ok(url)
}

fn valid_http_endpoint(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some() && no_credentials(url)
}
fn no_credentials(url: &Url) -> bool {
    url.username().is_empty() && url.password().is_none()
}
fn transport(error: reqwest::Error) -> GitHubApiError {
    GitHubApiError::Transport(error.without_url().to_string())
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
        let escapes_path = path.contains("..") || path.contains(['?', '#']);
        if !path.starts_with('/') || escapes_path {
            return Err(GitHubApiError::InvalidInput("invalid API path"));
        }
        let joined = format!("{}{path}", self.base.as_str().trim_end_matches('/'));
        Url::parse(&joined).map_err(|_| GitHubApiError::InvalidInput("invalid API path"))
    }

    /// Sends one request and decodes a success body as `T`. An empty body
    /// (204) decodes as JSON `null`, so `()` and `Option<_>` work.
    pub async fn send<T: DeserializeOwned>(
        &self,
        api: ApiRequest<'_>,
        auth: Auth<'_>,
    ) -> GitHubResult<T> {
        let mut url = self.url(&api.path)?;
        if !api.query.is_empty() {
            url.query_pairs_mut().extend_pairs(api.query);
        }
        let authorization = auth.header()?;
        let mut request = self
            .http
            .request(api.method, url)
            .header(AUTHORIZATION, authorization);
        if let Some(body) = api.body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.map_err(transport)?;
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
    /// One GraphQL request (`POST /graphql`; `/api/graphql` beside a GitHub
    /// Enterprise `/api/v3` base), its `data` decoded as `T`. GraphQL reports
    /// a refusal in `errors` with status 200: a rate limit comes back as
    /// `retryable`, `NOT_FOUND` as `not_found`, anything else as `invalid`
    /// with GitHub's messages.
    pub async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
        auth: Auth<'_>,
    ) -> GitHubResult<T> {
        #[derive(serde::Deserialize)]
        struct Reply {
            data: Option<serde_json::Value>,
            #[serde(default)]
            errors: Vec<serde_json::Value>,
        }
        let mut url = self.base.clone();
        let path = url.path().trim_end_matches('/');
        let path = match path.strip_suffix("/v3") {
            Some(api) => format!("{api}/graphql"),
            None => format!("{path}/graphql"),
        };
        url.set_path(&path);
        let body = serde_json::json!({"query": query, "variables": variables});
        let response = self
            .http
            .post(url)
            .header(AUTHORIZATION, auth.header()?)
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.map_err(transport)?;
        if !status.is_success() {
            return Err(map_error(status, &headers, &bytes, Utc::now()));
        }
        let reply: Reply = serde_json::from_slice(&bytes)
            .map_err(|_| GitHubApiError::Protocol("unexpected GraphQL response body".into()))?;
        if let Some(error) = reply.errors.first() {
            return Err(match error["type"].as_str() {
                Some("RATE_LIMITED") => GitHubApiError::Retryable { reset_at: None },
                Some("NOT_FOUND") => GitHubApiError::NotFound,
                _ => GitHubApiError::Invalid {
                    message: message(&bytes),
                },
            });
        }
        serde_json::from_value(reply.data.unwrap_or_default())
            .map_err(|_| GitHubApiError::Protocol("unexpected GraphQL data".into()))
    }

    /// A text download that `path` answers with a redirect, such as a job's
    /// logs, handed to `line` one line at a time (lossy UTF-8, each cut to
    /// `max_line` bytes). The redirect target is a signed URL: it is fetched
    /// once, without the installation token, and read as a stream so that a
    /// long log is never held whole. A read cut short has already handed
    /// over the lines before the cut.
    pub async fn send_text_lines(
        &self,
        path: &str,
        auth: Auth<'_>,
        max_line: usize,
        mut line: impl FnMut(&str),
    ) -> GitHubResult<()> {
        let url = self.url(path)?;
        let authorization = auth.header()?;
        let first = self
            .http
            .get(url.clone())
            .header(AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(transport)?;
        let mut response = if first.status().is_redirection() {
            let target = redirect_target(&first, &url)?;
            self.http.get(target).send().await.map_err(transport)?
        } else {
            first
        };
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = response.bytes().await.map_err(transport)?;
            return Err(map_error(status, &headers, &body, Utc::now()));
        }
        let mut pending: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            for piece in chunk.split_inclusive(|b| *b == b'\n') {
                let room = max_line.saturating_sub(pending.len()).min(piece.len());
                pending.extend_from_slice(&piece[..room]);
                if piece.ends_with(b"\n") {
                    line(&String::from_utf8_lossy(&pending));
                    pending.clear();
                }
            }
        }
        if !pending.is_empty() {
            line(&String::from_utf8_lossy(&pending));
        }
        Ok(())
    }
}

fn redirect_target(response: &reqwest::Response, base: &Url) -> GitHubResult<Url> {
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|location| base.join(location).ok())
        .filter(|t| matches!(t.scheme(), "http" | "https") && no_credentials(t))
        .ok_or_else(|| GitHubApiError::Protocol("unusable log redirect".into()))
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
