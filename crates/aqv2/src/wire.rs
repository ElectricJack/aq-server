//! Read-only DTOs for the pinned dashboard-actions-1 envelope. Unknown fields
//! remain compatible; missing required evidence is never treated as success.
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct Envelope {
    pub api_version: String,
    pub request_id: Uuid,
    pub server_time: String,
    pub scope: Value,
    pub data: Operation,
    pub observation: Value,
    pub pending_operations: Vec<Uuid>,
    pub coverage: Value,
}
#[derive(Deserialize)]
pub struct Operation {
    pub operation_id: Uuid,
    pub disposition: Disposition,
    pub request_digest: String,
    pub scope: Value,
    pub created_at: String,
    pub updated_at: String,
    pub subject_refs: Vec<EntityRef>,
    pub receipt_refs: Vec<EntityRef>,
    pub affected_entities: Vec<EntityRef>,
    pub recovery: Recovery,
}
#[derive(Deserialize)]
pub struct EntityRef {
    #[serde(rename = "type")]
    pub entity_type: String,
    pub id: String,
    pub revision: Option<String>,
}
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Pending,
    Confirmed,
    Conflict,
    Rejected,
    Unknown,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    LookupOriginal,
    RefreshAndReview,
    None,
}
