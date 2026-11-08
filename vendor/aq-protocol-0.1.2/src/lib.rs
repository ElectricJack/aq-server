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

/// How a stopped host's stop is proven when its guardian could not prove it itself (swarm
/// phase 1, D2 ruling). Absent (or `guardian`) means the guardian's own kernel evidence. The
/// other kinds carry only the outside facts that prove every process of the lost guardian is
/// dead; a stopped observation proven by one carries no guardian evidence (no ECHILD flag,
/// reap count or root status).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StopProof {
    /// The guardian's own kernel evidence (the default when absent).
    Guardian,
    /// The machine rebooted after the launch: the guardian's boot (`old_boot_id`) is not the
    /// boot that recorded the proof (`new_boot_id`).
    BootChanged {
        old_boot_id: Uuid,
        new_boot_id: Uuid,
    },
    /// The service's systemd unit was torn down before this invocation: when invocation
    /// `invocation_id` of `unit` started, its cgroup held only that invocation's processes,
    /// and the guardian started before it on the same boot.
    CgroupTeardown { invocation_id: String, unit: String },
}
impl StopProof {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Guardian => "guardian",
            Self::BootChanged { .. } => "boot_changed",
            Self::CgroupTeardown { .. } => "cgroup_teardown",
        }
    }
    pub fn is_guardian(&self) -> bool {
        matches!(self, Self::Guardian)
    }
    /// The facts' shape: a reboot names the guardian's boot and a different one; a teardown
    /// names a systemd invocation id (32 lowercase hex) and a `.service` unit.
    pub fn validate(&self, guardian_boot_id: &str) -> Result<(), &'static str> {
        match self {
            Self::Guardian => Ok(()),
            Self::BootChanged {
                old_boot_id,
                new_boot_id,
            } => {
                let named = Uuid::parse_str(guardian_boot_id).ok() == Some(*old_boot_id);
                (named && old_boot_id != new_boot_id && !new_boot_id.is_nil())
                    .then_some(())
                    .ok_or("boot_changed proof must name the guardian's boot and another")
            }
            Self::CgroupTeardown {
                invocation_id,
                unit,
            } => {
                let id = invocation_id.len() == 32
                    && invocation_id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                let unit_ok = unit.len() <= 256
                    && unit.ends_with(".service")
                    && unit.len() > ".service".len()
                    && !unit.contains('/')
                    && !unit.chars().any(char::is_control);
                (id && unit_ok)
                    .then_some(())
                    .ok_or("cgroup_teardown proof needs a systemd invocation id and unit")
            }
        }
    }
}

/// POST /v1/worker/lost-attempts (swarm phase 1, D2 ruling C). A restarted node asks the
/// server to release the hold on one of its attempts whose host was lost (its guardian cannot
/// prove the stop) and to retry the task. `proof` is the outside fact that every process of the
/// lost host is dead: `boot_changed` or `cgroup_teardown`, never `guardian`. The attempt's
/// unsaved work is not salvaged; its preserved sandbox stays for the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LostAttemptRelease {
    pub schema_version: u32,
    pub attempt_id: Uuid,
    pub proof: StopProof,
}
/// Why a lost attempt failed: its host was lost with the machine (a reboot, not charged to the
/// lineage budgets) or with the service (a crash, charged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LostReason {
    LostAfterReboot,
    LostAfterCrash,
}
impl LostReason {
    /// `boot_changed` is a reboot; `cgroup_teardown` a crash; a guardian proof is no loss.
    pub fn of(proof: &StopProof) -> Option<Self> {
        match proof {
            StopProof::Guardian => None,
            StopProof::BootChanged { .. } => Some(Self::LostAfterReboot),
            StopProof::CgroupTeardown { .. } => Some(Self::LostAfterCrash),
        }
    }
}
/// The answer to `LostAttemptRelease`: the attempt failed for `reason`; `released` when its
/// task claim was released for a retry, else `held` names what still blocks the retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LostAttemptReleased {
    pub schema_version: u32,
    pub attempt_id: Uuid,
    pub reason: LostReason,
    pub charged: bool,
    pub released: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_proof_kinds_round_trip_and_validate() {
        let old = Uuid::from_u128(1);
        let boot = StopProof::BootChanged {
            old_boot_id: old,
            new_boot_id: Uuid::from_u128(2),
        };
        let json = serde_json::to_value(&boot).unwrap();
        assert_eq!(json["proof_kind"], "boot_changed");
        assert_eq!(serde_json::from_value::<StopProof>(json).unwrap(), boot);
        assert_eq!(boot.validate(&old.to_string()), Ok(()));
        assert!(boot.validate(&Uuid::from_u128(3).to_string()).is_err());
        let same = StopProof::BootChanged {
            old_boot_id: old,
            new_boot_id: old,
        };
        assert!(same.validate(&old.to_string()).is_err());
        let teardown = StopProof::CgroupTeardown {
            invocation_id: "0123456789abcdef0123456789abcdef".into(),
            unit: "aq-node@ms1.service".into(),
        };
        assert_eq!(teardown.kind(), "cgroup_teardown");
        assert_eq!(teardown.validate("any"), Ok(()));
        let bad = StopProof::CgroupTeardown {
            invocation_id: "x".into(),
            unit: "../aq-node@ms1.service".into(),
        };
        assert!(bad.validate("any").is_err());
        let guardian: StopProof = serde_json::from_str(r#"{"proof_kind":"guardian"}"#).unwrap();
        assert!(guardian.is_guardian());
        assert!(serde_json::from_str::<StopProof>(r#"{"proof_kind":"boot_changed"}"#).is_err());
        let release = LostAttemptRelease {
            schema_version: 1,
            attempt_id: Uuid::from_u128(5),
            proof: teardown.clone(),
        };
        let json = serde_json::to_value(&release).unwrap();
        assert_eq!(json["proof"]["proof_kind"], "cgroup_teardown");
        assert_eq!(
            serde_json::from_value::<LostAttemptRelease>(json).unwrap(),
            release
        );
        assert_eq!(LostReason::of(&boot), Some(LostReason::LostAfterReboot));
        assert_eq!(LostReason::of(&teardown), Some(LostReason::LostAfterCrash));
        assert_eq!(LostReason::of(&StopProof::Guardian), None);
        assert_eq!(
            serde_json::to_value(LostReason::LostAfterCrash).unwrap(),
            "lost_after_crash"
        );
    }

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
