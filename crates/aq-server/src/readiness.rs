//! Loopback-only updater readiness and a separate dependency readiness surface.

use aq_human_http::readiness::{git_ready, graph_ready};
use std::net::SocketAddr;
use std::time::Duration;

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
