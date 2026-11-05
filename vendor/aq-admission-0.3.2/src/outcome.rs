//! Pinned pre-execution expectations. These values are requirements, not proof
//! of source retention, executed checks, authenticated review or task completion.
use crate::*;
use execution_spec::ProfiledExecutionSpec;
use std::collections::BTreeSet;
pub const MAX_BYTES: usize = 65_536;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    SourceChangeReviewed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRequirement {
    HumanOwner,
    PolicyActor,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceAnchor {
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub base: ObjectId,
    pub artifact: ArtifactRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    pub id: Label,
    #[schemars(length(min = 1, max = 8192))]
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequiredCheck {
    pub id: Label,
    pub validator: ArtifactRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpectedOutcomeDefinition {
    pub schema_version: u32,
    pub kind: OutcomeKind,
    pub project_id: Uuid,
    pub project_revision: u64,
    pub task: TaskRevision,
    pub source: SourceAnchor,
    pub task_observation: ArtifactRef,
    pub task_context: ArtifactRef,
    #[schemars(length(max = 64))]
    pub inputs: Vec<ArtifactRef>,
    #[schemars(length(min = 1, max = 32))]
    pub acceptance_criteria: Vec<Criterion>,
    pub validation_policy: ArtifactRef,
    pub review: ReviewRequirement,
    #[schemars(length(max = 32))]
    pub required_checks: Vec<RequiredCheck>,
}
impl ExpectedOutcomeDefinition {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "outcome schema version")?;
        id(self.project_id)?;
        revision(self.project_revision)?;
        self.task.validate()?;
        id(self.source.repository_id)?;
        revision(self.source.binding_generation)?;
        self.source.base.validate()?;
        for reference in [
            &self.source.artifact,
            &self.task_observation,
            &self.task_context,
            &self.validation_policy,
        ] {
            artifact(reference)?;
        }
        require(
            self.inputs.len() <= 64
                && (1..=32).contains(&self.acceptance_criteria.len())
                && self.required_checks.len() <= 32,
            "outcome cardinality",
        )?;
        let mut inputs = BTreeSet::new();
        for input in &self.inputs {
            artifact(input)?;
            require(
                inputs.insert((&input.id, &input.version)),
                "duplicate outcome input identity",
            )?;
        }
        let mut criteria = BTreeSet::new();
        for criterion in &self.acceptance_criteria {
            require(
                criteria.insert(&criterion.id)
                    && !criterion.text.trim().is_empty()
                    && criterion.text.len() <= 8192
                    && !criterion
                        .text
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
                "outcome criterion identity/text",
            )?;
        }
        let mut checks = BTreeSet::new();
        for check in &self.required_checks {
            artifact(&check.validator)?;
            require(checks.insert(&check.id), "duplicate required check")?;
        }
        require(
            self.canonical_bytes()?.len() <= MAX_BYTES,
            "outcome canonical byte bound",
        )
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut value = serde_json::to_value(self).map_err(|_| Invalid("outcome encoding"))?;
        value.sort_all_objects();
        serde_json::to_vec(&value).map_err(|_| Invalid("outcome encoding"))
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.validate()?;
        Ok(ArtifactRef {
            id: Text::new("aq.expected-outcome").map_err(|_| Invalid("outcome identity"))?,
            version: Text::new("1").map_err(|_| Invalid("outcome version"))?,
            digest: Digest::of(&self.canonical_bytes()?),
        })
    }
    /// Exact acyclic spec linkage only; callers still load authorized retained
    /// bytes, pin policy and verify current input revisions when settling.
    pub fn validate_against(&self, spec: &ProfiledExecutionSpec) -> Result<()> {
        self.validate()?;
        spec.validate()?;
        require(
            self.project_id == spec.lifecycle.project_id
                && self.project_revision == spec.project_revision
                && self.task == spec.task
                && self.source.repository_id == spec.source.repository_id
                && self.source.binding_generation == spec.source.repository_binding_generation
                && self.source.base == spec.source.resolved_object
                && self.source.artifact == spec.source_artifact
                && self.task_observation == spec.task_observation
                && self.task_context == spec.task_context
                && self.artifact()? == spec.outcome_artifact,
            "outcome differs from pinned execution inputs",
        )
    }
}
pub fn decode(bytes: &[u8]) -> Result<ExpectedOutcomeDefinition> {
    require(bytes.len() <= MAX_BYTES, "outcome input byte bound")?;
    let value: ExpectedOutcomeDefinition =
        serde_json::from_slice(bytes).map_err(|_| Invalid("outcome JSON shape"))?;
    value.validate()?;
    Ok(value)
}
pub fn schema() -> serde_json::Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(ExpectedOutcomeDefinition))
        .expect("outcome schema");
    schema["$comment"] = serde_json::json!(
        "UTF8 JSON and canonical bytes <=65536. Byte bounds, distinct IDs and exact spec linkage are enforced by typed validation; schema alone conveys no authority. Review and checks are requirements, not evidence."
    );
    schema
}
