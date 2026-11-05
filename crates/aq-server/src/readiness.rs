//! Loopback-only updater readiness and a separate dependency readiness surface.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use aq_release::ReadinessReport;
use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::config::AuthConfig;
use crate::error::AppError;
use crate::{AppState, version_info};

async fn is_loopback(req: Request) -> bool {
    let (mut parts, _) = req.into_parts();
    // Fail closed: no peer address (e.g. a router not served with connect
    // info) is treated as non-loopback.
    // The extractor also honours `MockConnectInfo`, which tests rely on.
    ConnectInfo::<SocketAddr>::from_request_parts(&mut parts, &())
        .await
        .is_ok_and(|ConnectInfo(a)| a.ip().to_canonical().is_loopback())
}

pub async fn readyz(State(state): State<AppState>, req: Request) -> Response {
    if !is_loopback(req).await {
        return AppError::NotFound("no such route".into()).into_response();
    }
    let info = version_info();
    let db_ok = state
        .store
        .control_view()
        .ping(Duration::from_secs(2))
        .await;
    let report = ReadinessReport {
        version: info
            .server_version
            .parse()
            .expect("CARGO_PKG_VERSION is semver"),
        source_revision: info.source_revision,
        state_format: info.state_format_version,
        protocol_min: info.protocol_min,
        protocol_max: info.protocol_max,
        database: if db_ok { "ok" } else { "error" }.into(),
        auth_mode: match state.config.auth {
            AuthConfig::Dev { .. } => "dev",
            AuthConfig::CloudflareAccess { .. } => "cloudflare_access",
        }
        .into(),
    };
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(report)).into_response()
}

#[derive(serde::Serialize)]
struct DependencyReadinessReport {
    status: &'static str,
    database: &'static str,
    dependencies: std::collections::BTreeMap<&'static str, &'static str>,
}

pub async fn dependency_readyz(State(state): State<AppState>, req: Request) -> Response {
    if !is_loopback(req).await {
        return AppError::NotFound("no such route".into()).into_response();
    }
    let control = state.store.control_view();
    let (db_ok, graph_ok, git_ok) = tokio::join!(
        control.ping(Duration::from_secs(2)),
        async {
            match &state.config.agent_access {
                Some(config) => Some(graph_ready(&config.graph_url).await),
                None => None,
            }
        },
        async {
            match &state.config.source_retention {
                Some(config) => Some(git_ready(&config.git_socket).await),
                None => None,
            }
        }
    );
    let ready = db_ok && graph_ok != Some(false) && git_ok != Some(false);
    let report = DependencyReadinessReport {
        status: if ready { "ready" } else { "degraded" },
        database: if db_ok { "ok" } else { "error" },
        dependencies: [("graph", graph_ok), ("aq_git", git_ok)]
            .into_iter()
            .filter_map(|(name, ok)| ok.map(|ok| (name, if ok { "ready" } else { "degraded" })))
            .collect(),
    };
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(report)).into_response()
}

async fn graph_ready(origin: &str) -> bool {
    let probe = async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .ok()?;
        let mut response = client
            .get(format!("{}/health/ready", origin.trim_end_matches('/')))
            .send()
            .await
            .ok()?;
        if response.status() != StatusCode::OK {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > 4096 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        (body["status"] == "ready" && body["api_version"] == "1").then_some(())
    };
    tokio::time::timeout(Duration::from_secs(1), probe)
        .await
        .is_ok_and(|v| v.is_some())
}

async fn git_ready(socket: &std::path::Path) -> bool {
    let probe = async {
        let id = uuid::Uuid::new_v4().to_string();
        let request = serde_json::json!({"protocol_version":1,"request_id":id,"operation":"capabilities","params":{}});
        let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        let mut bytes = serde_json::to_vec(&request).ok()?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await.ok()?;
        let mut reader = BufReader::new(stream.take(65_537));
        bytes.clear();
        reader.read_until(b'\n', &mut bytes).await.ok()?;
        if bytes.len() > 65_536 || bytes.last() != Some(&b'\n') {
            return None;
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        (body["protocol_version"] == 1
            && body["request_id"] == id
            && body["ok"] == true
            && body.get("error").is_none_or(serde_json::Value::is_null)
            && body["result"]["protocol_version"] == 1
            && body["result"]["operations"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(|op| op == "repo.show")))
        .then_some(())
    };
    tokio::time::timeout(Duration::from_secs(1), probe)
        .await
        .is_ok_and(|v| v.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn git_probe_rejects_uncorrelated_malformed_and_stalled_frames() {
        for frame in [Some(b"not-json\n".as_slice()), Some(b"{\"protocol_version\":1,\"request_id\":\"other\",\"ok\":true,\"result\":{\"protocol_version\":1,\"operations\":[\"repo.show\"]}}\n".as_slice()), None] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("git.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                if let Some(frame) = frame { stream.write_all(frame).await.unwrap(); }
                else { std::future::pending::<()>().await; }
            });
            assert!(!tokio::time::timeout(Duration::from_secs(2), git_ready(&path)).await.unwrap());
            peer.abort();
        }
    }

    #[tokio::test]
    async fn graph_probe_rejects_false_success_and_stalled_http() {
        for body in [
            Some("{}"),
            Some(r#"{"status":"ready","api_version":"99"}"#),
            None,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                if let Some(body) = body {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                } else {
                    std::future::pending::<()>().await;
                }
            });
            assert!(
                !tokio::time::timeout(Duration::from_secs(2), graph_ready(&origin))
                    .await
                    .unwrap()
            );
            peer.abort();
        }
    }
}
