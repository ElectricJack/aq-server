//! A stand-in for aq-git's git smart-HTTP socket (G2), for tests: `git
//! http-backend` over bare repositories `<root>/<repository_id>`, recording
//! each request's path and headers. It implements none of aq-git's checks.
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::io::AsyncWriteExt;

pub type Seen = Arc<Mutex<Vec<(String, HeaderMap)>>>;
pub struct FakeGitHttp {
    /// `(path?query, headers)` of every request received.
    pub seen: Seen,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeGitHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
/// Serves `root` on the Unix socket `socket`.
pub fn serve(socket: &Path, root: &Path) -> FakeGitHttp {
    let listener = tokio::net::UnixListener::bind(socket).expect("bind fake git socket");
    let seen = Seen::default();
    let app = Router::new()
        .fallback(cgi)
        .with_state((root.to_path_buf(), seen.clone()));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake git socket");
    });
    FakeGitHttp { seen, task }
}
async fn cgi(State((root, seen)): State<(PathBuf, Seen)>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let target = parts
        .uri
        .path_and_query()
        .map_or("", |p| p.as_str())
        .to_owned();
    seen.lock().unwrap().push((target, parts.headers.clone()));
    let bytes = body.collect().await.unwrap().to_bytes();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    let mut child = tokio::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", &root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("REMOTE_USER", "aq")
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("PATH_INFO", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", bytes.len().to_string())
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("GIT_PROTOCOL", header("git-protocol"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git http-backend");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&bytes).await.unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    let out = output.stdout;
    let (end, skip) = (out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4)))
    .or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2)))
    .expect("CGI headers");
    let mut response = Response::new(Body::from(out[end + skip..].to_vec()));
    for line in String::from_utf8_lossy(&out[..end]).lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value.split(' ').next().unwrap().parse().unwrap();
            *response.status_mut() = StatusCode::from_u16(code).unwrap();
        } else if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::try_from(name),
            axum::http::HeaderValue::try_from(value),
        ) {
            response.headers_mut().append(n, v);
        }
    }
    response
}
