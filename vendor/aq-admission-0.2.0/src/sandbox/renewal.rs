//! Phase execution handoff and renewal without a legacy initial-grant receipt.
use super::*;
use crate::{Digest, SessionIdentity, control::DeliveryStamp};
use aq_session_protocol::{ProfiledGrant, Validate};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhaseProfile {
    PhaseV2,
    PhaseRefreshV1,
}
/// Immutable origin retained by B2, not authority established by deserializing it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhaseExecutionAuthority {
    pub profile: PhaseProfile,
    pub receipt_digest: Digest,
    pub grant: ProfiledGrant,
    pub issued_at_ms: u64,
    pub hard_deadline_ms: u64,
}
impl PhaseExecutionAuthority {
    pub fn validate(&self) -> Result<()> {
        self.grant
            .validate()
            .map_err(|_| ContractError::InvalidShape)?;
        positive(self.issued_at_ms)?;
        positive(self.hard_deadline_ms)?;
        check(
            self.issued_at_ms < self.grant.expires_at_ms
                && self.grant.expires_at_ms <= self.hard_deadline_ms,
            ContractError::DeadlineMismatch,
        )
    }
    /// Call only after validating the full receipt against preparation history.
    /// This projection does not authenticate the receipt or mint a credential.
    pub fn from_phase_v2(receipt: &crate::preparation_v2::ExecutionReceipt) -> Result<Self> {
        let value = Self {
            profile: PhaseProfile::PhaseV2,
            receipt_digest: receipt.digest().map_err(|_| ContractError::InvalidShape)?,
            grant: receipt.grant.clone(),
            issued_at_ms: receipt.issued_at_ms,
            hard_deadline_ms: receipt.execution_deadline_ms,
        };
        value.validate()?;
        Ok(value)
    }
    /// Same boundary for preparation-refresh-v1 execution receipts.
    pub fn from_phase_refresh(
        receipt: &crate::preparation_refresh_v1::ExecutionReceipt,
    ) -> Result<Self> {
        let value = Self {
            profile: PhaseProfile::PhaseRefreshV1,
            receipt_digest: receipt.digest().map_err(|_| ContractError::InvalidShape)?,
            grant: receipt.grant.clone(),
            issued_at_ms: receipt.issued_at_ms,
            hard_deadline_ms: receipt.execution_deadline_ms,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate_binding(&self, permit: &DelegatedPermit) -> Result<()> {
        self.validate()?;
        permit.validate()?;
        check(
            permit.scope == PermitScope::Execution,
            ContractError::ScopeDenied,
        )?;
        // Native grants identify their stable generation by assignment_generation;
        // renewal revisions are separate and never change this binding.
        check(
            matches!(self.grant.binding.session, SessionIdentity::Remote {node_id, ..}
            if node_id == permit.binding.node.node_id)
                && self.grant.binding.attempt_id == permit.binding.attempt_id
                && self.grant.binding.assignment_generation == permit.binding.grant_generation,
            ContractError::BindingMismatch,
        )?;
        check(
            permit.hard_deadline_ms == self.hard_deadline_ms
                && permit.issued_at_ms >= self.issued_at_ms,
            ContractError::DeadlineMismatch,
        )
    }
    pub fn validate_handoff(&self, permit: &DelegatedPermit, now_ms: u64) -> Result<()> {
        self.validate_binding(permit)?;
        check(
            matches!(self.grant.binding.session, SessionIdentity::Remote {boot_id, ..} if boot_id == permit.binding.node.boot_id),
            ContractError::BindingMismatch,
        )?;
        permit.validate_for(
            &PermitContext {
                binding: permit.binding.clone(),
                phase: GrantPhase::Granted,
                grant_expires_at_ms: self.grant.expires_at_ms,
                hard_deadline_ms: self.hard_deadline_ms,
            },
            RunnerOperation::Started,
            now_ms,
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerRenewalRequest {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub authority: PhaseExecutionAuthority,
    pub permit: DelegatedPermit,
    pub revision: u64,
}
impl RunnerRenewalRequest {
    pub fn validate(&self) -> Result<()> {
        // The immutable receipt retains its original boot. After re-adoption B2
        // authenticates the rebound permit against the current incarnation.
        check(self.schema_version == 1, ContractError::InvalidShape)?;
        nonnil(self.operation_id)?;
        positive(self.revision)?;
        self.authority.validate_binding(&self.permit)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerRenewalResponse {
    pub schema_version: u32,
    pub revision: u64,
    pub reply: RenewalReply,
    pub delivery: Option<DeliveryStamp>,
}
impl RunnerRenewalResponse {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 1, ContractError::InvalidShape)?;
        positive(self.revision)?;
        self.reply.validate()?;
        match (&self.reply, &self.delivery) {
            (RenewalReply::AwaitingNodeReadoption { .. }, None) => Ok(()),
            (RenewalReply::Renewed { permit, .. }, Some(stamp)) => stamp
                .validate_for(stamp.nonce, permit.expires_at_ms)
                .map_err(|_| ContractError::DeadlineMismatch),
            _ => Err(ContractError::DeadlineMismatch),
        }
    }
    /// B2 must additionally authenticate the credential, compare the exact origin
    /// and revision to retained server state, and atomically persist the mutation.
    pub fn validate_for(
        &self,
        request: &RunnerRenewalRequest,
        current: &PermitContext,
        nonce: Uuid,
        now_ms: u64,
    ) -> Result<()> {
        request.validate()?;
        self.validate()?;
        request
            .permit
            .validate_for(current, RunnerOperation::Renew, now_ms)?;
        match &self.reply {
            RenewalReply::AwaitingNodeReadoption {
                operation_id,
                binding,
            } => {
                check(
                    *operation_id == request.operation_id && *binding == request.permit.binding,
                    ContractError::BindingMismatch,
                )?;
                check(
                    self.revision == request.revision,
                    ContractError::ReplayConflict,
                )
            }
            RenewalReply::Renewed {
                operation_id,
                permit,
            } => {
                check(
                    *operation_id == request.operation_id
                        && permit.permit_id == request.permit.permit_id
                        && permit.binding == request.permit.binding,
                    ContractError::BindingMismatch,
                )?;
                check(
                    self.revision > request.revision,
                    ContractError::ReplayConflict,
                )?;
                check(
                    permit.hard_deadline_ms == request.permit.hard_deadline_ms
                        && permit.expires_at_ms >= request.permit.expires_at_ms
                        && permit.issued_at_ms >= request.permit.issued_at_ms,
                    ContractError::DeadlineMismatch,
                )?;
                permit.validate_for(current, RunnerOperation::Renew, now_ms)?;
                self.delivery
                    .as_ref()
                    .ok_or(ContractError::InvalidShape)?
                    .validate_for(nonce, permit.expires_at_ms)
                    .map_err(|_| ContractError::BindingMismatch)
            }
        }
    }
    /// Run after validate_for. A replay of an installed revision retains the
    /// earlier local deadline; missing/refused renewal never extends it.
    pub fn client_deadline_ns(
        &self,
        sent_ns: u64,
        received_ns: u64,
        prior_deadline_ns: u64,
        hard_deadline_ns: u64,
        installed_revision: u64,
    ) -> Result<u64> {
        self.validate()?;
        check(
            received_ns < prior_deadline_ns && received_ns < hard_deadline_ns,
            ContractError::Expired,
        )?;
        check(
            self.revision >= installed_revision,
            ContractError::ReplayConflict,
        )?;
        let cap = if self.revision == installed_revision {
            prior_deadline_ns.min(hard_deadline_ns)
        } else {
            hard_deadline_ns
        };
        match &self.delivery {
            Some(stamp) => stamp
                .client_deadline_ns(sent_ns, received_ns, Some(cap))
                .map_err(|_| ContractError::Expired),
            None => {
                check(
                    self.revision == installed_revision,
                    ContractError::ReplayConflict,
                )?;
                Ok(prior_deadline_ns.min(hard_deadline_ns))
            }
        }
    }
}
