//! dashboard-actions-1 request, operation and outcome types.
pub mod playbooks;
pub mod receipts;
use super::{Envelope, Scope};
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivationCounts {
    pub activated: u32,
    pub skipped: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub operation_id: Uuid,
    pub disposition: Disposition,
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub request_digest: String,
    pub scope: Scope,
    #[schemars(with = "super::Timestamp")]
    pub created_at: String,
    #[schemars(with = "super::Timestamp")]
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
    /// Why a `rejected` operation was refused, when its domain owner names a reason (absent
    /// otherwise). `busy`: the owner's rows were contended, nothing was changed, and a new
    /// submission may succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64), regex(pattern = "^[a-z][a-z0-9_]*$"))]
    pub reason: Option<String>,
    /// Counts for a confirmed `task.activate.subtree` operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<ActivationCounts>,
}
impl Operation {
    /// Apply a bounded, internally consistent domain outcome to the admitted receipt.
    pub fn with_outcome(
        &self,
        outcome: Outcome,
        timestamp: String,
        valid_reason: impl Fn(&str) -> bool,
    ) -> Option<Self> {
        let mut completed = self.clone();
        if outcome.receipt_refs.len() <= 100
            && outcome.affected_entities.len() <= 100
            && outcome
                .receipt_refs
                .iter()
                .chain(&outcome.affected_entities)
                .all(EntityRef::valid)
            && (outcome.disposition == Disposition::Confirmed
                || (outcome.receipt_refs.is_empty() && outcome.affected_entities.is_empty()))
            && (outcome.current_versions.is_empty()
                || (outcome.disposition == Disposition::Conflict
                    && outcome.current_versions.len() <= 32
                    && outcome
                        .current_versions
                        .iter()
                        .all(|(k, v)| safe(k, 128) && revision(v))))
            && outcome
                .reason
                .is_none_or(|r| outcome.disposition == Disposition::Rejected && valid_reason(r))
            && outcome.activation.is_none_or(|counts| {
                outcome.disposition == Disposition::Confirmed
                    && counts.activated as usize == outcome.affected_entities.len()
            })
        {
            completed.disposition = outcome.disposition;
            completed.receipt_refs = outcome.receipt_refs;
            completed.affected_entities = outcome.affected_entities;
            completed.current_versions = outcome.current_versions;
            completed.reason = outcome.reason.map(str::to_owned);
            completed.activation = outcome.activation;
        } else {
            completed.disposition = Disposition::Unknown;
        }
        if completed.disposition == self.disposition
            && completed.receipt_refs == self.receipt_refs
            && completed.affected_entities == self.affected_entities
            && completed.current_versions == self.current_versions
            && completed.reason == self.reason
            && completed.activation == self.activation
        {
            return None;
        }
        completed.recovery = match completed.disposition {
            Disposition::Pending | Disposition::Unknown => Recovery::LookupOriginal,
            Disposition::Conflict | Disposition::Rejected => Recovery::RefreshAndReview,
            Disposition::Confirmed => Recovery::None,
        };
        completed.updated_at = timestamp.max(self.updated_at.clone());
        if serde_json::to_vec(&completed).map_or(true, |v| v.len() > 60000) {
            completed = self.clone();
            completed.disposition = Disposition::Unknown;
        }
        Some(completed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum ActionVersion {
    #[serde(rename = "dashboard-actions-1")]
    V1,
}
pub type OperationResponse = Envelope<Operation, ActionVersion>;
pub fn safe(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
pub fn revision(value: &str) -> bool {
    value.len() <= 40
        && value
            .as_bytes()
            .first()
            .is_some_and(|b| matches!(b, b'1'..=b'9'))
        && value.bytes().all(|b| b.is_ascii_digit())
}
impl EntityRef {
    pub fn valid(&self) -> bool {
        safe(&self.entity_type, 64)
            && safe(&self.id, 128)
            && self.revision.as_deref().is_none_or(revision)
    }
}
/// Why an `ActionRequest` is refused before authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    Invalid,
    TooLarge,
}
impl ActionRequest {
    fn valid_identity(&self) -> bool {
        !self.operation_id.is_nil() && valid_kind(&self.kind) && self.subject.valid()
    }
    fn valid_versions(&self) -> bool {
        self.expected_versions.len() <= 32
            && self
                .expected_versions
                .iter()
                .all(|(key, value)| safe(key, 128) && revision(value))
    }
    fn valid_fields(&self) -> bool {
        let valid_reason = self
            .reason
            .as_deref()
            .is_none_or(|reason| safe(reason, 1024));
        self.valid_identity() && self.valid_versions() && valid_reason
    }
    pub fn check(&self, max_bytes: usize) -> Result<(), RequestError> {
        if !self.valid_fields() {
            return Err(RequestError::Invalid);
        }
        let len = serde_json::to_vec(self)
            .map_err(|_| RequestError::Invalid)?
            .len();
        if len > max_bytes {
            return Err(RequestError::TooLarge);
        }
        Ok(())
    }
}
pub fn valid_kind(kind: &str) -> bool {
    kind.len() <= 128
        && kind.contains('.')
        && kind.split('.').all(|part| {
            part.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}
/// Domain adapters report only evidence they own; a transport failure is Unknown.
#[derive(Debug)]
pub struct Outcome {
    pub disposition: Disposition,
    pub receipt_refs: Vec<EntityRef>,
    pub affected_entities: Vec<EntityRef>,
    /// Conflict evidence only: the live versions a caller must refresh to.
    pub current_versions: BTreeMap<String, String>,
    /// Rejection only: the reason code the operation records (see [`Operation::reason`]).
    pub reason: Option<&'static str>,
    pub activation: Option<ActivationCounts>,
}
impl Outcome {
    pub fn pending() -> Self {
        Self {
            disposition: Disposition::Pending,
            ..Self::unknown()
        }
    }
    pub fn unknown() -> Self {
        Self {
            disposition: Disposition::Unknown,
            receipt_refs: vec![],
            affected_entities: vec![],
            current_versions: BTreeMap::new(),
            reason: None,
            activation: None,
        }
    }
    pub fn conflict(current_versions: BTreeMap<String, String>) -> Self {
        Self {
            disposition: Disposition::Conflict,
            current_versions,
            ..Self::unknown()
        }
    }
    /// A definite refusal that changed nothing, with its reason code.
    pub fn refused(reason: &'static str) -> Self {
        Self {
            disposition: Disposition::Rejected,
            reason: Some(reason),
            ..Self::unknown()
        }
    }
    pub fn confirmed(receipt_refs: Vec<EntityRef>, affected_entities: Vec<EntityRef>) -> Self {
        Self {
            disposition: Disposition::Confirmed,
            receipt_refs,
            affected_entities,
            ..Self::unknown()
        }
    }
}
