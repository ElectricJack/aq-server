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
        if self.preparation_operation_id.is_some_and(|id| id.is_nil())
            || self.cancellation_id.is_some_and(|id| id.is_nil())
            || self.execution_deadline_ms == Some(0)
            || self.owner.as_ref().is_some_and(|owner| {
                owner.guardian_id.is_nil()
                    || owner.launch_id.is_nil()
                    || owner.reservation_id.is_nil()
            })
        {
            return Err("hot identity or deadline");
        }
        if let Some(grant) = &self.grant {
            grant.validate().map_err(|_| "hot grant")?;
            if &grant.binding != binding
                || self.owner.is_none()
                || self
                    .execution_deadline_ms
                    .is_none_or(|deadline| grant.expires_at_ms > deadline)
            {
                return Err("hot grant scope or ceiling");
            }
        }
        if self.status == Status::Granted
            && (self.grant.is_none()
                || self.owner.is_none()
                || self.revoked
                || self.cancellation_id.is_some()
                || self.stop.known_stopped())
        {
            return Err("granted hot state needs actual grant and owner");
        }
        if self.status == Status::NeverGranted
            && (self.grant.is_some() || !matches!(self.stop, Stop::Unknown))
        {
            return Err("never-granted is not physical stop proof");
        }
        if (self.status == Status::Offered
            && (self.owner.is_some()
                || self.grant.is_some()
                || !matches!(self.stop, Stop::Unknown)))
            || (self.status == Status::Preparing
                && (self.owner.is_none()
                    || self.grant.is_some()
                    || !matches!(self.stop, Stop::Unknown)))
            || (matches!(self.status, Status::Finished | Status::Declined)
                && !self.stop.known_stopped())
            || (self.status == Status::Declined && self.grant.is_some())
            || (self.stop.known_stopped()
                && (self.owner.is_none()
                    || matches!(
                        self.status,
                        Status::Offered | Status::Preparing | Status::Granted
                    )))
        {
            return Err("inconsistent hot status and host owner");
        }
        for reference in [
            match &self.stop {
                Stop::NotStarted { receipt } | Stop::Stopped { receipt } => Some(receipt),
                _ => None,
            },
            match &self.retention {
                Retention::Retained { receipt } => Some(receipt),
                _ => None,
            },
            match &self.effects {
                Effects::Quiescent { receipt } => Some(receipt),
                _ => None,
            },
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
