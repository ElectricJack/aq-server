//! Bounded same-origin human API client. No redirects, proxies, automatic
//! retries, node credentials, cookie jar, secret Debug output or mutation retry.
pub mod readiness;
use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    time::Duration,
};
use zeroize::Zeroizing;
#[derive(Clone)]
pub enum Authentication {
    CfAccessTokenFile(PathBuf),
    #[cfg(any(test, feature = "loopback-development"))]
    LoopbackDevelopmentEmail(String),
}
impl std::fmt::Debug for Authentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CfAccessTokenFile(_) => "CfAccessTokenFile(<private>)",
            #[cfg(any(test, feature = "loopback-development"))]
            Self::LoopbackDevelopmentEmail(_) => "LoopbackDevelopmentEmail(<private>)",
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_request_bytes: 262144,
            max_response_bytes: 1048576,
            timeout: Duration::from_secs(25),
        }
    }
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid human API origin/path/limits")]
    Configuration,
    #[error("unsafe or unavailable private human credential")]
    Credential,
    #[error("human request exceeds byte budget")]
    RequestBound,
    #[error("human API transport failed; mutation outcome may be unknown")]
    Transport,
    #[error("human API returned HTTP {0}")]
    Status(u16),
    #[error("human response exceeds byte budget")]
    ResponseBound,
    #[error("invalid typed human response")]
    Response,
}
/// A bounded JSON response for clients consuming versioned API error envelopes.
/// Error bodies are parsed for the caller, never logged or interpolated into errors.
pub struct JsonResponse {
    pub status: u16,
    pub body: serde_json::Value,
}
struct RawResponse {
    status: u16,
    bytes: Vec<u8>,
}
impl RawResponse {
    fn json(self) -> Result<JsonResponse, Error> {
        let body = match serde_json::from_slice(&self.bytes) {
            Ok(body) => body,
            Err(_) if !(200..300).contains(&self.status) => serde_json::Value::Null,
            Err(_) => return Err(Error::Response),
        };
        Ok(JsonResponse {
            status: self.status,
            body,
        })
    }
}
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    origin: Url,
    /// None on a channel: there is no browser origin to assert.
    origin_header: Option<HeaderValue>,
    /// None on a channel: the proxy behind the socket holds the credential.
    auth_header: Option<(HeaderName, HeaderValue)>,
    limits: Limits,
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HumanClient")
            .field("origin", &self.origin.origin().ascii_serialization())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}
