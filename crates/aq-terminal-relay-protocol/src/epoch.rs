//! Separately negotiated long-lived INPUT mode. Finite v1 mutation semantics are
//! unchanged. Other mutation classes still retain finite durable ID quotas.
use crate::{Error, Result};
use aq_session_protocol::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub const CAPABILITY: &str = "aq.terminal-input-epoch";
pub const VERSION: u32 = 1;
pub const RECEIPT_WINDOW: usize = 64;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Permit {
    pub terminal: TerminalIdentity,
    pub epoch_id: Uuid,
    pub lease_id: Uuid,
    pub controller_generation: u64,
    pub expires_at_ms: u64,
    pub derivation_version: u32,
}
impl Permit {
    pub fn validate(&self) -> Result<()> {
        self.terminal.validate().map_err(|_| Error::Invalid)?;
        if self.epoch_id.is_nil() || self.lease_id.is_nil() {
            return Err(Error::Invalid);
        }
        if [self.controller_generation, self.expires_at_ms].contains(&0)
            || self.derivation_version != VERSION
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub permit: Permit,
    pub sequence: u64,
    pub input: InputCommand,
}
impl Command {
    pub fn validate(&self) -> Result<()> {
        self.permit.validate()?;
        self.input.validate().map_err(|_| Error::Invalid)?;
        let lease = &self.input.lease;
        let permit = &self.permit;
        if (&lease.terminal, lease.lease_id, lease.generation)
            != (
                &permit.terminal,
                permit.lease_id,
                permit.controller_generation,
            )
        {
            return Err(Error::Stale);
        }
        if self.input.deadline_ms > permit.expires_at_ms
            || self.input.operation_id != operation_id(permit, self.sequence)?
        {
            return Err(Error::Stale);
        }
        Ok(())
    }
}
/// UUIDv8: SHA256(domain || SHA256(canonical TerminalIdentity) || epoch UUID bytes
/// || lease UUID bytes || controller-generation u64BE || input-sequence u64BE),
/// first16 bytes, version8/variant RFC bits replaced. Collision resistance is a
/// cryptographic assumption, not an assertion of mathematical injectivity.
pub fn operation_id(permit: &Permit, sequence: u64) -> Result<Uuid> {
    permit.validate()?;
    if sequence == 0 {
        return Err(Error::Invalid);
    }
    let terminal = canonical_digest(&permit.terminal).map_err(|_| Error::Invalid)?;
    let mut bytes = b"aq.terminal-input-epoch/1\0".to_vec();
    for chunk in terminal.as_str().as_bytes().chunks_exact(2) {
        bytes.push(
            u8::from_str_radix(std::str::from_utf8(chunk).map_err(|_| Error::Invalid)?, 16)
                .map_err(|_| Error::Invalid)?,
        );
    }
    bytes.extend_from_slice(permit.epoch_id.as_bytes());
    bytes.extend_from_slice(permit.lease_id.as_bytes());
    bytes.extend_from_slice(&permit.controller_generation.to_be_bytes());
    bytes.extend_from_slice(&sequence.to_be_bytes());
    let hash = Digest::of(&bytes);
    let mut uuid = [0; 16];
    for (i, chunk) in hash.as_str().as_bytes()[..32].chunks_exact(2).enumerate() {
        uuid[i] = u8::from_str_radix(std::str::from_utf8(chunk).map_err(|_| Error::Invalid)?, 16)
            .map_err(|_| Error::Invalid)?;
    }
    uuid[6] = (uuid[6] & 0x0f) | 0x80;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(uuid))
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub epoch_id: Uuid,
    pub sequence: u64,
    pub receipt: InputReceipt,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Retirement {
    pub permit: Permit,
    pub retired_through: u64,
}
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochCommand")
            .field("permit", &self.permit)
            .field("sequence", &self.sequence)
            .field("operation_id", &self.input.operation_id)
            .finish_non_exhaustive()
    }
}
