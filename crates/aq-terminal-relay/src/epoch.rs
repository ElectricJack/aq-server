//! Explicit human epoch-input wrapper; worker wire types live independently.
use crate::{Error, Result};
use aq_session_api::InputRequest;
use aq_session_protocol::*;
pub use aq_terminal_relay_protocol::epoch::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub capability: Text,
    pub epoch_id: Uuid,
    pub sequence: u64,
    pub input: InputRequest,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        self.input.terminal.validate().map_err(|_| Error::Invalid)?;
        let input = &self.input;
        if !self.has_epoch_header() || !valid_control_input(input) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    fn has_epoch_header(&self) -> bool {
        self.version == VERSION
            && self.capability.as_str() == CAPABILITY
            && !self.epoch_id.is_nil()
            && self.sequence > 0
    }
}

fn valid_control_input(input: &InputRequest) -> bool {
    crate::draft::valid_control_fields(
        input.operation_id,
        input.lease_id,
        input.expected_session_revision,
        input.expected_controller_generation,
    ) && input.expires_at_ms > 0
        && (1..=MAX_INPUT_BYTES).contains(&input.bytes.len())
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochRequest")
            .field("epoch_id", &self.epoch_id)
            .field("sequence", &self.sequence)
            .field("operation_id", &self.input.operation_id)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub expected_controller_generation: u64,
    pub lease_id: Uuid,
    /// Short intent delivery deadline, not an extension of epoch authority.
    /// The resulting epoch is bounded by the already-current controller lease.
    pub expires_at_ms: u64,
}
impl Open {
    pub fn validate(&self) -> Result<()> {
        self.terminal.validate().map_err(|_| Error::Invalid)?;
        if !crate::draft::valid_control_fields(
            self.operation_id,
            self.lease_id,
            self.expected_session_revision,
            self.expected_controller_generation,
        ) || self.expires_at_ms == 0
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
