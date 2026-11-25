//! Validates a check request against its prepared source and the project’s pinned checks.
use crate::{
    Error, Namespace, Result,
    host_requests::{ArtifactPin, CHECK_EXECUTION_PROFILE, CheckAttempt, CheckExecutor},
    host_values::{HostResult, revision_of, text},
};
use serde_json::{Value, json};
use uuid::Uuid;

pub enum BoundSource {
    Invalid,
    Stale,
    Prepared(Value),
}
pub struct RequestedCheck<'a> {
    operation: Uuid,
    namespace: &'a Namespace,
    pub input: &'a Value,
    pub subject: &'a Value,
    check: &'a Value,
    pub id: &'a str,
}
impl<'a> RequestedCheck<'a> {
    pub fn new(operation: Uuid, namespace: &'a Namespace, input: &'a Value) -> Result<Self> {
        let check = &input["check"];
        Ok(Self {
            operation,
            namespace,
            input,
            subject: &input["subject"],
            check,
            id: text(check, "id")?,
        })
    }
    pub fn request(&self, source: &Value, deadline: i64) -> Value {
        json!({"check":self.check,"source":source,"subject":self.subject,
            "deadline_at_ms":deadline,"execution_profile":CHECK_EXECUTION_PROFILE})
    }
    fn validator(&self) -> Result<ArtifactPin> {
        serde_json::from_value(self.check["validator"].clone()).map_err(|_| Error::Invalid)
    }
    pub fn attempt(&self, source: &Value) -> Result<CheckAttempt> {
        Ok(CheckAttempt {
            operation: self.operation,
            workspace: self.namespace.workspace_id,
            project: self.namespace.project_id,
            subject_id: text(self.subject, "id")?.into(),
            subject_revision: revision_of(self.subject)?,
            check_id: self.id.into(),
            validator: self.validator()?,
            source_format: text(source, "format")?.into(),
            source_hex: text(source, "hex")?.into(),
            execution_profile: CHECK_EXECUTION_PROFILE,
        })
    }
    pub async fn admit(
        &self,
        executor: Option<&dyn CheckExecutor>,
        source: BoundSource,
    ) -> Result<std::result::Result<Value, HostResult>> {
        let original = &self.input["source"];
        let prepared = match source {
            BoundSource::Invalid => {
                return Ok(Err(self.refusal(original, Value::Null, "invalid_request")));
            }
            BoundSource::Stale => {
                return Ok(Err(self.refusal(original, json!("stale"), "stale_subject")));
            }
            BoundSource::Prepared(source) => source,
        };
        let Some(executor) = executor else {
            return Ok(Err(self.refusal(&prepared, Value::Null, "unavailable")));
        };
        let validator = self.validator()?;
        let pin = executor
            .pinned(
                self.namespace.workspace_id,
                self.namespace.project_id,
                text(self.subject, "id")?,
            )
            .await?;
        let Some(pin) = pin else {
            return Ok(Err(self.refusal(&prepared, Value::Null, "not_required")));
        };
        if pin.revision != revision_of(self.subject)? {
            return Ok(Err(self.refusal(
                &prepared,
                json!("stale"),
                "stale_subject",
            )));
        }
        let required = pin.required_checks.len() <= 32
            && pin
                .required_checks
                .iter()
                .any(|c| c.id == self.id && c.validator == validator);
        if !required {
            return Ok(Err(self.refusal(&prepared, Value::Null, "not_required")));
        }
        Ok(Ok(prepared))
    }
    fn refusal(&self, source: &Value, disposition: Value, reason: &str) -> HostResult {
        HostResult {
            outcome: "refused".into(),
            output: json!({"check":self.check,"disposition":disposition,"reason":reason,
            "reference":format!("check:{}",self.operation),"source":source,"subject":self.subject}),
        }
    }
}