#[cfg(any(test, feature = "loopback-development"))]
fn loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}
fn token(path: &std::path::Path) -> Result<HeaderValue, Error> {
    if !path.is_absolute() {
        return Err(Error::Credential);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| Error::Credential)?;
    let metadata = file.metadata().map_err(|_| Error::Credential)?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.nlink() != 1
    {
        return Err(Error::Credential);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Credential)?;
    if bytes.len() > 16384 {
        return Err(Error::Credential);
    }
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let pieces: Vec<_> = bytes.split(|b| *b == b'.').collect();
    if pieces.len() != 3
        || pieces.iter().any(|p| p.is_empty())
        || bytes
            .iter()
            .any(|b| !(b.is_ascii_alphanumeric() || b"-_.".contains(b)))
    {
        return Err(Error::Credential);
    }
    let mut value = HeaderValue::from_bytes(bytes).map_err(|_| Error::Credential)?;
    value.set_sensitive(true);
    Ok(value)
}
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > self.limit)
        {
            return Err(std::io::Error::other("bounded human request"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn valid_limits(limits: &Limits) -> bool {
    !(limits.max_request_bytes == 0
        || limits.max_request_bytes > 1048576
        || limits.max_response_bytes == 0
        || limits.max_response_bytes > 8388608
        || limits.timeout.is_zero()
        || limits.timeout > Duration::from_secs(30))
}
fn builder(limits: &Limits) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .timeout(limits.timeout)
        .connect_timeout(limits.timeout.min(Duration::from_secs(5)))
}
impl Client {
    /// A client for the sandbox AQ channel (spec 0006 §4.1, §5): plain HTTP/1.1
    /// over a local Unix socket. It sends no credential and no `Origin`; the
    /// channel proxy behind the socket injects the attempt's credential. The
    /// socket path must be absolute. Every other rule (bounds, no redirects,
    /// no proxies, no retries) is the same as [`Client::new`].
    pub fn channel(socket: &std::path::Path, limits: Limits) -> Result<Self, Error> {
        if !valid_limits(&limits)
            || !socket.is_absolute()
            || socket.as_os_str().len() > 107
            || socket
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::Configuration);
        }
        let http = builder(&limits)
            .unix_socket(socket)
            .build()
            .map_err(|_| Error::Configuration)?;
        Ok(Self {
            http,
            origin: Url::parse("http://localhost/").map_err(|_| Error::Configuration)?,
            origin_header: None,
            auth_header: None,
            limits,
        })
    }
    pub fn new(
        origin: &str,
        authentication: Authentication,
        limits: Limits,
    ) -> Result<Self, Error> {
        if !valid_limits(&limits) {
            return Err(Error::Configuration);
        }
        let url = Url::parse(origin).map_err(|_| Error::Configuration)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || url.host().is_none()
        {
            return Err(Error::Configuration);
        }
        let builder = builder(&limits);
        let (auth_header, builder) = match authentication {
            Authentication::CfAccessTokenFile(path) => {
                if url.scheme() != "https" {
                    return Err(Error::Configuration);
                }
                (
                    (
                        HeaderName::from_static("cf-access-jwt-assertion"),
                        token(&path)?,
                    ),
                    builder,
                )
            }
            #[cfg(any(test, feature = "loopback-development"))]
            Authentication::LoopbackDevelopmentEmail(email) => {
                if url.scheme() != "http"
                    || !loopback(&url)
                    || email.is_empty()
                    || email.len() > 320
                    || email.chars().any(char::is_control)
                {
                    return Err(Error::Configuration);
                }
                let builder = if url.host_str() == Some("localhost") {
                    let port = url.port_or_known_default().ok_or(Error::Configuration)?;
                    builder.resolve_to_addrs(
                        "localhost",
                        &[
                            std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
                            std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port),
                        ],
                    )
                } else {
                    builder
                };
                let mut value = HeaderValue::from_str(&email).map_err(|_| Error::Configuration)?;
                value.set_sensitive(true);
                ((HeaderName::from_static("x-aq-dev-email"), value), builder)
            }
        };
        let origin_header = HeaderValue::from_str(&url.origin().ascii_serialization())
            .map_err(|_| Error::Configuration)?;
        Ok(Self {
            http: builder.build().map_err(|_| Error::Configuration)?,
            origin: url,
            origin_header: Some(origin_header),
            auth_header: Some(auth_header),
            limits,
        })
    }
    fn endpoint(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, Error> {
        if !path.starts_with("/api/")
            || path.len() > 2048
            || path.contains("//")
            || path.split('/').any(|s| s == "." || s == "..")
            || path
                .bytes()
                .any(|b| !b.is_ascii_alphanumeric() && !b"/-_.".contains(&b))
            || query.len() > 16
            || query.iter().any(|(k, v)| k.len() > 64 || v.len() > 4096)
        {
            return Err(Error::Configuration);
        }
        let mut url = self.origin.join(path).map_err(|_| Error::Configuration)?;
        if url.origin() != self.origin.origin() {
            return Err(Error::Configuration);
        }
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }
    pub async fn get_json<R: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<R, Error> {
        let url = self.endpoint(path, query)?;
        self.request(reqwest::Method::GET, url, None).await
    }
    pub async fn post_json<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, Error> {
        let url = self.endpoint(path, &[])?;
        let mut writer = Bounded {
            bytes: vec![],
            limit: self.limits.max_request_bytes,
        };
        serde_json::to_writer(&mut writer, body).map_err(|_| Error::RequestBound)?;
        self.request(reqwest::Method::POST, url, Some(writer.bytes))
            .await
    }
    /// Same transport and budgets as get_json, including bounded non-2xx JSON.
    pub async fn get_json_response(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<JsonResponse, Error> {
        self.request_raw(
            reqwest::Method::GET,
            self.endpoint(path, query)?,
            None,
            false,
        )
        .await?
        .json()
    }
    /// Single submission; callers must inspect status and the typed error envelope.
    pub async fn post_json_response<T: Serialize>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<JsonResponse, Error> {
        let url = self.endpoint(path, &[])?;
        let mut writer = Bounded {
            bytes: vec![],
            limit: self.limits.max_request_bytes,
        };
        serde_json::to_writer(&mut writer, body).map_err(|_| Error::RequestBound)?;
        self.request_raw(reqwest::Method::POST, url, Some(writer.bytes), false)
            .await?
            .json()
    }
    async fn request<R: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<R, Error> {
        let raw = self.request_raw(method, url, body, true).await?;
        serde_json::from_slice(&raw.bytes).map_err(|_| Error::Response)
    }
    async fn request_raw(
        &self,
        method: reqwest::Method,
        url: Url,
        body: Option<Vec<u8>>,
        reject_errors: bool,
    ) -> Result<RawResponse, Error> {
        let mut headers = HeaderMap::new();
        if let Some((name, value)) = &self.auth_header {
            headers.insert(name.clone(), value.clone());
        }
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        if method == reqwest::Method::POST {
            headers.insert(
                HeaderName::from_static("x-aq-request"),
                HeaderValue::from_static("1"),
            );
            if let Some(origin) = &self.origin_header {
                headers.insert(reqwest::header::ORIGIN, origin.clone());
            }
        }
        let mut request = self.http.request(method, url).headers(headers);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        // No automatic retry even for transport ambiguity. Caller owns stable op IDs
        // and explicit receipt/unknown reconciliation; input can never be blindly sent.
        let mut response = request.send().await.map_err(|_| Error::Transport)?;
        if reject_errors && !response.status().is_success() {
            return Err(Error::Status(response.status().as_u16()));
        }
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|n| n > self.limits.max_response_bytes as u64)
        {
            return Err(Error::ResponseBound);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|n| n > self.limits.max_response_bytes)
            {
                return Err(Error::ResponseBound);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(RawResponse { status, bytes })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn auth() -> Authentication {
        Authentication::LoopbackDevelopmentEmail("private-user@example.test".into())
    }
    async fn server(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 8192];
            let mut n = 0;
            while !bytes[..n].windows(4).any(|v| v == b"\r\n\r\n") {
                let read = stream.read(&mut bytes[n..]).await.unwrap();
                assert!(read > 0);
                n += read;
                assert!(n < bytes.len());
            }
            let request = String::from_utf8_lossy(&bytes[..n]).to_string();
            stream.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (origin, task)
    }
    // Channel mode (spec 0006 §4.1): HTTP over a Unix socket with no
    // credential, no Origin and no development identity; the proxy adds them.
    #[tokio::test]
    async fn channel_speaks_http_over_a_unix_socket_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("aq.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 8192];
            let mut n = 0;
            while !bytes[..n].windows(4).any(|v| v == b"\r\n\r\n") {
                let read = stream.read(&mut bytes[n..]).await.unwrap();
                assert!(read > 0);
                n += read;
            }
            let body = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
            stream.write_all(body).await.unwrap();
            String::from_utf8_lossy(&bytes[..n]).to_lowercase()
        });
        let client = Client::channel(&socket, Limits::default()).unwrap();
        let value: serde_json::Value = client
            .post_json("/api/v1/commands/whoami", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(value["ok"], true);
        let raw = task.await.unwrap();
        assert!(
            raw.starts_with("post /api/v1/commands/whoami http/1.1"),
            "{raw}"
        );
        assert!(raw.contains("x-aq-request: 1"));
        for header in ["authorization", "origin:", "cf-access", "x-aq-dev-email"] {
            assert!(!raw.contains(header), "{header} in {raw}");
        }
        assert_eq!(
            Client::channel(std::path::Path::new("relative.sock"), Limits::default()).unwrap_err(),
            Error::Configuration
        );
        let missing = Client::channel(&dir.path().join("gone.sock"), Limits::default()).unwrap();
        let result: Result<serde_json::Value, _> = missing.get_json("/api/v1/commands", &[]).await;
        assert_eq!(result.unwrap_err(), Error::Transport);
    }
    #[tokio::test]
    async fn mutation_sends_same_origin_and_csrf_headers_and_never_follows_redirect() {
        let (target, task) =
            server("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}").await;
        let client = Client::new(&target, auth(), Limits::default()).unwrap();
        let value: serde_json::Value = client
            .post_json(
                "/api/v1/test",
                &serde_json::json!({"operation_id":"stable"}),
            )
            .await
            .unwrap();
        assert_eq!(value["ok"], true);
        let raw = task.await.unwrap().to_lowercase();
        assert!(raw.contains("x-aq-request: 1"));
        assert!(raw.contains(&format!("origin: {target}")));
        assert!(raw.contains("x-aq-dev-email: private-user@example.test"));
        assert!(!format!("{client:?}").contains("private-user"));
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/api/v1/leak\r\nContent-Length: 0\r\n\r\n",
            destination.local_addr().unwrap()
        );
        let response: &'static str = Box::leak(redirect.into_boxed_str());
        let (origin, task) = server(response).await;
        let client = Client::new(&origin, auth(), Limits::default()).unwrap();
        let result: Result<serde_json::Value, _> = client
            .post_json("/api/v1/test", &serde_json::json!({}))
            .await;
        assert!(matches!(result, Err(Error::Status(307))));
        task.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), destination.accept())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn responses_requests_and_error_material_are_bounded() {
        let (origin, task) = server("HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n").await;
        let client = Client::new(
            &origin,
            auth(),
            Limits {
                max_response_bytes: 16,
                ..Limits::default()
            },
        )
        .unwrap();
        let result: Result<serde_json::Value, _> = client.get_json("/api/v1/test", &[]).await;
        assert!(matches!(result, Err(Error::ResponseBound)));
        task.await.unwrap();
        let (chunk_origin,chunk_task)=server("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n20\r\n\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"\r\n0\r\n\r\n").await;
        let chunk_client = Client::new(
            &chunk_origin,
            auth(),
            Limits {
                max_response_bytes: 16,
                ..Limits::default()
            },
        )
        .unwrap();
        let chunk: Result<serde_json::Value, _> = chunk_client.get_json("/api/v1/test", &[]).await;
        assert!(matches!(chunk, Err(Error::ResponseBound)));
        chunk_task.await.unwrap();
        let result: Result<serde_json::Value, _> =
            client.post_json("/api/v1/test", &"x".repeat(262145)).await;
        assert!(matches!(result, Err(Error::RequestBound)));
        let (origin, task) =
            server("HTTP/1.1 403 Forbidden\r\nContent-Length: 20\r\n\r\nprivate-secret-error")
                .await;
        let client = Client::new(&origin, auth(), Limits::default()).unwrap();
        let result: Result<serde_json::Value, _> = client.get_json("/api/v1/test", &[]).await;
        let err = result.unwrap_err();
        assert_eq!(err, Error::Status(403));
        assert!(!format!("{err:?}{err}").contains("secret"));
        task.await.unwrap();
    }
    #[tokio::test]
    async fn transport_timeout_attempts_a_mutation_only_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let task = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            seen.fetch_add(1, Ordering::SeqCst);
            let mut request = [0; 4096];
            let _ = first.read(&mut request).await;
            tokio::time::sleep(Duration::from_millis(80)).await;
            if tokio::time::timeout(Duration::from_millis(40), listener.accept())
                .await
                .is_ok()
            {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        });
        let client = Client::new(
            &origin,
            auth(),
            Limits {
                timeout: Duration::from_millis(30),
                ..Limits::default()
            },
        )
        .unwrap();
        let result: Result<serde_json::Value, _> = client
            .post_json("/api/v1/test", &serde_json::json!({"operation_id":"one"}))
            .await;
        assert!(matches!(result, Err(Error::Transport)));
        task.await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn unsafe_origins_paths_and_nonloopback_dev_are_refused() {
        for origin in [
            "http://example.com",
            "http://user:pass@127.0.0.1",
            "http://127.0.0.1/path",
            "http://127.0.0.1/?token=secret",
            "http://127.0.0.1/#secret",
        ] {
            assert!(Client::new(origin, auth(), Limits::default()).is_err());
        }
        let client = Client::new("http://127.0.0.1:1", auth(), Limits::default()).unwrap();
        for path in [
            "https://other/api/x",
            "//other/api/x",
            "/v1/worker/enroll",
            "/api/../worker",
            "/api/%2e%2e/x",
            "/api/x?token=secret",
        ] {
            assert!(client.endpoint(path, &[]).is_err(), "{path}");
        }
        assert_eq!(
            client
                .endpoint("/api/v1/test", &[("cursor", "https://other/?x=1")])
                .unwrap()
                .host_str(),
            Some("127.0.0.1")
        );
    }
    #[test]
    fn private_jwt_files_refuse_symlinks_hardlinks_permissions_fifo_and_node_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");
        std::fs::write(&path, b"header.payload.signature\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let header = token(&path).unwrap();
        assert!(header.is_sensitive());
        assert!(!format!("{header:?}").contains("payload"));
        let link = dir.path().join("symlink");
        symlink(&path, &link).unwrap();
        assert!(token(&link).is_err());
        let hard = dir.path().join("hard");
        std::fs::hard_link(&path, &hard).unwrap();
        assert!(token(&path).is_err());
        std::fs::remove_file(hard).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(token(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, b"aqn_node_credential").unwrap();
        assert!(token(&path).is_err());
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(token(&fifo).is_err());
    }
}

