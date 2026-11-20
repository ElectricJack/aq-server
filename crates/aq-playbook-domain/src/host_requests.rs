//! The injected ports of the asynchronous host commands (catalogs 2.0.0 to
//! 2.2.0): the sandbox check executor and the judgment provider, with the
//! values that cross them. No secret crosses either boundary.
use crate::{Result, digest, host_values::HostResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

/// Turns an executor observation into a completion when it has stopped.
pub fn check_completion(
    operation: Uuid,
    r: &Value,
    observation: CheckObservation,
    now: i64,
) -> Result<Option<HostResult>> {
    let base = |outcome: &str, disposition: &str, receipt: Value| HostResult {
        outcome: outcome.into(),
        output: json!({"check": r["check"], "disposition": disposition, "receipt": receipt,
            "reference": format!("check-receipt:{operation}"), "source": r["source"], "subject": r["subject"]}),
    };
    let result = match observation {
        CheckObservation::Running => return Ok(None),
        CheckObservation::NotFound => {
            if now < r["deadline_at_ms"].as_i64().ok_or(crate::Error::Unknown)? {
                return Ok(None);
            }
            base("infrastructure_failure", "unknown", Value::Null)
        }
        CheckObservation::Finished(run) => {
            let (code, stop) = match run.stop {
                CheckStop::Exited(code) => (json!(code), "exited"),
                CheckStop::Signalled => (Value::Null, "signalled"),
                CheckStop::TimedOut => (Value::Null, "timed_out"),
                CheckStop::Cancelled => (Value::Null, "cancelled"),
                CheckStop::Lost => (Value::Null, "lost"),
            };
            let exit = json!({"code": code, "stop": stop});
            let artifacts = serde_json::to_value(&run.result_artifacts)?;
            let receipt_digest = digest(&(
                "aq.check.receipt",
                &r["subject"],
                &r["check"],
                &r["source"],
                CHECK_EXECUTION_PROFILE,
                &run.executor_version,
                &run.inputs_digest,
                &artifacts,
                &exit,
            ))?;
            let receipt = json!({"digest": receipt_digest, "executor_version": run.executor_version,
                "inputs_digest": run.inputs_digest, "result_artifacts": artifacts, "exit": exit});
            // Missing input or an uncertain stop is never passed.
            match (&run.stop, run.inputs_complete) {
                (CheckStop::Exited(0), true) => base("passed", "passed", receipt),
                (CheckStop::Exited(_), true) => base("failed", "failed", receipt),
                (CheckStop::TimedOut, _) => base("infrastructure_failure", "timed_out", receipt),
                (CheckStop::Cancelled, _) => base("infrastructure_failure", "cancelled", receipt),
                _ => base("infrastructure_failure", "unknown", receipt),
            }
        }
    };
    Ok(Some(result))
}
/// One accepted judgment whose provider request runs off the dispatch path.
pub struct JudgmentJob {
    pub operation: Uuid,
    pub command: String,
    pub intent_id: String,
    pub reference: String,
    pub subject: Value,
    pub credential_profile: String,
    pub prompt_digest: String,
    pub context_digest: String,
    pub request: JudgmentRequest,
}

impl JudgmentJob {
    /// Turns a provider result into its schema-checked outcome and provenance.
    /// A vanished reply represents a deadline exceeded.
    pub fn completion(
        &self,
        reply: Option<std::result::Result<JudgmentReply, ProviderFailure>>,
    ) -> Result<HostResult> {
        let Self {
            reference,
            subject,
            credential_profile,
            prompt_digest,
            context_digest,
            request,
            ..
        } = self;
        let refusal = |provenance: Value, reason: &str| HostResult {
            outcome: "refused".into(),
            output: json!({"provenance": provenance, "reason": reason, "reference": reference, "subject": subject}),
        };
        let provenance = |reply: &JudgmentReply, output: Value| {
            json!({"provider": reply.provider, "model": reply.model,
                "credential_profile": credential_profile, "prompt_digest": prompt_digest,
                "context_digest": context_digest, "output_digest": output,
                "usage": {"input_tokens": reply.input_tokens, "output_tokens": reply.output_tokens,
                    "cost_microunits": reply.cost_microunits}})
        };
        let failed = |reason: &str| HostResult {
            outcome: "failed".into(),
            output: json!({"provenance": null, "reason": reason, "reference": reference, "subject": subject}),
        };
        Ok(match reply {
            None => refusal(Value::Null, "deadline_exceeded"),
            Some(Err(ProviderFailure::Error)) => failed("provider_error"),
            Some(Err(ProviderFailure::Unavailable)) => failed("provider_unavailable"),
            Some(Ok(reply)) => {
                if reply.input_tokens > request.max_input_tokens
                    || reply.output_tokens > request.max_output_tokens
                    || reply.cost_microunits > request.max_cost_microunits
                {
                    refusal(provenance(&reply, Value::Null), "budget_exceeded")
                } else if !request.choices.contains(&reply.choice)
                    || reply.rationale.len() as u64 > request.rationale_max_bytes
                    || reply.provider.is_empty()
                    || reply.provider.len() > 128
                    || reply.model.is_empty()
                    || reply.model.len() > 256
                {
                    refusal(Value::Null, "invalid_output")
                } else {
                    let output_digest =
                        digest(&("aq.judgment.output", &reply.choice, &reply.rationale))?;
                    HostResult {
                        outcome: "assessed".into(),
                        output: json!({"choice": reply.choice, "rationale": reply.rationale,
                            "provenance": provenance(&reply, json!(output_digest)),
                            "reference": reference, "subject": subject}),
                    }
                }
            }
        })
    }
}
