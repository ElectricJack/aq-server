//! Pure bounded wire claims. Digests/schemas do not authenticate a human,
//! executor, retained source or result eligibility. Resolve actual owner rows.
pub use aq_session_protocol::{
    ArtifactRef, Digest, LaunchBinding, SessionIdentity, Text, Validate,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;
pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_BYTES: usize = 65536;
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("invalid review contract: {0}")]
pub struct Invalid(pub &'static str);
pub type Result<T> = std::result::Result<T, Invalid>;
fn require(value: bool, reason: &'static str) -> Result<()> {
    if value { Ok(()) } else { Err(Invalid(reason)) }
}
fn id(value: Uuid) -> Result<()> {
    require(!value.is_nil(), "nil identity")
}
fn revision(value: u64) -> Result<()> {
    require(value > 0 && value <= i64::MAX as u64, "revision/time bound")
}
fn artifact(value: &ArtifactRef) -> Result<()> {
    value
        .id
        .validate()
        .map_err(|_| Invalid("artifact identity"))?;
    value
        .version
        .validate()
        .map_err(|_| Invalid("artifact version"))
}
fn content_artifact(
    content: &impl Serialize,
    id: String,
    identity_error: &'static str,
    version_error: &'static str,
) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| Invalid(identity_error))?,
        version: Text::new("1").map_err(|_| Invalid(version_error))?,
        digest: digest(content)?,
    })
}
pub fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::other("review byte bound"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Count(MAX_BYTES), value).map_err(|_| Invalid("canonical byte bound"))?;
    let mut value = serde_json::to_value(value).map_err(|_| Invalid("encoding"))?;
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).map_err(|_| Invalid("encoding"))?;
    require(bytes.len() <= MAX_BYTES, "canonical byte bound")?;
    Ok(bytes)
}
pub fn digest(value: &impl Serialize) -> Result<Digest> {
    Ok(Digest::of(&canonical_bytes(value)?))
}
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    require(bytes.len() <= MAX_BYTES, "input byte bound")?;
    serde_json::from_slice(bytes).map_err(|_| Invalid("JSON"))
}
macro_rules! record {($name:ident {$($(#[$meta:meta])* $field:ident:$ty:ty),* $(,)?})=>{#[derive(Debug,Clone,Serialize,Deserialize,JsonSchema,PartialEq,Eq)] #[serde(deny_unknown_fields)] pub struct $name {$($(#[$meta])* pub $field:$ty),*}}}
record!(TaskRevision {
    workspace_id: Uuid,
    task_id: Uuid,
    version: u64,
    content_revision: u64
});
impl TaskRevision {
    pub fn validate(&self) -> Result<()> {
        id(self.workspace_id)?;
        id(self.task_id)?;
        revision(self.version)?;
        revision(self.content_revision)
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
record!(ObjectId {
    format: ObjectFormat,
    #[schemars(length(min = 40, max = 64))]
    hex: String
});
impl ObjectId {
    pub fn validate(&self) -> Result<()> {
        let length = match self.format {
            ObjectFormat::Sha1 => 40,
            ObjectFormat::Sha256 => 64,
        };
        require(
            self.hex.len() == length
                && self
                    .hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "canonical object identity",
        )
    }
}
record!(ResultPin {
    report_id: Uuid,
    report_digest: Digest,
    record_receipt: ArtifactRef,
    stopped_receipt: ArtifactRef
});
record!(SourcePin {
    repository_id: Uuid,
    binding_generation: u64,
    base: ObjectId,
    resulting_object: ObjectId,
    input_artifact: ArtifactRef,
    resulting_artifact: ArtifactRef,
    source_receipt_id: Uuid,
    retention_receipt: ArtifactRef
});
impl SourcePin {
    pub fn validate(&self) -> Result<()> {
        id(self.repository_id)?;
        id(self.source_receipt_id)?;
        revision(self.binding_generation)?;
        self.base.validate()?;
        self.resulting_object.validate()?;
        require(
            self.base.format == self.resulting_object.format,
            "repository object format changed",
        )?;
        for a in [
            &self.input_artifact,
            &self.resulting_artifact,
            &self.retention_receipt,
        ] {
            artifact(a)?;
        }
        Ok(())
    }
}
record!(ReviewSubject {schema_version:u32,project_id:Uuid,original_project_revision:u64,task:TaskRevision,binding:LaunchBinding,execution_spec:ArtifactRef,expected_outcome:ArtifactRef,task_observation:ArtifactRef,task_context:ArtifactRef,validation_policy:ArtifactRef,result:ResultPin,source:SourcePin,#[schemars(length(min=1,max=32))] criteria:Vec<Text>});
impl ReviewSubject {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == SCHEMA_VERSION, "subject schema")?;
        id(self.project_id)?;
        revision(self.original_project_revision)?;
        self.task.validate()?;
        self.binding
            .validate()
            .map_err(|_| Invalid("launch binding"))?;
        revision(self.binding.assignment_generation)?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "initial subject requires recorded native worker binding",
        )?;
        self.source.validate()?;
        id(self.result.report_id)?;
        for a in [
            &self.execution_spec,
            &self.expected_outcome,
            &self.task_observation,
            &self.task_context,
            &self.validation_policy,
            &self.result.record_receipt,
            &self.result.stopped_receipt,
        ] {
            artifact(a)?;
        }
        require(
            self.execution_spec.id.as_str() == "aq.profiled-execution-spec"
                && self.execution_spec.version.as_str() == "1",
            "execution spec reference",
        )?;
        require(
            self.execution_spec.digest == self.binding.execution_spec_digest,
            "execution spec digest",
        )?;
        require(
            self.expected_outcome.id.as_str() == "aq.expected-outcome"
                && self.expected_outcome.version.as_str() == "1",
            "outcome contract reference",
        )?;
        require(
            self.result.record_receipt.id.as_str()
                == format!("aq.result-record/{}", self.result.report_id)
                && self.result.record_receipt.version.as_str() == "1",
            "result record v1 reference",
        )?;
        require(
            self.result.stopped_receipt.id.as_str()
                == format!("aq.native-stopped/{}", self.binding.attempt_id)
                && self.result.stopped_receipt.version.as_str() == "1",
            "stopped owner reference",
        )?;
        require(
            (1..=32).contains(&self.criteria.len()),
            "criteria cardinality",
        )?;
        let mut seen = BTreeSet::new();
        for criterion in &self.criteria {
            criterion
                .validate()
                .map_err(|_| Invalid("criterion identity"))?;
            require(seen.insert(criterion.as_str()), "duplicate criterion")?;
        }
        canonical_bytes(self)?;
        Ok(())
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.validate()?;
        content_artifact(
            self,
            format!("aq.review-subject/{}", self.result.report_id),
            "subject identity",
            "subject version",
        )
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Approve,
    Reject,
}
record!(DecisionRequest {schema_version:u32,operation_id:Uuid,decision_id:Uuid,subject:ArtifactRef,eligible_owners:Vec<Uuid>,eligibility_project_revision:u64,review_policy:ArtifactRef,expires_at_ms:u64});
impl DecisionRequest {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == SCHEMA_VERSION, "decision schema")?;
        id(self.operation_id)?;
        id(self.decision_id)?;
        artifact(&self.subject)?;
        artifact(&self.review_policy)?;
        revision(self.eligibility_project_revision)?;
        revision(self.expires_at_ms)?;
        require(
            (1..=16).contains(&self.eligible_owners.len()),
            "eligible owner cardinality",
        )?;
        let mut seen = BTreeSet::new();
        for owner in &self.eligible_owners {
            id(*owner)?;
            require(seen.insert(*owner), "duplicate eligible owner")?;
        }
        canonical_bytes(self)?;
        Ok(())
    }
}
record!(ResolveRequest {schema_version:u32,operation_id:Uuid,decision_id:Uuid,expected_decision_revision:u64,choice:Choice,comment:Option<String>});
impl ResolveRequest {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == SCHEMA_VERSION, "answer schema")?;
        id(self.operation_id)?;
        id(self.decision_id)?;
        revision(self.expected_decision_revision)?;
        if let Some(comment) = &self.comment {
            require(
                !comment.trim().is_empty()
                    && comment.len() <= 2048
                    && !comment
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
                "comment bound",
            )?;
        }
        canonical_bytes(self)?;
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionFinality {
    HumanAnswer {
        owner_user_id: Uuid,
        operation_id: Uuid,
        choice: Choice,
        comment_digest: Option<Digest>,
    },
    Expired {},
    Cancelled {
        operation_id: Uuid,
    },
}
record!(DecisionReceipt {
    schema_version: u32,
    decision_id: Uuid,
    decision_revision: u64,
    subject: ArtifactRef,
    request_digest: Digest,
    eligibility_project_revision: u64,
    review_policy: ArtifactRef,
    recorded_at_ms: u64,
    finality: DecisionFinality
});
impl DecisionReceipt {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == SCHEMA_VERSION, "receipt schema")?;
        id(self.decision_id)?;
        revision(self.decision_revision)?;
        require(
            self.decision_revision == 2,
            "initial final receipt revision",
        )?;
        artifact(&self.subject)?;
        artifact(&self.review_policy)?;
        revision(self.eligibility_project_revision)?;
        revision(self.recorded_at_ms)?;
        match &self.finality {
            DecisionFinality::HumanAnswer {
                owner_user_id,
                operation_id,
                ..
            } => {
                id(*owner_user_id)?;
                id(*operation_id)?;
            }
            DecisionFinality::Cancelled { operation_id } => id(*operation_id)?,
            DecisionFinality::Expired {} => {}
        }
        canonical_bytes(self)?;
        Ok(())
    }
    pub fn validate_for(&self, request: &DecisionRequest) -> Result<()> {
        self.validate()?;
        request.validate()?;
        require(
            self.decision_id == request.decision_id
                && self.subject == request.subject
                && self.request_digest == digest(request)?
                && self.eligibility_project_revision == request.eligibility_project_revision
                && self.review_policy == request.review_policy,
            "receipt differs from decision request",
        )?;
        if let DecisionFinality::HumanAnswer { owner_user_id, .. } = self.finality {
            require(
                request.eligible_owners.contains(&owner_user_id),
                "answer owner not originally eligible",
            )?;
            require(
                self.recorded_at_ms < request.expires_at_ms,
                "answer after deadline",
            )?;
        }
        if matches!(self.finality, DecisionFinality::Expired {}) {
            require(
                self.recorded_at_ms >= request.expires_at_ms,
                "expiry before deadline",
            )?;
        }
        Ok(())
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.validate()?;
        content_artifact(
            self,
            format!(
                "aq.human-decision/{}/{}",
                self.decision_id, self.decision_revision
            ),
            "receipt identity",
            "receipt version",
        )
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckDisposition {
    Passed,
    Failed,
    Cancelled,
    TimedOut,
    Unknown,
    Stale,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutorEvidence {
    InProcessReadOnly {
        definition: ArtifactRef,
        #[schemars(length(min = 1, max = 64))]
        checked_artifacts: Vec<ArtifactRef>,
        result_artifact: ArtifactRef,
    },
    StoppedProcess {
        definition: ArtifactRef,
        binding: Box<LaunchBinding>,
        started_receipt: ArtifactRef,
        stopped_receipt: ArtifactRef,
        #[schemars(length(max = 16))]
        result_artifacts: Vec<ArtifactRef>,
        exit_code: i32,
    },
    Incomplete {
        definition: ArtifactRef,
        #[schemars(length(max = 16))]
        evidence: Vec<ArtifactRef>,
    },
}
record!(CheckReceipt {
    schema_version: u32,
    operation_id: Uuid,
    check_id: Text,
    subject: ArtifactRef,
    validator: ArtifactRef,
    recorded_at_ms: u64,
    disposition: CheckDisposition,
    executor: ExecutorEvidence
});
fn validate_checked_inputs(checked_artifacts: &[ArtifactRef]) -> Result<()> {
    require(
        (1..=64).contains(&checked_artifacts.len()),
        "checked input cardinality",
    )?;
    let mut ids = BTreeSet::new();
    for a in checked_artifacts {
        artifact(a)?;
        require(
            ids.insert((&a.id, &a.version)),
            "duplicate checked artifact identity",
        )?;
    }
    Ok(())
}

impl ExecutorEvidence {
    fn validate(&self, validator: &ArtifactRef, disposition: CheckDisposition) -> Result<()> {
        let definition = match self {
            Self::InProcessReadOnly { definition, .. }
            | Self::StoppedProcess { definition, .. }
            | Self::Incomplete { definition, .. } => definition,
        };
        artifact(definition)?;
        require(
            definition == validator,
            "executor definition differs from validator",
        )?;
        match self {
            ExecutorEvidence::InProcessReadOnly {
                checked_artifacts,
                result_artifact,
                ..
            } => {
                artifact(result_artifact)?;
                validate_checked_inputs(checked_artifacts)?;
            }
            ExecutorEvidence::StoppedProcess {
                binding,
                started_receipt,
                stopped_receipt,
                result_artifacts,
                exit_code,
                ..
            } => {
                binding
                    .validate()
                    .map_err(|_| Invalid("check process binding"))?;
                artifact(started_receipt)?;
                artifact(stopped_receipt)?;
                require(result_artifacts.len() <= 16, "check result refs bound")?;
                for a in result_artifacts {
                    artifact(a)?;
                }
                if disposition == CheckDisposition::Passed {
                    require(*exit_code == 0, "passed process requires actual zero exit")?;
                }
            }
            ExecutorEvidence::Incomplete { evidence, .. } => {
                require(evidence.len() <= 16, "incomplete refs bound")?;
                for a in evidence {
                    artifact(a)?;
                }
                require(
                    matches!(
                        disposition,
                        CheckDisposition::Unknown
                            | CheckDisposition::TimedOut
                            | CheckDisposition::Cancelled
                            | CheckDisposition::Stale
                    ),
                    "incomplete evidence cannot pass or fail as executed",
                )?;
            }
        }
        Ok(())
    }
}

impl CheckReceipt {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == SCHEMA_VERSION, "check schema")?;
        id(self.operation_id)?;
        self.check_id
            .validate()
            .map_err(|_| Invalid("check identity"))?;
        artifact(&self.subject)?;
        artifact(&self.validator)?;
        revision(self.recorded_at_ms)?;
        self.executor.validate(&self.validator, self.disposition)?;
        canonical_bytes(self)?;
        Ok(())
    }
    pub fn validate_for_subject(&self, subject: &ReviewSubject) -> Result<()> {
        self.validate()?;
        subject.validate()?;
        require(
            self.subject == subject.artifact()?,
            "check differs from exact subject",
        )
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.validate()?;
        content_artifact(
            self,
            format!("aq.check/{}/{}", self.check_id.as_str(), self.operation_id),
            "check receipt identity",
            "check receipt version",
        )
    }
}
pub fn subject_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(ReviewSubject)).expect("subject schema");
    schema["$comment"] = serde_json::json!(
        "Shape claims only. Requires actual B1 report/receipt/outcome and retained source owner resolution; no eligibility, human or source authority is conveyed by wire fields/digests."
    );
    schema
}
pub fn decision_schema() -> serde_json::Value {
    serde_json::json!({"request":schemars::schema_for!(DecisionRequest),"resolve":schemars::schema_for!(ResolveRequest),"receipt":schemars::schema_for!(DecisionReceipt)})
}
pub fn check_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(CheckReceipt)).expect("check schema");
    schema["$comment"] = serde_json::json!(
        "Executor evidence is a claim until resolved through the actual registered check owner. No passed:true endpoint or model proof is implemented."
    );
    schema
}