/// Same-origin mutation rule shared by AQ's owner routes.
pub fn csrf_rejection(
    method: &reqwest::Method,
    headers: &reqwest::header::HeaderMap,
    expected: &str,
    development: bool,
) -> Option<&'static str> {
    if matches!(*method, reqwest::Method::GET | reqwest::Method::HEAD) {
        return None;
    }
    if headers
        .get("x-aq-request")
        .is_none_or(|v| v.as_bytes() != b"1")
    {
        return Some("missing X-AQ-Request: 1 header");
    }
    if headers
        .get("origin")
        .is_some_and(|origin| !origin_allowed(expected, origin.as_bytes(), development))
    {
        return Some("request origin is not allowed");
    }
    None
}

/// Exact origin, with the existing development-only loopback alias.
pub fn origin_allowed(expected: &str, origin: &[u8], development: bool) -> bool {
    let alias = |from: &str, to: &str| {
        let (scheme, rest) = expected.split_once("://")?;
        let port = rest.strip_prefix(from)?;
        (port.is_empty() || port.starts_with(':')).then(|| format!("{scheme}://{to}{port}"))
    };
    origin == expected.as_bytes()
        || (development
            && alias("127.0.0.1", "localhost")
                .or_else(|| alias("localhost", "127.0.0.1"))
                .is_some_and(|alias| origin == alias.as_bytes()))
}
