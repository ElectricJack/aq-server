//! The injected ports of the asynchronous host commands (catalogs 2.0.0 to
//! 2.2.0): the sandbox check executor and the judgment provider, with the
//! values that cross them. No secret crosses either boundary.
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Check execution profile every D6 check attempt runs under.
pub const CHECK_EXECUTION_PROFILE: &str = "aq.check.sandbox/v1";

/// Immutable artifact reference (id, version, digest).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPin {
    pub id: String,
    pub version: String,
    pub digest: String,
}

/// One required check of a pinned expected outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedCheck {
    pub id: String,
    pub validator: ArtifactPin,
}

/// The expected outcome pinned for a subject: its revision and its
/// required checks (at most 32).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedOutcome {
    pub revision: i64,
    pub required_checks: Vec<PinnedCheck>,
}

/// One sandbox check attempt. Carries no shell, URL or path: the executor
/// resolves the validator and source from their immutable identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckAttempt {
    /// Idempotency key: one attempt per operation, ever.
    pub operation: Uuid,
    pub workspace: Uuid,
    pub project: Uuid,
    pub subject_id: String,
    pub subject_revision: i64,
    pub check_id: String,
    pub validator: ArtifactPin,
    pub source_format: String,
    pub source_hex: String,
    pub execution_profile: &'static str,
}

/// How a sandbox check attempt stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckStop {
    Exited(i32),
    Signalled,
    TimedOut,
    Cancelled,
    Lost,
}

/// Executor-reported evidence for a stopped attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRun {
    pub stop: CheckStop,
    /// False when any declared input was missing; such a run is never passed.
    pub inputs_complete: bool,
    pub executor_version: String,
    pub inputs_digest: String,
    pub result_artifacts: Vec<ArtifactPin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckObservation {
    Running,
    Finished(CheckRun),
    /// The executor holds no attempt for this operation.
    NotFound,
}

/// Sandbox check executor port (C2/B5 backend in production).
#[async_trait::async_trait]
pub trait CheckExecutor: Send + Sync {
    /// The expected outcome pinned for `subject_id`, if any.
    async fn pinned(
        &self,
        workspace: Uuid,
        project: Uuid,
        subject_id: &str,
    ) -> Result<Option<PinnedOutcome>>;
    /// Starts the attempt; idempotent by `attempt.operation`. An error is
    /// ambiguous: the attempt may or may not exist.
    async fn start(&self, attempt: &CheckAttempt) -> Result<()>;
    async fn observe(&self, operation: Uuid) -> Result<CheckObservation>;
    /// Requests that the attempt stop; idempotent by `operation`, and a
    /// cancelled operation is never started afterwards. Acknowledgement only:
    /// the attempt is cancelled once `observe` reports `CheckStop::Cancelled`.
    /// An error is ambiguous.
    async fn cancel(&self, operation: Uuid) -> Result<()>;
}

/// One bounded provider request. `credential_profile` is a reference chosen
/// by the host; the secret it names never crosses this boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgmentRequest {
    pub operation: Uuid,
    pub credential_profile: String,
    pub question: String,
    pub context_text: String,
    pub context_artifacts: Vec<ArtifactPin>,
    pub choices: Vec<String>,
    pub rationale_max_bytes: u64,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_cost_microunits: u64,
    pub deadline_ms: u64,
}

/// A provider completion with its reported usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgmentReply {
    pub provider: String,
    pub model: String,
    pub choice: String,
    pub rationale: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microunits: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFailure {
    Error,
    Unavailable,
}

/// Default pre-call input-token estimate: one token per four UTF-8 bytes of
/// the text the host sends (question, context text and choices), rounded up.
/// Artifact contents are resolved by the provider, so a provider that inlines
/// them should override [`JudgmentProvider::estimate_input_tokens`]. The
/// provider-reported usage check after the call remains authoritative.
pub fn estimate_input_tokens(request: &JudgmentRequest) -> u64 {
    let bytes = request.question.len()
        + request.context_text.len()
        + request.choices.iter().map(String::len).sum::<usize>();
    (bytes as u64).div_ceil(4)
}

/// Grace after a judgment's provider deadline before `operation.get` treats a
/// still-running operation as lost (indeterminate). The in-process provider
/// task settles at its own deadline; the grace keeps reconciliation from
/// racing it.
pub const JUDGMENT_RECONCILE_GRACE_MS: i64 = 5_000;

/// Judgment provider port: one completion per request, no tool loop.
#[async_trait::async_trait]
pub trait JudgmentProvider: Send + Sync {
    /// The credential profile reference the host permits for this scope.
    fn credential_profile(&self, workspace: Uuid, project: Uuid) -> Option<String>;
    /// Input tokens `request` is expected to consume. A request whose
    /// estimate exceeds `max_input_tokens` is refused `budget_exceeded`
    /// before anything is sent.
    fn estimate_input_tokens(&self, request: &JudgmentRequest) -> u64 {
        estimate_input_tokens(request)
    }
    async fn complete(
        &self,
        request: &JudgmentRequest,
    ) -> std::result::Result<JudgmentReply, ProviderFailure>;
    /// One structured answer (`judgment.assess`); the error is the catalog's
    /// `failed` reason. A provider that cannot answer one is unavailable.
    async fn assess(&self, _: &AssessRequest) -> std::result::Result<AssessReply, &'static str> {
        Err("provider_unavailable")
    }
}

/// One structured judgment for the provider. No secret crosses this boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct AssessRequest {
    pub operation: Uuid,
    /// The internal work purpose of the source the playbook was authored for.
    pub purpose: aq_internal_work::Purpose,
    pub instructions: String,
    pub input: String,
    /// JSON Schema of the answer.
    pub schema: Value,
    pub deadline_ms: u64,
    pub max_output_bytes: u64,
}

/// The provider's answer (an object of the request's schema) and its usage.
#[derive(Clone, Debug, PartialEq)]
pub struct AssessReply {
    pub answer: Value,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
