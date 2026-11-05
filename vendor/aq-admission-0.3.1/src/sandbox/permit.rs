use super::*;

/// An opaque server-verified credential, never a self-authorizing JSON claim.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PermitCredential(
    #[schemars(length(min = 32, max = 256), regex(pattern = "^[A-Za-z0-9_-]+$"))] String,
);
impl std::fmt::Debug for PermitCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PermitCredential([REDACTED])")
    }
}
impl PermitCredential {
    pub fn expose(&self) -> &str {
        &self.0
    }
    fn validate(&self) -> Result<()> {
        check(
            (32..=256).contains(&self.0.len())
                && self
                    .0
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
            ContractError::InvalidShape,
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermitScope {
    Preparation,
    Execution,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrantPhase {
    Preparing,
    Granted,
    Ended,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunnerOperation {
    FetchPreparationMaterial,
    Observe,
    UploadArtifact,
    Started,
    Renew,
    Terminal,
    FetchExecutionMaterial,
    /// Result submission, including the native stopped report. In Ended this
    /// allows result submission and only an exact replay of a retained stop;
    /// use `DelegatedPermit::validate_stopped_replay` for the latter.
    ReportResult,
    AcceptOffer,
    RegisterIncarnation,
    /// Preparation receipt submission (frozen Prepared). Preparation scope only.
    Prepared,
    /// Session-addressed mailbox fetch. Execution scope only: no session exists
    /// before the grant. This is not the node's offer poll, which no permit allows.
    MailboxPoll,
    /// Acknowledge a durably persisted mailbox receipt. Execution scope only.
    MailboxAck,
}
/// The server assigns one channel per sandbox and retains it across boot changes.
/// Both terminal and observation streams use this same identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerChannelIdentity {
    pub attempt_id: Uuid,
    pub grant_generation: u64,
    pub sandbox_id: Uuid,
    pub node: WorkerIdentity,
    pub channel_id: Uuid,
}
impl RunnerChannelIdentity {
    pub fn validate(&self) -> Result<()> {
        for value in [self.attempt_id, self.sandbox_id, self.channel_id] {
            nonnil(value)?;
        }
        node(&self.node)?;
        positive(self.grant_generation)
    }
}
pub type PermitBinding = RunnerChannelIdentity;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegatedPermit {
    pub schema_version: u32,
    pub permit_id: Uuid,
    pub credential: PermitCredential,
    pub binding: PermitBinding,
    pub scope: PermitScope,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub hard_deadline_ms: u64,
}
/// Trusted server state; not request-supplied evidence. During preparation the
/// allocated next grant generation and preparation deadline occupy these fields.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermitContext {
    pub binding: PermitBinding,
    pub phase: GrantPhase,
    pub grant_expires_at_ms: u64,
    pub hard_deadline_ms: u64,
}
impl DelegatedPermit {
    pub fn validate(&self) -> Result<()> {
        check(self.schema_version == 1, ContractError::InvalidShape)?;
        nonnil(self.permit_id)?;
        self.credential.validate()?;
        self.binding.validate()?;
        positive(self.issued_at_ms)?;
        positive(self.expires_at_ms)?;
        positive(self.hard_deadline_ms)?;
        check(
            self.issued_at_ms < self.expires_at_ms && self.expires_at_ms <= self.hard_deadline_ms,
            ContractError::DeadlineMismatch,
        )
    }
    /// Check scope against trusted current state. Ended retains only execution
    /// ReportResult authority within the existing permit/grant/hard deadlines.
    /// This never authorizes a new stop report after the native stop committed;
    /// the server must compare that retry with its retained original report.
    pub fn validate_for(
        &self,
        context: &PermitContext,
        operation: RunnerOperation,
        now_ms: u64,
    ) -> Result<()> {
        self.validate()?;
        context.binding.validate()?;
        positive(context.grant_expires_at_ms)?;
        positive(context.hard_deadline_ms)?;
        positive(now_ms)?;
        check(
            self.binding == context.binding,
            ContractError::BindingMismatch,
        )?;
        check(
            self.hard_deadline_ms == context.hard_deadline_ms
                && self.expires_at_ms <= context.grant_expires_at_ms
                && context.grant_expires_at_ms <= context.hard_deadline_ms,
            ContractError::DeadlineMismatch,
        )?;
        check(
            self.issued_at_ms <= now_ms && now_ms < self.expires_at_ms,
            ContractError::Expired,
        )?;
        use RunnerOperation::*;
        let allowed = match (self.scope, context.phase) {
            (PermitScope::Preparation, GrantPhase::Preparing) => matches!(
                operation,
                FetchPreparationMaterial | Observe | UploadArtifact | Prepared
            ),
            (PermitScope::Execution, GrantPhase::Granted) => matches!(
                operation,
                Started
                    | Renew
                    | Observe
                    | Terminal
                    | FetchExecutionMaterial
                    | UploadArtifact
                    | ReportResult
                    | MailboxPoll
                    | MailboxAck
            ),
            (PermitScope::Execution, GrantPhase::Ended) => operation == ReportResult,
            _ => false,
        };
        check(allowed, ContractError::ScopeDenied)
    }

    /// Authorize an exact canonical stopped-report replay against the original
    /// report retained by the server. The original must have been authenticated,
    /// validated against Started and atomically recorded with the native stop.
    /// Neither input is trusted merely because it decodes as a stopped request.
    /// Replay does not extend expiry or permit renewal, access, or launch.
    pub fn validate_stopped_replay(
        &self,
        context: &PermitContext,
        original: &crate::stopped::Request,
        request: &crate::stopped::Request,
        now_ms: u64,
    ) -> Result<()> {
        self.validate_for(context, RunnerOperation::ReportResult, now_ms)?;
        let original_digest = original.digest().map_err(|_| ContractError::InvalidShape)?;
        let request_digest = request.digest().map_err(|_| ContractError::InvalidShape)?;
        check(
            original_digest == request_digest,
            ContractError::ReplayConflict,
        )?;
        let binding = &request.evidence.binding;
        let crate::SessionIdentity::Remote {
            node_id, boot_id, ..
        } = &binding.session
        else {
            return Err(ContractError::BindingMismatch);
        };
        check(
            binding.attempt_id == self.binding.attempt_id
                && binding.assignment_generation == self.binding.grant_generation
                && *node_id == self.binding.node.node_id
                && *boot_id == self.binding.node.boot_id,
            ContractError::BindingMismatch,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RenewalReply {
    /// No delivery clock, expiry or hard deadline: this reply extends nothing.
    AwaitingNodeReadoption {
        operation_id: Uuid,
        binding: PermitBinding,
    },
    Renewed {
        operation_id: Uuid,
        permit: DelegatedPermit,
    },
}
impl RenewalReply {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::AwaitingNodeReadoption {
                operation_id,
                binding,
            } => {
                nonnil(*operation_id)?;
                binding.validate()
            }
            Self::Renewed {
                operation_id,
                permit,
            } => {
                nonnil(*operation_id)?;
                permit.validate()?;
                check(
                    permit.scope == PermitScope::Execution,
                    ContractError::ScopeDenied,
                )
            }
        }
    }
}
