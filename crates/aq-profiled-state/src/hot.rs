//! Compact mutable authority. No manifest, source artifact or credential bytes.
//! Receipt references are projections; only owning producers verify release.
use crate::{Effects, Retention, Status, Stop};
use aq_session_protocol::{LaunchBinding, ProfiledGrant, Validate};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_HOT_BYTES: usize = 16_384;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchOwner {
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotState {
    pub schema_version: u32,
    pub status: Status,
    pub grant: Option<ProfiledGrant>,
    pub execution_deadline_ms: Option<u64>,
    pub owner: Option<LaunchOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation_operation_id: Option<Uuid>,
    pub cancellation_id: Option<Uuid>,
    pub revoked: bool,
    pub stop: Stop,
    pub retention: Retention,
    pub effects: Effects,
}
impl HotState {
    /// Initial adoption never invents a host owner for an old active grant.
    /// Such rows remain on the qualified compatibility path until a producer
    /// can verify and migrate their actual durable launch identity.
    pub fn from_compat(value: &crate::ProfiledState) -> Result<Self, &'static str> {
        value.validate()?;
        let result = Self {
            schema_version: 1,
            status: value.status,
            grant: value.grant.clone(),
            execution_deadline_ms: value.execution_deadline_ms,
            owner: None,
            preparation_operation_id: None,
            cancellation_id: value.cancellation_id,
            revoked: value.revoked,
            stop: value.stop.clone(),
            retention: value.retention.clone(),
            effects: value.effects.clone(),
        };
        result.validate_against(&value.offer.binding)?;
        Ok(result)
    }
    pub fn validate_against(&self, binding: &LaunchBinding) -> Result<(), &'static str> {
        binding.validate().map_err(|_| "hot launch binding")?;
        if self.schema_version != 1
            || serde_json::to_vec(self).map_err(|_| "hot encoding")?.len() > MAX_HOT_BYTES
        {
            return Err("hot state version or byte bound");
        }
        self.validate_identity()?;
        self.validate_grant(binding)?;
        self.validate_status()?;
        self.validate_receipts()
    }
    fn validate_identity(&self) -> Result<(), &'static str> {
        let nil_operation = [self.preparation_operation_id, self.cancellation_id]
            .into_iter()
            .flatten()
            .any(|id| id.is_nil());
        let invalid_owner = self.owner.as_ref().is_some_and(LaunchOwner::has_nil_id);
        let invalid_identity = nil_operation || invalid_owner;
        if invalid_identity || self.execution_deadline_ms == Some(0) {
            return Err("hot identity or deadline");
        }
        Ok(())
    }
    fn validate_grant(&self, binding: &LaunchBinding) -> Result<(), &'static str> {
        let Some(grant) = &self.grant else {
            return Ok(());
        };
        grant.validate().map_err(|_| "hot grant")?;
        let has_ceiling = self
            .execution_deadline_ms
            .is_some_and(|deadline| grant.expires_at_ms <= deadline);
        let bound_to_owner = self.owner.is_some() && &grant.binding == binding;
        if !bound_to_owner || !has_ceiling {
            return Err("hot grant scope or ceiling");
        }
        Ok(())
    }
    fn validate_status(&self) -> Result<(), &'static str> {
        match self.status {
            Status::Granted if !self.has_live_owner_grant() => {
                Err("granted hot state needs actual grant and owner")
            }
            Status::NeverGranted if !self.has_no_grant_or_stop() => {
                Err("never-granted is not physical stop proof")
            }
            _ if !self.owner_matches_status() => Err("inconsistent hot status and host owner"),
            _ => Ok(()),
        }
    }
    fn has_live_owner_grant(&self) -> bool {
        let cancelled = self.revoked || self.cancellation_id.is_some();
        self.owner.is_some() && self.grant.is_some() && !cancelled && !self.stop.known_stopped()
    }
    fn has_no_grant_or_stop(&self) -> bool {
        self.grant.is_none() && matches!(self.stop, Stop::Unknown)
    }
    fn owner_matches_status(&self) -> bool {
        let has_owner = self.owner.is_some();
        let stopped = self.stop.known_stopped();
        match self.status {
            Status::Offered => !has_owner && self.has_no_grant_or_stop(),
            Status::Preparing => has_owner && self.has_no_grant_or_stop(),
            Status::Finished => has_owner && stopped,
            Status::Declined => has_owner && stopped && self.grant.is_none(),
            _ => !stopped || has_owner,
        }
    }
    fn validate_receipts(&self) -> Result<(), &'static str> {
        for reference in [
            self.stop.receipt(),
            self.retention.receipt(),
            self.effects.receipt(),
        ]
        .into_iter()
        .flatten()
        {
            reference
                .id
                .validate()
                .map_err(|_| "hot receipt identity")?;
            reference
                .version
                .validate()
                .map_err(|_| "hot receipt version")?;
        }
        if matches!(self.retention, Retention::NotRequired)
            && !matches!(self.stop, Stop::NotStarted { .. })
        {
            return Err("retention not required only with verified nonstart");
        }
        if matches!(self.effects, Effects::Pending {count} if count > 4096) {
            return Err("hot pending effect bound");
        }
        Ok(())
    }
    /// Completeness only, not physical-stop or receipt-owner proof.
    pub fn release_projection(&self) -> bool {
        self.stop.known_stopped()
            && matches!(
                self.retention,
                Retention::NotRequired | Retention::Retained { .. }
            )
            && matches!(self.effects, Effects::Quiescent { .. })
    }
}

impl LaunchOwner {
    fn has_nil_id(&self) -> bool {
        [self.guardian_id, self.launch_id, self.reservation_id]
            .into_iter()
            .any(|id| id.is_nil())
    }
}
