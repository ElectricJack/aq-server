use super::*;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LiveSandbox {
    pub permit_id: Uuid,
    pub attempt_id: Uuid,
    pub grant_generation: u64,
    pub sandbox_id: Uuid,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadoptionRequest {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub previous: WorkerIdentity,
    pub current: WorkerIdentity,
    #[schemars(length(max = 128))]
    pub sandboxes: Vec<LiveSandbox>,
}
fn transition(previous: &WorkerIdentity, current: &WorkerIdentity) -> Result<()> {
    node(previous)?;
    node(current)?;
    check(
        previous.node_id == current.node_id && previous.boot_id != current.boot_id,
        ContractError::BindingMismatch,
    )
}
impl ReadoptionRequest {
    pub fn validate(&self) -> Result<()> {
        check(
            self.schema_version == 1 && self.sandboxes.len() <= MAX_SANDBOXES,
            ContractError::InvalidShape,
        )?;
        nonnil(self.operation_id)?;
        transition(&self.previous, &self.current)?;
        let (mut attempts, mut sandboxes, mut permits) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for sandbox in &self.sandboxes {
            nonnil(sandbox.permit_id)?;
            nonnil(sandbox.attempt_id)?;
            nonnil(sandbox.sandbox_id)?;
            positive(sandbox.grant_generation)?;
            check(
                attempts.insert(sandbox.attempt_id)
                    && sandboxes.insert(sandbox.sandbox_id)
                    && permits.insert(sandbox.permit_id),
                ContractError::InvalidShape,
            )?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadoptionRefusal {
    UnknownSandbox,
    NoCurrentGrant,
    Expired,
    Revoked,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadoptionResult {
    Rebound {
        permit: DelegatedPermit,
    },
    Refused {
        sandbox_id: Uuid,
        code: ReadoptionRefusal,
    },
}
impl ReadoptionResult {
    fn sandbox_id(&self) -> Uuid {
        match self {
            Self::Rebound { permit } => permit.binding.sandbox_id,
            Self::Refused { sandbox_id, .. } => *sandbox_id,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadoptionResponse {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub previous: WorkerIdentity,
    pub current: WorkerIdentity,
    #[schemars(length(max = 128))]
    pub results: Vec<ReadoptionResult>,
}
impl ReadoptionResponse {
    pub fn validate(&self) -> Result<()> {
        check(
            self.schema_version == 1 && self.results.len() <= MAX_SANDBOXES,
            ContractError::InvalidShape,
        )?;
        nonnil(self.operation_id)?;
        transition(&self.previous, &self.current)?;
        let mut seen = BTreeSet::new();
        let mut channels = BTreeSet::new();
        for result in &self.results {
            nonnil(result.sandbox_id())?;
            check(
                seen.insert(result.sandbox_id()),
                ContractError::InvalidShape,
            )?;
            if let ReadoptionResult::Rebound { permit } = result {
                permit.validate()?;
                check(
                    permit.binding.node == self.current,
                    ContractError::BindingMismatch,
                )?;
                check(
                    channels.insert(permit.binding.channel_id),
                    ContractError::InvalidShape,
                )?;
            }
        }
        Ok(())
    }
    /// `previous_permits` must come from authenticated current server records.
    /// No stored state is mutated; the server atomically fences incarnations.
    pub fn validate_for(
        &self,
        request: &ReadoptionRequest,
        previous_permits: &[DelegatedPermit],
        now_ms: u64,
    ) -> Result<()> {
        request.validate()?;
        self.validate()?;
        positive(now_ms)?;
        check(
            self.operation_id == request.operation_id
                && self.previous == request.previous
                && self.current == request.current
                && self.results.len() == request.sandboxes.len(),
            ContractError::BindingMismatch,
        )?;
        check(
            previous_permits.len() <= MAX_SANDBOXES,
            ContractError::InvalidShape,
        )?;
        let mut seen = BTreeSet::new();
        for previous in previous_permits {
            previous.validate()?;
            check(seen.insert(previous.permit_id), ContractError::InvalidShape)?;
        }
        for result in &self.results {
            let listed = request
                .sandboxes
                .iter()
                .find(|s| s.sandbox_id == result.sandbox_id())
                .ok_or(ContractError::BindingMismatch)?;
            if let ReadoptionResult::Rebound { permit } = result {
                let old = previous_permits
                    .iter()
                    .find(|p| p.permit_id == listed.permit_id)
                    .ok_or(ContractError::BindingMismatch)?;
                check(
                    old.binding.node == request.previous
                        && old.binding.attempt_id == listed.attempt_id
                        && old.binding.grant_generation == listed.grant_generation
                        && old.binding.sandbox_id == listed.sandbox_id
                        && old.permit_id == permit.permit_id,
                    ContractError::BindingMismatch,
                )?;
                let mut rebound_binding = old.binding.clone();
                rebound_binding.node = request.current.clone();
                check(
                    permit.binding == rebound_binding,
                    ContractError::BindingMismatch,
                )?;
                check(
                    permit.scope == old.scope && old.scope == PermitScope::Execution,
                    ContractError::ScopeDenied,
                )?;
                check(
                    permit.issued_at_ms == old.issued_at_ms
                        && permit.expires_at_ms == old.expires_at_ms
                        && permit.hard_deadline_ms == old.hard_deadline_ms,
                    ContractError::DeadlineMismatch,
                )?;
                check(
                    old.issued_at_ms <= now_ms && now_ms < old.expires_at_ms,
                    ContractError::Expired,
                )?;
            }
        }
        Ok(())
    }
}
/// Exact replay is idempotent, never a fresh lease. The caller also verifies
/// that the server's current boot is still the operation's target incarnation.
pub fn validate_replay(
    original_request: &ReadoptionRequest,
    original_response: &ReadoptionResponse,
    request: &ReadoptionRequest,
    response: &ReadoptionResponse,
    previous_permits: &[DelegatedPermit],
    now_ms: u64,
) -> Result<()> {
    check(
        original_request == request && original_response == response,
        ContractError::ReplayConflict,
    )?;
    response.validate_for(request, previous_permits, now_ms)
}
