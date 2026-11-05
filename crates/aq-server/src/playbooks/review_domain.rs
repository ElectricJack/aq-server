//! Explicit catalog2 addition. Catalog1 functions/fixture bytes remain fixed.
use super::domain::{Error, Recipient, Result};
use aq_playbook_host::{model::*, runner::Authority};
use aq_session_protocol::{ArtifactRef, Validate};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogProfile {
    Mail1,
    HumanReview2,
    /// HumanReview2 plus the confirmed graph-operation event.
    GraphEvents3,
    /// GraphEvents3 plus confirmed reviewed-task settlement events.
    SettlementEvents4,
    /// D4 host catalog 1.0.0, vendored and fingerprint-pinned (D5), numbered 5 after the main-line event profiles 3 and 4.
    Host5,
    /// D8 findings and daily occurrences; old catalog profiles remain frozen.
    Monitoring6,
    /// D6: AQ host catalog 2.0.0 (supersedes 1.0.0), vendored and
    /// fingerprint-pinned for a new namespace/generation, numbered 7 after the
    /// main-line D8 profile 6. Adds the
    /// asynchronous judgment.request.v2/check.request.v2 and operation.get.
    /// D11 registers all twelve catalog commands (adds findings.reconcile,
    /// message.send and decision.request).
    Host7,
    /// AQ host catalog 2.1.0 (supersedes 2.0.0), vendored and
    /// fingerprint-pinned beside the untouched Host7 pin. Same capabilities as
    /// Host7; candidate.prepare (2.1.0) outputs the prepared `source` OID that
    /// policies bind check.request `source` to.
    Host8,
}
pub const HOST8_VERSION: &str = "2.1.0";
pub const HOST8_FINGERPRINT: &str =
    "sha256:24407a41481dba1bc9618e4c9239e0282083cca2ef57316a42d5f61df32d1716";
fn host8_registry() -> Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let registry: Registry = serde_json::from_str(include_str!(
                "../../../../vendor/aq-host-catalog-2.1.0/registry.json"
            ))
            .expect("vendored AQ host catalog 2.1.0 parses");
            assert_eq!(
                aq_playbook_host::model::digest(&(HOST8_VERSION, &registry))
                    .ok()
                    .as_deref(),
                Some(HOST8_FINGERPRINT),
                "vendored AQ host catalog 2.1.0 fingerprint"
            );
            registry
        })
        .clone()
}
pub const HOST7_VERSION: &str = "2.0.0";
pub const HOST7_FINGERPRINT: &str =
    "sha256:e2ce7dc8ff4b37538f8d27f175a4e365ff95cf14508ae7074beea04026b143db";
pub const HOST7_CAPABILITIES: [&str; 12] = [
    "context.read",
    "work.ensure",
    "work.change",
    "operation.cancel",
    "operation.get",
    "judgment.request.v2",
    "check.request.v2",
    "candidate.prepare",
    "candidate.publish",
    "findings.reconcile",
    "message.send",
    "decision.request",
];
fn host7_registry() -> Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let registry: Registry = serde_json::from_str(include_str!(
                "../../../../vendor/aq-host-catalog-2.0.0/registry.json"
            ))
            .expect("vendored AQ host catalog 2.0.0 parses");
            assert_eq!(
                aq_playbook_host::model::digest(&(HOST7_VERSION, &registry))
                    .ok()
                    .as_deref(),
                Some(HOST7_FINGERPRINT),
                "vendored AQ host catalog 2.0.0 fingerprint"
            );
            registry
        })
        .clone()
}
pub const HOST5_VERSION: &str = "1.0.0";
pub const HOST5_FINGERPRINT: &str =
    "sha256:627125cc393d8274bfaa9ffc785c338e566cc69af362ac027a3daa355c3509e1";
