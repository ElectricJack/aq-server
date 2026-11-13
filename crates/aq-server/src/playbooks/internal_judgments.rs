//! `judgment.request` served by internal work (spec 0012 §6).
use super::host_adapters::{
    JudgmentProvider, JudgmentReply, JudgmentRequest, ProviderFailure, estimate_input_tokens,
};
use crate::{InternalWork, error::AppError};
use aq_internal_work::{Affected, AnswerShape, Outcome, Purpose, Reason, Request};
use uuid::Uuid;

pub struct InternalJudgments {
    work: InternalWork,
    purpose: Purpose,
}

impl InternalJudgments {
    pub fn new(work: InternalWork, purpose: Purpose) -> Self {
        Self { work, purpose }
    }
}

#[async_trait::async_trait]
impl JudgmentProvider for InternalJudgments {
    /// Internal work picks the model and credential itself.
    fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
        Some("internal-work".into())
    }

    async fn complete(&self, r: &JudgmentRequest) -> Result<JudgmentReply, ProviderFailure> {
        let request = Request {
            purpose: self.purpose,
            instructions: r.question.clone(),
            input: r.context_text.clone(),
            answer: AnswerShape::Choice {
                choices: r.choices.clone(),
                rationale_max_bytes: r.rationale_max_bytes,
            },
            blocking: true,
            affected: Affected {
                kind: "playbook_operation".into(),
                id: r.operation.to_string(),
                project_id: None,
            },
            deadline_ms: r.deadline_ms,
            max_output_bytes: r.rationale_max_bytes.saturating_add(4096),
            escalate: false,
            profile_override: None,
            agent: None,
        };
        let (answer, provider, model, usage) = match self.work.run(request).await {
            Ok(Outcome::Answered {
                answer,
                provider,
                model,
                usage,
                ..
            }) => (answer, provider, model, usage),
            Ok(Outcome::Failed { reason, .. }) => {
                return Err(match reason {
                    Reason::ProviderUnavailable
                    | Reason::DeadlineExceeded
                    | Reason::NoCandidate => ProviderFailure::Unavailable,
                    _ => ProviderFailure::Error,
                });
            }
            Err(AppError::BadRequest(_)) => return Err(ProviderFailure::Error),
            Err(_) => return Err(ProviderFailure::Unavailable),
        };
        let text = |key: &str| answer[key].as_str().map(str::to_owned);
        let (Some(choice), Some(rationale)) = (text("choice"), text("rationale")) else {
            return Err(ProviderFailure::Error);
        };
        // Without reported usage: the host's estimate, one token per four bytes.
        let output_tokens = ((choice.len() + rationale.len()) as u64).div_ceil(4);
        let (input_tokens, output_tokens) = usage
            .map_or((estimate_input_tokens(r), output_tokens), |u| {
                (u.input_tokens, u.output_tokens)
            });
        Ok(JudgmentReply {
            provider,
            model,
            choice,
            rationale,
            input_tokens,
            output_tokens,
            cost_microunits: 0,
        })
    }
}
