//! Acceptance-to-permit issuance, grant upgrade and node enrollment envelopes.
//!
//! The frozen phase payloads (`AcceptRequest`, `AcceptedDelivery`,
//! `PrepareRequest`, `ExecutionDelivery`) are carried unchanged by their own
//! profile. These sibling envelopes correlate to them by digest and exact
//! identity. B2 decides which existing route carries each envelope. Every pair
//! is idempotent by `operation_id`: an exact replay returns identical bytes,
//! and any changed byte is a `replay_conflict`.
use super::*;
use crate::{Digest, LaunchBinding, SessionIdentity};
use aq_session_protocol::Validate;

fn same_bytes<T: Serialize>(a: &T, b: &T) -> bool {
    matches!((serde_json::to_vec(a), serde_json::to_vec(b)), (Ok(x), Ok(y)) if x == y)
}
fn launch_node(binding: &LaunchBinding) -> Result<WorkerIdentity> {
    binding
        .validate()
        .map_err(|_| ContractError::InvalidShape)?;
    match binding.session {
        SessionIdentity::Remote {
            node_id,
            boot_id,
            attempt_id,
            assignment_generation,
            ..
        } if attempt_id == binding.attempt_id
            && assignment_generation == binding.assignment_generation =>
        {
            Ok(WorkerIdentity { node_id, boot_id })
        }
        _ => Err(ContractError::BindingMismatch),
    }
}
fn frozen<T>(value: crate::Result<T>) -> Result<T> {
    value.map_err(|_| ContractError::InvalidShape)
}

/// Projection of a validated frozen `AcceptedReceipt`, retained by B2 as the
/// trusted preparation state. Deserializing it establishes no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreparationAuthority {
    pub profile: PhaseProfile,
    pub accepted_receipt_digest: Digest,
    /// The accepted binding; `assignment_generation` is the reserved next grant generation.
    pub binding: LaunchBinding,
    pub issued_at_ms: u64,
    /// The accepted preparation lease expiry; the preparation permit's hard deadline.
    pub preparation_deadline_ms: u64,
}
impl PreparationAuthority {
    pub fn validate(&self) -> Result<()> {
        node(&launch_node(&self.binding)?)?;
        positive(self.issued_at_ms)?;
        positive(self.preparation_deadline_ms)?;
        check(
            self.issued_at_ms < self.preparation_deadline_ms,
            ContractError::DeadlineMismatch,
        )
    }
    /// Call only after validating the full receipt against its proposal/request.
    pub fn from_phase_v2(receipt: &crate::preparation_v2::AcceptedReceipt) -> Result<Self> {
        let value = Self {
            profile: PhaseProfile::PhaseV2,
            accepted_receipt_digest: frozen(receipt.digest())?,
            binding: receipt.lease.binding.clone(),
            issued_at_ms: receipt.lease.issued_at_ms,
            preparation_deadline_ms: receipt.lease.expires_at_ms,
        };
        value.validate()?;
        Ok(value)
    }
    /// Same boundary for preparation-refresh-v1 accepted receipts.
    pub fn from_phase_refresh(
        receipt: &crate::preparation_refresh_v1::AcceptedReceipt,
    ) -> Result<Self> {
        let value = Self {
            profile: PhaseProfile::PhaseRefreshV1,
            accepted_receipt_digest: frozen(receipt.digest())?,
            binding: receipt.lease.binding.clone(),
            issued_at_ms: receipt.lease.issued_at_ms,
            preparation_deadline_ms: receipt.lease.expires_at_ms,
        };
        value.validate()?;
        Ok(value)
    }
    /// Preparation-scope check for any runner operation before the grant:
    /// exact attempt, reserved generation, node and boot, and the preparation
    /// deadline as both grant expiry and hard deadline.
    pub fn validate_permit(
        &self,
        permit: &DelegatedPermit,
        operation: RunnerOperation,
        now_ms: u64,
    ) -> Result<()> {
        self.validate()?;
        permit.validate()?;
        check(
            permit.scope == PermitScope::Preparation,
            ContractError::ScopeDenied,
        )?;
        check(
            permit.binding.node == launch_node(&self.binding)?
                && permit.binding.attempt_id == self.binding.attempt_id
                && permit.binding.grant_generation == self.binding.assignment_generation,
            ContractError::BindingMismatch,
        )?;
        check(
            permit.issued_at_ms >= self.issued_at_ms,
            ContractError::DeadlineMismatch,
        )?;
        permit.validate_for(
            &PermitContext {
                binding: permit.binding.clone(),
                phase: GrantPhase::Preparing,
                grant_expires_at_ms: self.preparation_deadline_ms,
                hard_deadline_ms: self.preparation_deadline_ms,
            },
            operation,
            now_ms,
        )
    }
}

