use super::super::contract::{Envelope, Scope};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EntityRef {
    #[serde(rename = "type")]
    #[schemars(length(min = 1, max = 64))]
    pub entity_type: String,
    #[schemars(length(min = 1, max = 128))]
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = "^[1-9][0-9]{0,39}$"))]
    pub revision: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub operation_id: Uuid,
    #[schemars(
        length(max = 128),
        regex(pattern = "^[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)+$")
    )]
    pub kind: String,
    pub subject: EntityRef,
    pub expected_versions: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub reason: Option<String>,
    pub payload: Value,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Pending,
    Confirmed,
    Conflict,
    Rejected,
    Unknown,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    LookupOriginal,
    RefreshAndReview,
    None,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub operation_id: Uuid,
    pub disposition: Disposition,
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub request_digest: String,
    pub scope: Scope,
    #[schemars(with = "super::super::contract::Timestamp")]
    pub created_at: String,
    #[schemars(with = "super::super::contract::Timestamp")]
    pub updated_at: String,
    #[schemars(length(max = 1))]
    pub subject_refs: Vec<EntityRef>,
    #[schemars(length(max = 100))]
    pub receipt_refs: Vec<EntityRef>,
    #[schemars(length(max = 100))]
    pub affected_entities: Vec<EntityRef>,
    pub recovery: Recovery,
    /// Live versions observed when the disposition is `conflict`; empty otherwise.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("maxProperties" = 32))]
    pub current_versions: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum ActionVersion {
    #[serde(rename = "dashboard-actions-1")]
    V1,
}
pub type OperationResponse = Envelope<Operation, ActionVersion>;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationRetention {
    pub minimum_days: u16,
    pub unresolved_indefinite: bool,
    pub retired_ids_indefinite: bool,
}
impl Default for OperationRetention {
    fn default() -> Self {
        Self {
            minimum_days: 90,
            unresolved_indefinite: true,
            retired_ids_indefinite: true,
        }
    }
}
pub(super) fn safe(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
pub(super) fn revision(value: &str) -> bool {
    value.len() <= 40
        && value
            .as_bytes()
            .first()
            .is_some_and(|b| matches!(b, b'1'..=b'9'))
        && value.bytes().all(|b| b.is_ascii_digit())
}
impl EntityRef {
    pub(super) fn valid(&self) -> bool {
        safe(&self.entity_type, 64)
            && safe(&self.id, 128)
            && self.revision.as_deref().is_none_or(revision)
    }
}
impl ActionRequest {
    pub(super) fn validate(&self) -> Result<(), super::ApiError> {
        self.validate_with_limit(65536)
    }
    pub(super) fn validate_with_limit(&self, max_bytes: usize) -> Result<(), super::ApiError> {
        if self.operation_id.is_nil()
            || !super::registry::valid_kind(&self.kind)
            || !self.subject.valid()
            || self.expected_versions.len() > 32
            || self
                .expected_versions
                .iter()
                .any(|(k, v)| !safe(k, 128) || !revision(v))
            || self.reason.as_deref().is_some_and(|v| !safe(v, 1024))
        {
            return Err(super::ApiError::invalid());
        }
        if serde_json::to_vec(self)
            .map_err(|_| super::ApiError::invalid())?
            .len()
            > max_bytes
        {
            return Err(super::ApiError::for_status(
                super::StatusCode::PAYLOAD_TOO_LARGE,
            ));
        }
        Ok(())
    }
}
/// Domain adapters report only evidence they own; a transport failure is Unknown.
#[derive(Debug)]
pub struct Outcome {
    pub disposition: Disposition,
    pub receipt_refs: Vec<EntityRef>,
    pub affected_entities: Vec<EntityRef>,
    /// Conflict evidence only: the live versions a caller must refresh to.
    pub current_versions: BTreeMap<String, String>,
}
impl Outcome {
    pub fn unknown() -> Self {
        Self {
            disposition: Disposition::Unknown,
            receipt_refs: vec![],
            affected_entities: vec![],
            current_versions: BTreeMap::new(),
        }
    }
    pub fn conflict(current_versions: BTreeMap<String, String>) -> Self {
        Self {
            disposition: Disposition::Conflict,
            receipt_refs: vec![],
            affected_entities: vec![],
            current_versions,
        }
    }
    pub fn confirmed(receipt_refs: Vec<EntityRef>, affected_entities: Vec<EntityRef>) -> Self {
        Self {
            disposition: Disposition::Confirmed,
            receipt_refs,
            affected_entities,
            current_versions: BTreeMap::new(),
        }
    }
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum Wire {
    Request(ActionRequest),
    Operation(Box<OperationResponse>),
    Error(super::super::errors::ErrorEnvelope),
}
pub fn schema(registry: &super::Registry) -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(Wire)).expect("static schema");
    let request = &mut schema["$defs"]["ActionRequest"];
    request["properties"]["expected_versions"]["maxProperties"] = serde_json::json!(32);
    request["properties"]["expected_versions"]["propertyNames"] =
        serde_json::json!({"minLength":1,"maxLength":128});
    request["properties"]["expected_versions"]["additionalProperties"] =
        serde_json::json!({"type":"string","pattern":"^[1-9][0-9]{0,39}$"});
    if registry.schemas().is_empty() {
        request["not"] = serde_json::json!({});
    } else {
        request["oneOf"]=Value::Array(registry.schemas().iter().map(|(kind,payload)| {
            let mut payload=payload.clone();
            payload["$id"]=Value::String(format!("urn:aq:dashboard-actions:1:{kind}:payload"));
            serde_json::json!({"properties":{"kind":{"const":kind},"payload":payload},"required":["kind","payload"]})
        }).collect());
    }
    schema["x-capability-version"] = serde_json::json!(1);
    schema
}
pub fn schema_bytes() -> Vec<u8> {
    let mut bytes =
        serde_json::to_vec_pretty(&schema(&super::existing::schemas())).expect("static schema");
    bytes.push(b'\n');
    bytes
}
