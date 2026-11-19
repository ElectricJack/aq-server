//! The trusted description of one observation source: what the server's
//! ledger enrolls for an attempt and checks every event against.
use crate::{Error, Result, reconcile::Policy, wire::*};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Constructed by the daemon's trusted catalog, never deserialized as a
/// worker request. Account/namespace policy is owner data.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSource {
    pub binding: Binding,
    pub source_id: Uuid,
    pub generation: u64,
    pub native_session_key: Text,
    pub account_claim: Text,
    pub policy: Policy,
    /// How this source's usage is costed, from the selected credential kind.
    /// Absent on sources registered before H2; the digest of those is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<EstimateBasis>,
}
impl OwnerSource {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        self.native_session_key.validate()?;
        self.account_claim.validate()?;
        self.policy.validate()?;
        if self.source_id.is_nil()
            || self.generation == 0
            || i64::try_from(self.generation).is_err()
            || serde_json::to_vec(self).map_err(|_| Error::Invalid)?.len() > 32768
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::of(
            serde_json::to_vec(self)
                .map_err(|_| Error::Invalid)?
                .as_slice(),
        ))
    }
    /// The event names this source, its session and one of its parsers.
    pub fn matches(&self, event: &ObservationRecord) -> bool {
        let p = &event.body.provenance;
        self.binding == event.body.binding
            && self.source_id == p.source_id
            && self.generation == p.source_generation
            && self.native_session_key == p.native_session_key
            && self.policy.parsers.iter().any(|x| x.parser == p.parser)
    }
}
