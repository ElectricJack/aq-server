//! Wire payloads and versioned subjects for dashboard playbook actions.
use super::ActionRequest;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;
pub const RUN: &str = "playbook_run";
pub const BINDING: &str = "playbook_binding";

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunCancel {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(length(min = 1, max = 128))]
    pub run_id: String,
}

/// One scalar run input.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Input {
    Bool(bool),
    Number(serde_json::Number),
    Text(#[schemars(length(max = 4096))] String),
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunStart {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    /// The binding's activation revision, as shown in the catalog.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,19}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 128))]
    pub binding: String,
    /// The binding's currently active artifact digest.
    #[schemars(length(min = 1, max = 128))]
    pub artifact: String,
    #[schemars(length(min = 1, max = 128))]
    pub entry: String,
    #[serde(default)]
    #[schemars(extend("maxProperties" = 32))]
    pub inputs: BTreeMap<String, Input>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BindingChange {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    /// The binding's activation revision, as shown in the catalog.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,19}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 128))]
    pub binding: String,
    /// Activate: the artifact to make active. Rollback: the active artifact.
    #[schemars(length(min = 1, max = 128))]
    pub artifact: String,
}

fn id_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
pub fn version(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0 && v.to_string() == value)
}

fn binding_subject(request: &ActionRequest, binding: &str, expected: &str) -> bool {
    let binding_matches = request.subject.entity_type == BINDING
        && request.subject.id == binding
        && request.subject.revision.as_deref() == Some(expected);
    let version_matches = version(expected).is_some()
        && request.expected_versions == BTreeMap::from([(BINDING.into(), expected.into())]);
    id_ok(binding) && binding_matches && version_matches
}
impl RunCancel {
    pub fn matches(&self, request: &ActionRequest) -> bool {
        let run = request.subject.entity_type == RUN && request.subject.id == self.run_id;
        let unversioned =
            request.subject.revision.is_none() && request.expected_versions.is_empty();
        id_ok(&self.run_id) && run && unversioned
    }
}
impl RunStart {
    pub fn matches(&self, request: &ActionRequest) -> bool {
        let binding = binding_subject(request, &self.binding, &self.expected_version);
        let target = id_ok(&self.artifact) && id_ok(&self.entry);
        let inputs = self.inputs.len() <= 32 && self.inputs.keys().all(|key| id_ok(key));
        binding && target && inputs
    }
}
impl BindingChange {
    pub fn matches(&self, request: &ActionRequest) -> bool {
        binding_subject(request, &self.binding, &self.expected_version) && id_ok(&self.artifact)
    }
}
