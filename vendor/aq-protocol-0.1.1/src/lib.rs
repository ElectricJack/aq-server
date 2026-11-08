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

pub const MAX_MACHINE_SLOTS: u32 = 8;
pub const MACHINE_SCHEMA_VERSION: u32 = 1;

/// The worker-machine/v1 `machine.key` rule: 1-63 characters of [a-z0-9-], not starting with '-'.
pub fn valid_machine_key(key: &str) -> bool {
    (1..=63).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !key.starts_with('-')
}

/// POST /v1/worker/slots, under the bearer of the machine's slot-1 node.
/// Claims `machine_key` for the caller (slot 1) and mints the missing slots 2..=slots.
/// `slots` is the total wanted, 1..=8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotsRequest {
    pub schema_version: u32,
    pub machine_key: String,
    pub slots: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotsResponse {
    pub schema_version: u32,
    pub machine_key: String,
    pub slots: Vec<SlotEnrollment>,
}

/// `slot` is 2..=8 (slot 1 is the caller). `credential` is `Some` only in the
/// response that minted it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotEnrollment {
    pub slot: u32,
    pub node_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl std::fmt::Debug for SlotEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotEnrollment")
            .field("slot", &self.slot)
            .field("node_id", &self.node_id)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Draining,
    Stopped,
}

/// POST /v1/worker/control, under any slot node's bearer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub schema_version: u32,
}

/// `generation` is >= 1. `handoff` lists attempts on this machine to hand off now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResponse {
    pub schema_version: u32,
    pub machine_key: String,
    pub desired: DesiredState,
    pub generation: u64,
    pub handoff: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualification: Option<QualificationPin>,
}

/// `digest` is 64 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPin {
    pub id: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationPin {
    pub renderer: ArtifactPin,
    pub effective_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffReason {
    OwnerStop,
    ServiceStop,
    Restart,
}

/// POST /v1/worker/handoffs (node announces a handoff it is performing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffNotice {
    pub schema_version: u32,
    pub attempt_id: Uuid,
    pub reason: HandoffReason,
}

/// POST /v1/worker/qualifications. `result` is the Claude stream-json `result` event, verbatim.
/// The response body is `QualificationPin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredCall {
    pub call: String,
    pub prompt_utf8_bytes: u64,
    pub prompt_sha256: String,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementSubmission {
    pub schema_version: u32,
    pub calls: Vec<MeasuredCall>,
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

    #[test]
    fn machine_fixture_round_trips() {
        fn rt<T: Serialize + serde::de::DeserializeOwned>(v: &serde_json::Value) {
            let t: T = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(&serde_json::to_value(&t).unwrap(), v);
        }
        let f: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/machine-v1.json")).unwrap();
        let o = f.as_object().unwrap();
        assert_eq!(o.len(), 7);
        rt::<SlotsRequest>(&o["slots_request"]);
        rt::<SlotsResponse>(&o["slots_response"]);
        rt::<ControlRequest>(&o["control_request"]);
        rt::<ControlResponse>(&o["control_response"]);
        rt::<HandoffNotice>(&o["handoff_notice"]);
        rt::<MeasurementSubmission>(&o["measurement_submission"]);
        rt::<QualificationPin>(&o["qualification_pin"]);
    }

    #[test]
    fn slot_enrollment_debug_redacts_credential() {
        let s = format!(
            "{:?}",
            SlotEnrollment {
                slot: 2,
                node_id: Uuid::nil(),
                credential: Some("aqn_secret".into())
            }
        );
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("aqn_secret"));
    }

    #[test]
    fn desired_state_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&DesiredState::Draining).unwrap(),
            "\"draining\""
        );
        assert_eq!(
            serde_json::to_string(&HandoffReason::ServiceStop).unwrap(),
            "\"service_stop\""
        );
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(serde_json::from_str::<ControlRequest>(r#"{"schema_version":1,"x":1}"#).is_err());
    }

    #[test]
    fn machine_key_rule_matches_worker_machine_v1() {
        for ok in ["desktop", "a", "desk-1", "0a", &"a".repeat(63)] {
            assert!(valid_machine_key(ok), "{ok}");
        }
        for bad in ["", "-a", "Desk", "a_b", "a b", &"a".repeat(64)] {
            assert!(!valid_machine_key(bad), "{bad}");
        }
    }
}