/// Sent with the frozen accept. Supplies the sandbox identity the frozen
/// `AcceptRequest` lacks; the runner channel is allocated by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermitIssuanceRequest {
    pub schema_version: u32,
    /// Equals the frozen accept's `operation_id`.
    pub operation_id: Uuid,
    pub profile: PhaseProfile,
    pub accept_request_digest: Digest,
    pub attempt_id: Uuid,
    pub grant_generation: u64,
    pub sandbox_id: Uuid,
    pub node: WorkerIdentity,
}
macro_rules! accept_correlation {
    ($name:ident, $module:ident, $profile:ident) => {
        /// Correlates this envelope to the exact frozen accept it accompanies.
        pub fn $name(&self, accept: &crate::$module::AcceptRequest) -> Result<()> {
            self.validate()?;
            let original = &accept.original;
            check(
                self.profile == PhaseProfile::$profile
                    && self.accept_request_digest == frozen(accept.digest())?
                    && self.operation_id == original.operation_id
                    && self.node == original.identity
                    && self.attempt_id == original.acceptance.binding.attempt_id
                    && self.grant_generation == original.acceptance.binding.assignment_generation,
                ContractError::BindingMismatch,
            )
        }
    };
}
impl PermitIssuanceRequest {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 1, ContractError::InvalidShape)?;
        for value in [self.operation_id, self.attempt_id, self.sandbox_id] {
            nonnil(value)?;
        }
        positive(self.grant_generation)?;
        node(&self.node)
    }
    accept_correlation!(validate_accept_v2, preparation_v2, PhaseV2);
    accept_correlation!(
        validate_accept_refresh,
        preparation_refresh_v1,
        PhaseRefreshV1
    );
}
/// Returned with the frozen accepted delivery: the preparation-scoped permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermitIssuanceResponse {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub authority: PreparationAuthority,
    pub permit: DelegatedPermit,
}
impl PermitIssuanceResponse {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 1, ContractError::InvalidShape)?;
        nonnil(self.operation_id)?;
        self.authority.validate()?;
        self.permit.validate()?;
        check(
            self.permit.scope == PermitScope::Preparation,
            ContractError::ScopeDenied,
        )?;
        check(
            self.permit.binding.node == launch_node(&self.authority.binding)?
                && self.permit.binding.attempt_id == self.authority.binding.attempt_id
                && self.permit.binding.grant_generation
                    == self.authority.binding.assignment_generation,
            ContractError::BindingMismatch,
        )?;
        check(
            self.permit.hard_deadline_ms == self.authority.preparation_deadline_ms
                && self.permit.issued_at_ms >= self.authority.issued_at_ms,
            ContractError::DeadlineMismatch,
        )
    }
    /// Runner-side check of an authenticated reply. The issued permit must
    /// authorize `Prepared` now; B2 also validates the frozen receipt history.
    pub fn validate_for(&self, request: &PermitIssuanceRequest, now_ms: u64) -> Result<()> {
        request.validate()?;
        self.validate()?;
        let binding = &self.permit.binding;
        check(
            self.operation_id == request.operation_id
                && self.authority.profile == request.profile
                && binding.attempt_id == request.attempt_id
                && binding.grant_generation == request.grant_generation
                && binding.sandbox_id == request.sandbox_id
                && binding.node == request.node,
            ContractError::BindingMismatch,
        )?;
        self.authority
            .validate_permit(&self.permit, RunnerOperation::Prepared, now_ms)
    }
}
pub fn validate_issuance_replay(
    original_request: &PermitIssuanceRequest,
    original_response: &PermitIssuanceResponse,
    request: &PermitIssuanceRequest,
    response: &PermitIssuanceResponse,
    now_ms: u64,
) -> Result<()> {
    check(
        same_bytes(original_request, request) && same_bytes(original_response, response),
        ContractError::ReplayConflict,
    )?;
    response.validate_for(request, now_ms)
}