pub const HOST5_CAPABILITIES: [&str; 4] = [
    "context.read",
    "work.ensure",
    "work.change",
    "operation.cancel",
];
fn host5_registry() -> Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let registry: Registry = serde_json::from_str(include_str!(
                "../../../../vendor/aq-host-catalog-1.0.0/registry.json"
            ))
            .expect("vendored D4 host catalog parses");
            assert_eq!(
                aq_playbook_host::model::digest(&(HOST5_VERSION, &registry))
                    .ok()
                    .as_deref(),
                Some(HOST5_FINGERPRINT),
                "vendored D4 host catalog fingerprint"
            );
            registry
        })
        .clone()
}
impl CatalogProfile {
    pub fn number(self) -> i16 {
        match self {
            Self::Mail1 => 1,
            Self::HumanReview2 => 2,
            Self::GraphEvents3 => 3,
            Self::SettlementEvents4 => 4,
            Self::Host5 => 5,
            Self::Monitoring6 => 6,
            Self::Host7 => 7,
            Self::Host8 => 8,
        }
    }
    /// Catalog 2.x host profiles (Host7 = 2.0.0, Host8 = 2.1.0): the
    /// asynchronous judgment/check, integration and operation.get adapters.
    pub fn is_host2(self) -> bool {
        matches!(self, Self::Host7 | Self::Host8)
    }
    pub fn has_reviews(self) -> bool {
        !matches!(
            self,
            Self::Mail1 | Self::Host5 | Self::Monitoring6 | Self::Host7 | Self::Host8
        )
    }
    pub fn registry(self) -> Registry {
        if self == Self::Monitoring6 {
            let mut registry = host5_registry();
            for event in [graph_operation_event(), task_completed_event()] {
                registry.events.insert(event.name.clone(), event);
            }
            return registry;
        }
        if self == Self::Host5 {
            return host5_registry();
        }
        if self == Self::Host7 {
            return host7_registry();
        }
        if self == Self::Host8 {
            return host8_registry();
        }
        let mut registry = super::domain::registry();
        if matches!(self, Self::GraphEvents3 | Self::SettlementEvents4) {
            let event = graph_operation_event();
            registry.events.insert(event.name.clone(), event);
        }
        if self == Self::SettlementEvents4 {
            let event = task_completed_event();
            registry.events.insert(event.name.clone(), event);
        }
        if self.has_reviews() {
            let decision = decision_contract();
            let operation = operation_contract();
            let event = decision_event();
            registry.commands.insert(decision.name.clone(), decision);
            registry.commands.insert(operation.name.clone(), operation);
            registry.events.insert(event.name.clone(), event);
        }
        registry
    }
    pub fn validate_scope(self, scope: &super::domain::GrantScope) -> Result<()> {
        if self == Self::Mail1 {
            return scope.validate();
        }
        if matches!(
            self,
            Self::Host5 | Self::Monitoring6 | Self::Host7 | Self::Host8
        ) {
            let mut allowed: BTreeSet<String> = if self.is_host2() {
                HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect()
            } else {
                HOST5_CAPABILITIES.iter().map(|c| c.to_string()).collect()
            };
            if self == Self::Monitoring6 {
                allowed.insert("findings.reconcile".into());
            }
            if scope.workspace_id.is_nil()
                || scope.project_id.is_nil()
                || !scope.recipients.is_empty()
                || scope.capabilities.is_empty()
                || !scope.capabilities.is_subset(&allowed)
                || aq_playbook_host::model::canonical_bytes(scope)
                    .map_err(|_| Error::Invalid)?
                    .len()
                    > 32768
            {
                return Err(Error::Invalid);
            }
            return Ok(());
        }
        let mut base = scope.clone();
        base.capabilities = BTreeSet::from(["message.send".into()]);
        base.validate()?;
        let allowed = BTreeSet::from([
            "message.send".into(),
            "decision.request".into(),
            "operation.get".into(),
        ]);
        if scope.capabilities.is_empty() || !scope.capabilities.is_subset(&allowed) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn authority(
        self,
        id: Uuid,
        revision: i64,
        scope: &super::domain::GrantScope,
    ) -> Result<Authority> {
        self.validate_scope(scope)?;
        let mut authority = super::domain::authority(id, revision);
        authority.capabilities = scope.capabilities.clone();
        Ok(authority)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionInput {
    pub recipient: Recipient,
    pub report_id: Uuid,
    pub source_operation_id: Uuid,
    pub expected_subject: ArtifactRef,
    pub eligible_owners: Vec<Uuid>,
    pub eligibility_project_revision: u64,
    pub expires_at_ms: u64,
}
impl DecisionInput {
    pub fn validate(&self) -> Result<()> {
        self.recipient.validate()?;
        self.expected_subject
            .id
            .validate()
            .map_err(|_| Error::Invalid)?;
        self.expected_subject
            .version
            .validate()
            .map_err(|_| Error::Invalid)?;
        if self.report_id.is_nil()
            || self.source_operation_id.is_nil()
            || self.expected_subject.id.as_str() != format!("aq.review-subject/{}", self.report_id)
            || self.expected_subject.version.as_str() != "1"
            || self.eligibility_project_revision == 0
            || self.eligibility_project_revision > i64::MAX as u64
            || self.expires_at_ms == 0
            || self.expires_at_ms > i64::MAX as u64
            || self.eligible_owners.is_empty()
            || self.eligible_owners.len() > 16
            || self.eligible_owners.iter().any(Uuid::is_nil)
            || self.eligible_owners.iter().collect::<BTreeSet<_>>().len()
                != self.eligible_owners.len()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
fn object(fields: impl IntoIterator<Item = (&'static str, Schema)>) -> Schema {
    Schema::object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn fixed(value: &str) -> Schema {
    Schema::String {
        max_length: 64,
        values: BTreeSet::from([value.into()]),
    }
}
fn artifact() -> Schema {
    object([
        ("id", Schema::string(1024)),
        ("version", Schema::string(1024)),
        ("digest", Schema::string(64)),
    ])
}
fn output() -> Schema {
    object([("receipt", artifact()), ("subject", artifact())])
}
fn outcome() -> Schema {
    Schema::String {
        max_length: 16,
        values: BTreeSet::from([
            "approved".into(),
            "rejected".into(),
            "expired".into(),
            "cancelled".into(),
        ]),
    }
}
fn completion() -> Schema {
    object([("outcome", outcome()), ("output", output())])
}
pub fn decision_contract() -> CommandContract {
    CommandContract {
        name: "decision.request".into(),
        version: "1".into(),
        input: object([
            ("recipient", super::domain::recipient_schema()),
            ("report_id", Schema::string(36)),
            ("source_operation_id", Schema::string(36)),
            ("expected_subject", artifact()),
            (
                "eligible_owners",
                Schema::Array {
                    items: Box::new(Schema::string(36)),
                    max_items: 16,
                },
            ),
            (
                "eligibility_project_revision",
                Schema::Integer {
                    minimum: 1,
                    maximum: i64::MAX,
                },
            ),
            (
                "expires_at_ms",
                Schema::Integer {
                    minimum: 1,
                    maximum: i64::MAX,
                },
            ),
        ]),
        outcomes: BTreeMap::from([
            ("accepted".into(), object([("handle", Schema::string(36))])),
            ("approved".into(), output()),
            ("rejected".into(), output()),
            ("expired".into(), output()),
            ("cancelled".into(), output()),
        ]),
        capabilities: BTreeSet::from(["decision.request".into()]),
        scopes: BTreeSet::from(["project".into()]),
        effect: Effect::Mutation,
        recovery: Recovery::Reconcilable,
        asynchronous: Some(AsyncContract {
            completion_event: "aq.decision.completed".into(),
            completion: completion(),
            correlation_field: "decision".into(),
            reconciliation_command: "operation.get".into(),
        }),
        cancellation: true,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::from(["eligible_owners".into()]),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65536,
        deadline_ms: 10000,
    }
}
pub fn operation_contract() -> CommandContract {
    CommandContract {
        name: "operation.get".into(),
        version: "1".into(),
        input: object([
            ("kind", fixed("human_decision")),
            ("handle", Schema::string(36)),
            ("recipient", super::domain::recipient_schema()),
        ]),
        outcomes: BTreeMap::from([
            (
                "pending".into(),
                object([("handle", Schema::string(36)), ("subject", artifact())]),
            ),
            ("approved".into(), output()),
            ("rejected".into(), output()),
            ("expired".into(), output()),
            ("cancelled".into(), output()),
        ]),
        capabilities: BTreeSet::from(["operation.get".into()]),
        scopes: BTreeSet::from(["project".into()]),
        effect: Effect::Observation,
        // Frozen host1.1 admits only reconcilable registrations. This is a
        // read-only retained owner lookup, conservatively reconciled by exact
        // handle rather than automatically retried as a generic observation.
        recovery: Recovery::Reconcilable,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65536,
        deadline_ms: 5000,
    }
}
pub const GRAPH_PRODUCER: &str = "aq.graph.operations@1";
pub const SETTLEMENT_PRODUCER: &str = "aq.task.settlement@1";
/// The exact retained completion outbox body, keyed by completion operation.
pub fn task_completed_event() -> EventContract {
    let revision = || Schema::Integer {
        minimum: 1,
        maximum: i64::MAX,
    };
    EventContract {
        name: "task.completed".into(),
        version: "1".into(),
        payload: object([
            ("event_id", Schema::string(36)),
            ("producer", fixed(SETTLEMENT_PRODUCER)),
            ("event", fixed("task.completed")),
            ("attempt_id", Schema::string(36)),
            ("evaluation_operation_id", Schema::string(36)),
            ("report", artifact()),
            ("release_id", Schema::string(36)),
            (
                "receipt",
                object([
                    ("workspace_id", Schema::string(36)),
                    ("task_id", Schema::string(36)),
                    ("change_set_id", Schema::string(36)),
                    ("version", revision()),
                    ("content_revision", revision()),
                    ("response_digest", Schema::string(64)),
                ]),
            ),
        ]),
        trusted_producers: BTreeSet::from([SETTLEMENT_PRODUCER.into()]),
        scopes: BTreeSet::from(["project".into()]),
        correlation_fields: BTreeSet::from(["operation".into()]),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 8192,
        retention_ms: 86400000,
        sensitive_payload_fields: BTreeSet::new(),
    }
}
/// One event per confirmed graph operation; `event_id` is the graph outbox id.
pub fn graph_operation_event() -> EventContract {
    EventContract {
        name: "aq.graph.operation.confirmed".into(),
        version: "1".into(),
        payload: object([
            ("operation_id", Schema::string(36)),
            ("task_id", Schema::string(36)),
            ("attempt_id", Schema::string(36)),
        ]),
        trusted_producers: BTreeSet::from([GRAPH_PRODUCER.into()]),
        scopes: BTreeSet::from(["project".into()]),
        correlation_fields: BTreeSet::from(["operation".into()]),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 16384,
        retention_ms: 86400000,
        sensitive_payload_fields: BTreeSet::new(),
    }
}
pub fn decision_event() -> EventContract {
    EventContract {
        name: "aq.decision.completed".into(),
        version: "1".into(),
        payload: completion(),
        trusted_producers: BTreeSet::from(["aq.human.decisions@1".into()]),
        scopes: BTreeSet::from(["project".into()]),
        correlation_fields: BTreeSet::from(["decision".into()]),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 16384,
        retention_ms: 86400000,
        sensitive_payload_fields: BTreeSet::new(),
    }
}
