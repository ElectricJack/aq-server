//! Builds an asynchronous judgment and the record retained before its provider runs.
use crate::{
    Error, Result, digest,
    host_requests::{JudgmentJob, JudgmentRequest},
    host_values::{HostResult, text},
};
use aq_playbook_host::runner::Intent;
use serde_json::{Value, json};
use uuid::Uuid;
pub struct JudgmentAdmission<'a> {
    operation: Uuid,
    intent: &'a Intent,
    choices: Vec<String>,
}
impl<'a> JudgmentAdmission<'a> {
    pub fn new(operation: Uuid, intent: &'a Intent) -> Result<Self> {
        let choices = serde_json::from_value(intent.input["output"]["choices"].clone())
            .map_err(|_| Error::Invalid)?;
        Ok(Self {
            operation,
            intent,
            choices,
        })
    }
    pub fn subject(&self) -> &Value {
        &self.intent.input["subject"]
    }
    pub fn refusal(&self, reason: &str) -> HostResult {
        HostResult {
            outcome: "refused".into(),
            output: json!({"provenance":null,"reason":reason,"reference":format!("judgment:{}",self.operation),"subject":self.subject()}),
        }
    }
    pub fn refused(&self, profile_available: bool, stale: bool) -> Result<Option<HostResult>> {
        let reason = if self.choices.is_empty() || text(&self.intent.input, "question")?.is_empty()
        {
            "invalid_request"
        } else if !profile_available {
            "no_permitted_profile"
        } else if stale {
            "stale_subject"
        } else {
            return Ok(None);
        };
        Ok(Some(self.refusal(reason)))
    }
    pub fn request(&self, credential_profile: String) -> Result<JudgmentRequest> {
        let input = &self.intent.input;
        let budget = &input["budget"];
        let field = |name: &str| budget[name].as_u64().ok_or(Error::Invalid);
        // The provider runs off the dispatch path, so its deadline is the
        // declared budget, not what is left of the dispatch deadline.
        Ok(JudgmentRequest {
            operation: self.operation,
            credential_profile,
            question: text(input, "question")?.into(),
            context_text: text(&input["context"], "text")?.into(),
            context_artifacts: serde_json::from_value(input["context"]["artifacts"].clone())
                .map_err(|_| Error::Invalid)?,
            choices: self.choices.clone(),
            rationale_max_bytes: input["output"]["rationale_max_bytes"]
                .as_u64()
                .ok_or(Error::Invalid)?,
            max_input_tokens: field("max_input_tokens")?,
            max_output_tokens: field("max_output_tokens")?,
            max_cost_microunits: field("max_cost_microunits")?,
            deadline_ms: field("deadline_ms")?,
        })
    }
    pub fn job(self, request: JudgmentRequest) -> Result<JudgmentJob> {
        let prompt_digest = digest(&(
            "aq.judgment.prompt",
            &request.question,
            &request.choices,
            request.rationale_max_bytes,
        ))?;
        let context_digest = digest(&("aq.judgment.context", &self.intent.input["context"]))?;
        Ok(JudgmentJob {
            operation: self.operation,
            command: self.intent.command.name.clone(),
            intent_id: self.intent.id.clone(),
            reference: format!("judgment:{}", self.operation),
            subject: self.subject().clone(),
            credential_profile: request.credential_profile.clone(),
            prompt_digest,
            context_digest,
            request,
        })
    }
}
impl JudgmentJob {
    pub fn admission_record(&self, estimated_input_tokens: u64, now: i64) -> Value {
        json!({"subject":self.subject,"credential_profile":self.credential_profile,"prompt_digest":self.prompt_digest,"context_digest":self.context_digest,
             "estimated_input_tokens":estimated_input_tokens,"deadline_at_ms":now+self.request.deadline_ms as i64})
    }
}
