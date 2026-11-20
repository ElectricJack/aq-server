//! Wire contracts for owner knowledge actions.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    Fact,
    Decision,
    Policy,
    Procedure,
    Incident,
    Reference,
    Note,
}

#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Verified,
    Disputed,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeAuthor {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_project_revision: String,
    #[schemars(length(min = 1, max = 512))]
    pub title: String,
    #[schemars(length(max = 32768))]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 16))]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<ContentType>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeCorrect {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 512))]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32768))]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 16))]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<ContentType>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeVerify {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    pub verification: Verdict,
    /// Graph `Source` objects; each is validated against the pinned Graph schema.
    #[schemars(length(min = 1, max = 64))]
    pub evidence: Vec<serde_json::Map<String, Value>>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeRetire {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 4096))]
    pub retirement_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_id: Option<Uuid>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeRestore {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(range(min = 1))]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_lifecycle: Option<bool>,
}
