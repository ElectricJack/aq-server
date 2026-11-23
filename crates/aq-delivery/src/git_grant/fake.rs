//! A fake aq-git git-HTTP socket for tests. It refuses a request whose grant
//! headers are missing or name another repository, records the headers of
//! every request, and serves it with `git http-backend` from one bare
//! repository, `<dir>/git-http-repositories/<repository>`. It enforces nothing
//! else of the grant: the server and aq-git do that.
use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub struct FakeGitHttp {
    pub socket: PathBuf,
    /// The bare repository served for the repository ID.
    pub repository: PathBuf,
    /// The commit the main ref points at.
    pub base: String,
    /// The headers of every request received, in order.
    pub seen: Arc<Mutex<Vec<HeaderMap>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeGitHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Runs `git -C dir args…` with a fixed identity and no global configuration,
/// and returns its trimmed standard output.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "aq")
        .env("GIT_AUTHOR_EMAIL", "aq@example.invalid")
        .env("GIT_COMMITTER_NAME", "aq")
        .env("GIT_COMMITTER_EMAIL", "aq@example.invalid")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl FakeGitHttp {
    /// Serves `repository` on `dir/git-http.sock`; its `main_ref` points at one
    /// commit of the empty tree.
    pub async fn start(dir: &Path, repository: Uuid, main_ref: &str) -> Self {
        let root = dir.join("git-http-repositories");
        let bare = root.join(repository.to_string());
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-q"]);
        git(&bare, &["config", "http.receivepack", "true"]);
        let tree = git(&bare, &["mktree"]);
        let base = git(&bare, &["commit-tree", &tree, "-m", "base"]);
        git(&bare, &["update-ref", main_ref, &base]);
        git(&bare, &["symbolic-ref", "HEAD", main_ref]);
        let socket = dir.join("git-http.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let served = seen.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (root, seen) = (root.clone(), served.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        serve(root.clone(), seen.clone(), request)
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            socket,
            repository: bare,
            base,
            seen,
            task,
        }
    }
}

fn plain(status: StatusCode, text: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(text.as_bytes())));
    *response.status_mut() = status;
    response
}

async fn serve(
    root: PathBuf,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let (parts, body) = request.into_parts();
    seen.lock().unwrap().push(parts.headers.clone());
    if !valid_grant(&parts) {
        return Ok(plain(StatusCode::BAD_REQUEST, "invalid_grant"));
    }
    let Ok(body) = body.collect().await else {
        return Ok(plain(StatusCode::BAD_REQUEST, "unreadable body"));
    };
    let body = body.to_bytes();
    let output = backend(root, &parts, body).await;
    Ok(cgi_response(&output))
}

fn header<'a>(parts: &'a http::request::Parts, name: &str) -> &'a str {
    parts
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

fn valid_grant(parts: &http::request::Parts) -> bool {
    let first = parts.uri.path().trim_start_matches('/').split('/').next();
    let repository_matches = Some(header(parts, "aq-repository")) == first;
    let valid_range = header(parts, "aq-base").len() == 40
        && header(parts, "aq-task-ref").starts_with("refs/heads/aq/");
    let operation = Uuid::try_parse(header(parts, "aq-operation")).is_ok();
    let bounded = header(parts, "aq-max-push-bytes")
        .parse::<u64>()
        .unwrap_or(0)
        > 0;
    [repository_matches, valid_range, operation, bounded]
        .into_iter()
        .all(|valid| valid)
}

async fn backend(root: PathBuf, parts: &http::request::Parts, body: Bytes) -> Vec<u8> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("http-backend")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_PROJECT_ROOT", &root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", parts.uri.path())
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("QUERY_STRING", parts.uri.query().unwrap_or_default())
        .env("CONTENT_TYPE", header(parts, "content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_USER", "aq")
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if !header(parts, "content-encoding").is_empty() {
        command.env("HTTP_CONTENT_ENCODING", header(parts, "content-encoding"));
    }
    let mut child = command.spawn().expect("run git http-backend");
    let mut stdin = child.stdin.take().unwrap();
    let write = async move {
        let _ = stdin.write_all(&body).await;
    };
    let (_, output) = tokio::join!(write, child.wait_with_output());
    output.expect("git http-backend output").stdout
}

fn cgi_response(output: &[u8]) -> Response<Full<Bytes>> {
    let split = (output
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4)))
    .or_else(|| output.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
    let Some((end, gap)) = split else {
        return plain(StatusCode::BAD_GATEWAY, "no CGI header");
    };
    let mut response = Response::new(Full::new(Bytes::copy_from_slice(&output[end + gap..])));
    for line in String::from_utf8_lossy(&output[..end]).lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value.split(' ').next().and_then(|c| c.parse().ok());
            *response.status_mut() = code
                .and_then(|c| StatusCode::from_u16(c).ok())
                .unwrap_or(StatusCode::BAD_GATEWAY);
        } else if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.trim().as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            response.headers_mut().append(name, value);
        }
    }
    response
}