/// Sent with the frozen Prepared request, authenticated by the preparation
/// permit's credential (never carried in this body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermitUpgradeRequest {
    pub schema_version: u32,
    /// Equals the frozen Prepare's `operation_id`.
    pub operation_id: Uuid,
    pub profile: PhaseProfile,
    pub prepare_request_digest: Digest,
    pub accepted_receipt_digest: Digest,
    pub permit_id: Uuid,
    pub binding: PermitBinding,
}
macro_rules! prepare_correlation {
    ($name:ident, $module:ident, $profile:ident) => {
        /// Correlates this envelope to the exact frozen Prepared it accompanies.
        pub fn $name(&self, prepare: &crate::$module::PrepareRequest) -> Result<()> {
            self.validate()?;
            let original = &prepare.original;
            check(
                self.profile == PhaseProfile::$profile
                    && self.prepare_request_digest == frozen(prepare.digest())?
                    && self.accepted_receipt_digest == prepare.accepted_receipt_digest
                    && self.operation_id == original.operation_id
                    && self.binding.node == original.identity
                    && self.binding.attempt_id == original.binding.attempt_id
                    && self.binding.grant_generation == original.binding.assignment_generation,
                ContractError::BindingMismatch,
            )
        }
    };
}
impl PermitUpgradeRequest {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 1, ContractError::InvalidShape)?;
        nonnil(self.operation_id)?;
        nonnil(self.permit_id)?;
        self.binding.validate()
    }
    prepare_correlation!(validate_prepare_v2, preparation_v2, PhaseV2);
    prepare_correlation!(
        validate_prepare_refresh,
        preparation_refresh_v1,
        PhaseRefreshV1
    );
    /// Server-side scope check against retained preparation state and the
    /// authenticated installed permit: `Prepared` is preparation scope only.
    pub fn validate_for(
        &self,
        preparation: &PreparationAuthority,
        permit: &DelegatedPermit,
        now_ms: u64,
    ) -> Result<()> {
        self.validate()?;
        check(
            self.permit_id == permit.permit_id
                && self.binding == permit.binding
                && self.profile == preparation.profile
                && self.accepted_receipt_digest == preparation.accepted_receipt_digest,
            ContractError::BindingMismatch,
        )?;
        preparation.validate_permit(permit, RunnerOperation::Prepared, now_ms)
    }
}
/// Returned with the frozen execution delivery when the grant commits. The
/// same permit identity and binding are upgraded to execution scope, bounded by
/// the grant's execution deadline.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermitUpgradeResponse {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub authority: PhaseExecutionAuthority,
    pub permit: DelegatedPermit,
}
impl PermitUpgradeResponse {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 1, ContractError::InvalidShape)?;
        nonnil(self.operation_id)?;
        self.authority.validate_binding(&self.permit)
    }
    /// Runner-side check of an authenticated reply against the retained
    /// preparation state and installed preparation permit.
    pub fn validate_for(
        &self,
        request: &PermitUpgradeRequest,
        preparation: &PreparationAuthority,
        preparation_permit: &DelegatedPermit,
        now_ms: u64,
    ) -> Result<()> {
        request.validate()?;
        self.validate()?;
        preparation.validate()?;
        preparation_permit.validate()?;
        check(
            preparation_permit.scope == PermitScope::Preparation,
            ContractError::ScopeDenied,
        )?;
        check(
            self.operation_id == request.operation_id
                && self.authority.profile == request.profile
                && request.profile == preparation.profile
                && request.accepted_receipt_digest == preparation.accepted_receipt_digest
                && request.permit_id == preparation_permit.permit_id
                && request.binding == preparation_permit.binding
                && self.permit.permit_id == request.permit_id
                && self.permit.binding == request.binding
                && self.authority.grant.binding == preparation.binding,
            ContractError::BindingMismatch,
        )?;
        check(
            self.permit.issued_at_ms >= preparation_permit.issued_at_ms,
            ContractError::DeadlineMismatch,
        )?;
        self.authority.validate_handoff(&self.permit, now_ms)
    }
}
#[allow(clippy::too_many_arguments)]
pub fn validate_upgrade_replay(
    original_request: &PermitUpgradeRequest,
    original_response: &PermitUpgradeResponse,
    request: &PermitUpgradeRequest,
    response: &PermitUpgradeResponse,
    preparation: &PreparationAuthority,
    preparation_permit: &DelegatedPermit,
    now_ms: u64,
) -> Result<()> {
    check(
        same_bytes(original_request, request) && same_bytes(original_response, response),
        ContractError::ReplayConflict,
    )?;
    response.validate_for(request, preparation, preparation_permit, now_ms)
}

/// Registration v2 reply. `compatible` enrolls exactly the registered node
/// incarnation; a decline enrolls nothing and issues no permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistrationResponse {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub node: WorkerIdentity,
    pub compatibility: CompatibilityReply,
}
impl RegistrationResponse {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 2, ContractError::InvalidShape)?;
        nonnil(self.operation_id)?;
        node(&self.node)?;
        self.compatibility.validate()
    }
    pub fn validate_for(&self, registration: &Registration) -> Result<()> {
        registration.validate()?;
        self.validate()?;
        check(
            self.operation_id == registration.registration.operation_id
                && self.node == registration.registration.identity,
            ContractError::BindingMismatch,
        )?;
        match self.compatibility {
            CompatibilityReply::Compatible => Ok(()),
            CompatibilityReply::UnsupportedRequiredCapability { .. } => {
                Err(ContractError::UnsupportedRequiredCapability)
            }
        }
    }
}
pub fn validate_registration_replay(
    original_registration: &Registration,
    original_response: &RegistrationResponse,
    registration: &Registration,
    response: &RegistrationResponse,
) -> Result<()> {
    check(
        same_bytes(original_registration, registration) && same_bytes(original_response, response),
        ContractError::ReplayConflict,
    )?;
    response.validate_for(registration)
}
