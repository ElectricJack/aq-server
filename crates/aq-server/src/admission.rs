//! Independent, bounded request budgets. Control requests also use a separate
//! PostgreSQL pool, so held general/bulk work cannot consume their connections.
use std::{sync::Arc, time::Duration};

use aq_worker_protocol::{ErrorCode, ProtocolError};
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::sync::Semaphore;

pub const CONTROL_LIMIT: usize = 16;
pub const GENERAL_LIMIT: usize = 8;
pub const BULK_LIMIT: usize = 2;

pub struct Admission {
    control: Semaphore,
    general: Semaphore,
    bulk: Semaphore,
    notification: Semaphore,
    observation: Semaphore,
    observation_read: Semaphore,
    terminal_poll: Semaphore,
    terminal_output: Semaphore,
    terminal_interactive: Semaphore,
    terminal_read: Semaphore,
}
impl Default for Admission {
    fn default() -> Self {
        Self {
            control: Semaphore::new(CONTROL_LIMIT),
            general: Semaphore::new(GENERAL_LIMIT),
            bulk: Semaphore::new(BULK_LIMIT),
            notification: Semaphore::new(4),
            observation: Semaphore::new(2),
            observation_read: Semaphore::new(2),
            terminal_poll: Semaphore::new(32),
            terminal_output: Semaphore::new(4),
            terminal_interactive: Semaphore::new(8),
            terminal_read: Semaphore::new(4),
        }
    }
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Class {
    Control,
    General,
    Bulk,
    Notification,
    Observation,
    ObservationRead,
    TerminalPoll,
    TerminalOutput,
    TerminalInteractive,
    TerminalRead,
}
pub(crate) fn execution_cancel_path(path: &str) -> bool {
    path.strip_prefix("/api/execution/attempts/")
        .and_then(|rest| rest.strip_suffix("/cancel"))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}
pub(crate) fn agent_revoke_path(path: &str) -> bool {
    path.strip_prefix("/api/agent-access/")
        .and_then(|rest| rest.strip_suffix("/revoke"))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}
pub fn classify(path: &str) -> Class {
    if path.starts_with("/v1/worker/profiled/source/")
        || matches!(
            path,
            "/v1/worker/profiled/material" | "/v1/worker/profiled/artifact"
        )
    {
        return Class::Bulk;
    }
    if matches!(
        path,
        "/v1/worker/profiled/register"
            | "/v1/worker/profiled/accept"
            | "/v1/worker/profiled/prepare"
            | "/v1/worker/profiled/poll"
            | "/v1/worker/profiled/started"
            | "/v1/worker/profiled/stopped"
    ) {
        return Class::Control;
    }
    if path.starts_with("/api/observations/") {
        return Class::ObservationRead;
    }
    if matches!(
        path,
        "/v1/worker/observations/batch" | "/v1/worker/observations/transport"
    ) {
        return Class::Observation;
    }
    if path == "/api/notifications" || path.starts_with("/api/notifications/") {
        return Class::Notification;
    }
    if path == "/v1/terminal-relay/poll" {
        return Class::TerminalPoll;
    }
    if path == "/v1/terminal-relay/upload" {
        return Class::TerminalOutput;
    }
    if path.starts_with("/api/v1/terminals/")
        && !path.ends_with("/mutations")
        && !path.ends_with("/access")
    {
        return Class::TerminalRead;
    }
    if path.starts_with("/v1/terminal-relay/")
        || path == "/api/v1/terminals"
        || path.starts_with("/api/v1/terminals/")
    {
        return Class::TerminalInteractive;
    }

    if path == "/v1/execution/sources/status" {
        return Class::Control;
    }
    if matches!(
        path,
        "/v2/worker/artifact-handoff" | "/api/execution/test-bulk" | "/api/source-bases/provision"
    ) || path.starts_with("/v1/execution/sources/")
    {
        Class::Bulk
    } else if matches!(
        path,
        "/healthz"
            | "/readyz"
            | "/v1/worker/heartbeat"
            | "/v1/worker/probe"
            | "/v2/worker/poll"
            | "/v2/worker/decline"
            | "/v2/worker/renew"
            | "/v2/worker/observe"
            | "/v2/worker/directive-ack"
            | "/v2/worker/result"
            | "/v2/worker/reconcile"
            | "/v1/worker/profiled/reconcile"
            | "/v2/worker/agent-session"
            | "/api/execution/replace-incarnation"
            | "/api/execution/test-control"
    ) || execution_cancel_path(path)
        || agent_revoke_path(path)
        || crate::playbooks::api::cancellation_path(path)
    {
        Class::Control
    } else {
        Class::General
    }
}
fn busy(path: &str, message: &str) -> Response {
    let mut response = if path.starts_with("/v1/agent/") {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                ("X-AQ-Agent-API-Version", "1"),
                ("Cache-Control", "no-store"),
            ],
            Json(serde_json::json!({
                "api_version": 1, "request_id": uuid::Uuid::new_v4(),
                "operation_id": null, "code": "overloaded", "message": message,
                "effect": "unknown", "retryable": true, "recovery": null
            })),
        )
            .into_response()
    } else if path.starts_with("/v2/worker/") {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProtocolError {
                code: ErrorCode::Overloaded,
                message: message.into(),
                retryable: true,
                operation_id: None,
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(aq_protocol::ApiError {
                error: "overloaded".into(),
                message: message.into(),
            }),
        )
            .into_response()
    };
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}
pub async fn gate(
    State(admission): State<Arc<Admission>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let (slots, timeout) = match classify(&path) {
        Class::Control => (&admission.control, Duration::from_secs(5)),
        Class::General => (&admission.general, Duration::from_secs(15)),
        Class::Bulk => (&admission.bulk, Duration::from_secs(30)),
        Class::Notification => (&admission.notification, Duration::from_secs(10)),
        Class::Observation => (&admission.observation, Duration::from_secs(8)),
        Class::ObservationRead => (&admission.observation_read, Duration::from_secs(8)),
        Class::TerminalPoll => (&admission.terminal_poll, Duration::from_secs(23)),
        Class::TerminalOutput => (&admission.terminal_output, Duration::from_secs(5)),
        Class::TerminalInteractive => (&admission.terminal_interactive, Duration::from_secs(5)),
        Class::TerminalRead => (&admission.terminal_read, Duration::from_secs(5)),
    };
    let Ok(_permit) = slots.try_acquire() else {
        return busy(
            &path,
            "request capacity exhausted; retry the same operation",
        );
    };
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => busy(
            &path,
            "request deadline exceeded; reconcile or retry the same operation",
        ),
    }
}
