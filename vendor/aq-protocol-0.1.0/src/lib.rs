//! Wire types shared by the Agent Q server and worker.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;

/// Inclusive (min, max) worker protocol versions the server accepts.
pub const SUPPORTED_WORKER_PROTOCOLS: (u32, u32) = (1, 1);

pub const UNSUPPORTED_PROTOCOL: &str = "unsupported_protocol";
pub const INVALID_CREDENTIAL: &str = "invalid_credential";
pub const CREDENTIAL_REVOKED: &str = "credential_revoked";
pub const ENROLLMENT_INVALID: &str = "enrollment_invalid";
pub const FORBIDDEN: &str = "forbidden";
pub const UNAUTHENTICATED: &str = "unauthenticated";
pub const CSRF_REJECTED: &str = "csrf_rejected";
pub const IDENTITY_UNAVAILABLE: &str = "identity_unavailable";
pub const NOT_FOUND: &str = "not_found";
pub const BAD_REQUEST: &str = "bad_request";
pub const CONFLICT: &str = "conflict";
pub const INTERNAL: &str = "internal";
pub const UPDATE_IN_PROGRESS: &str = "update_in_progress";
pub const MAINTENANCE: &str = "maintenance";
pub const UPDATES_UNAVAILABLE: &str = "updates_unavailable";
pub const INVALID_VERSION: &str = "invalid_version";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub server_version: String,
    pub source_revision: String,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub state_format_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub code: String,
    pub name: String,
    pub worker_version: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub node_id: Uuid,
    pub credential: String,
    pub heartbeat_interval_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub worker_version: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub server_version: String,
    pub heartbeat_interval_secs: u64,
}

/// `nonce` is at most 64 characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResponse {
    pub nonce: String,
    pub server_time: DateTime<Utc>,
}

/// Body of a 426 response. `error` is always `UNSUPPORTED_PROTOCOL`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityError {
    pub error: String,
    pub message: String,
    pub supported_min: u32,
    pub supported_max: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_round_trips() {
        let e = ApiError {
            error: FORBIDDEN.into(),
            message: "no".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, r#"{"error":"forbidden","message":"no"}"#);
        assert_eq!(serde_json::from_str::<ApiError>(&json).unwrap(), e);
    }
}
