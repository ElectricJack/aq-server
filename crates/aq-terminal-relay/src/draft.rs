//! Human draft lifecycle and portable wire reexports, no installed routes.
use aq_session_protocol::*;
pub use aq_terminal_relay_protocol::{
    CAPABILITY, Capture, Effect, Envelope, Fence, FenceAck, Peer, PeerUpload, RelayPoll,
    ResizeReceipt, UploadAck, VERSION, WireDocument, decode, negotiate, negotiate_epoch,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenewControl {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub expected_controller_generation: u64,
    pub lease_id: Uuid,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseControl {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub expected_controller_generation: u64,
    pub lease_id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlStatus {
    pub terminal: TerminalIdentity,
    pub session_revision: u64,
    pub generation: u64,
    pub lease: Option<ControllerLease>,
    pub fence_acknowledged: bool,
    pub input_paused: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HumanCapture {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub uncertain_operation_id: Uuid,
    pub lease_id: Uuid,
    pub expected_controller_generation: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HumanLifecycle {
    Renew(RenewControl),
    Release(ReleaseControl),
    Capture(HumanCapture),
    OpenEpoch(crate::epoch::Open),
    Status(ControlStatus),
}

pub(crate) fn valid_control_fields(
    operation_id: Uuid,
    lease_id: Uuid,
    session_revision: u64,
    controller_generation: u64,
) -> bool {
    !operation_id.is_nil()
        && !lease_id.is_nil()
        && session_revision > 0
        && controller_generation > 0
}

impl ControlStatus {
    fn validate_lease(&self) -> std::result::Result<(), ContractError> {
        if let Some(lease) = &self.lease {
            lease.validate()?;
            if lease.terminal != self.terminal || lease.generation != self.generation {
                return Err(ContractError::Stale);
            }
        }
        Ok(())
    }
}

impl HumanLifecycle {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        let good = match self {
            Self::Renew(v) => {
                v.terminal.validate()?;
                valid_control_fields(
                    v.operation_id,
                    v.lease_id,
                    v.expected_session_revision,
                    v.expected_controller_generation,
                ) && v.expires_at_ms > 0
            }
            Self::Release(v) => {
                v.terminal.validate()?;
                valid_control_fields(
                    v.operation_id,
                    v.lease_id,
                    v.expected_session_revision,
                    v.expected_controller_generation,
                )
            }
            Self::Capture(v) => {
                v.terminal.validate()?;
                valid_control_fields(
                    v.operation_id,
                    v.lease_id,
                    v.expected_session_revision,
                    v.expected_controller_generation,
                ) && !v.uncertain_operation_id.is_nil()
                    && v.expires_at_ms > 0
            }
            Self::OpenEpoch(v) => {
                v.validate()
                    .map_err(|_| ContractError::Invalid("open epoch"))?;
                true
            }
            Self::Status(v) => {
                v.terminal.validate()?;
                v.validate_lease()?;
                v.session_revision > 0
            }
        };
        if good {
            Ok(())
        } else {
            Err(ContractError::Invalid("draft human fields"))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "contract",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DraftDocument {
    Downstream(Envelope),
    Upstream(PeerUpload),
    Human(HumanLifecycle),
    EpochInput(crate::epoch::Request),
    Poll(RelayPoll),
    Register(Register),
    UploadAck(UploadAck),
}

impl DraftDocument {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        match self {
            Self::Downstream(v) => v.validate(),
            Self::Upstream(v) => v.validate(),
            Self::Human(v) => v.validate(),
            Self::Poll(v) => v.validate(),
            Self::Register(v) => v.validate(),
            Self::UploadAck(v) => v.validate(),
            Self::EpochInput(v) => v
                .validate()
                .map_err(|_| ContractError::Invalid("epoch input request")),
        }
    }
}

pub fn decode_document(bytes: &[u8]) -> std::result::Result<DraftDocument, ContractError> {
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(ContractError::Invalid("draft document bytes"));
    }
    let v: DraftDocument = serde_json::from_slice(bytes)
        .map_err(|_| ContractError::Invalid("relay JSON encoding or shape"))?;
    v.validate()?;
    Ok(v)
}

pub use aq_terminal_relay_protocol::{InputMode, Register};
