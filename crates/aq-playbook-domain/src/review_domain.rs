//! Explicit catalog2 addition. Catalog1 functions/fixture bytes remain fixed.
use crate::{Error, Recipient, Result};
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
    /// AQ host catalog 2.2.0 (supersedes 2.1.0; adds contracts only), vendored
    /// and fingerprint-pinned beside the untouched Host8 pin: Host8 plus
    /// `work.read`, `work.route`, `work.unroutable` and `judgment.assess`.
    Host9,
    /// AQ host catalog 2.3.0 (Epic 0, spec 0016 section 6; supersedes 2.2.0),
    /// vendored and fingerprint-pinned beside the untouched Host9 pin: Host9's
    /// contracts, `work.read` with the facts of the task's latest result, the
    /// event `aq.work.result_ready` and the delivery commands. Compiled in only
    /// with the cargo feature `delivery_playbook`.
    #[cfg(feature = "delivery_playbook")]
    Host10,
}
pub const HOST9_VERSION: &str = "2.2.0";
pub const HOST9_FINGERPRINT: &str =
    "sha256:ca3c3d68959d760cc761d0c39b8db8740a27b3e414ae54e76d64df944f9b677a";
/// `HOST7_CAPABILITIES` plus the four catalog 2.2.0 commands.
pub const HOST9_CAPABILITIES: [&str; 16] = [
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
    "work.read",
    "work.route",
    "work.unroutable",
    "judgment.assess",
];
pub const HOST5_COMMANDS: [&str; 5] = [
    "context.read",
    "work.ensure",
    "work.change",
    "operation.cancel",
    "findings.reconcile",
];
/// Catalog 2.0.0 adds asynchronous judgments/checks and operation.get. D11
/// registers every catalog command: exactly the registry's twelve.
pub const HOST7_COMMANDS: [&str; 12] = [
    "context.read",
    "work.ensure",
    "work.change",
    "operation.cancel",
    "judgment.request",
    "check.request",
    "operation.get",
    "candidate.prepare",
    "candidate.publish",
    "findings.reconcile",
    "message.send",
    "decision.request",
];
/// The commands a Host9 namespace serves: the catalog's sixteen.
pub const HOST9_COMMANDS: [&str; 16] = [
    "context.read",
    "work.ensure",
    "work.change",
    "operation.cancel",
    "judgment.request",
    "check.request",
    "operation.get",
    "candidate.prepare",
    "candidate.publish",
    "findings.reconcile",
    "message.send",
    "decision.request",
    "work.read",
    "work.route",
    "work.unroutable",
    "judgment.assess",
];
fn host9_registry() -> Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let registry: Registry = serde_json::from_str(include_str!(
                "../../../vendor/aq-host-catalog-2.2.0/registry.json"
            ))
            .expect("vendored AQ host catalog 2.2.0 parses");
            assert_eq!(
                aq_playbook_host::model::digest(&(HOST9_VERSION, &registry))
                    .ok()
                    .as_deref(),
                Some(HOST9_FINGERPRINT),
                "vendored AQ host catalog 2.2.0 fingerprint"
            );
            registry
        })
        .clone()
}
/// AQ host catalog 2.3.0.
#[cfg(feature = "delivery_playbook")]
pub const HOST10_VERSION: &str = "2.3.0";
#[cfg(feature = "delivery_playbook")]
pub const HOST10_FINGERPRINT: &str =
    "sha256:0bda7d5de41b0630ec46a4b9cf555609db59a7276459e89620c5771deeced89a";
/// What a Host10 namespace admits in a grant: the sixteen capabilities of
/// Host9. The ten delivery commands of the catalog (`work.complete`,
/// `work.return`, ...) are served, and their capabilities admitted, by the
/// tasks that implement them; this profile serves `work.read` 2.3.0 and the
/// event `aq.work.result_ready`.
#[cfg(feature = "delivery_playbook")]
pub const HOST10_CAPABILITIES: [&str; 16] = HOST9_CAPABILITIES;
/// The capabilities of the ten delivery commands of catalog 2.3.0. A Host10
/// grant may name them, so that the built-in `delivery` source (spec 0016 §8)
/// can hold a grant for the playbook its document is authored into; whether a
/// command is served is the business of the task that implements it.
#[cfg(feature = "delivery_playbook")]
pub const HOST10_DELIVERY_CAPABILITIES: [&str; 10] = [
    "work.complete",
    "work.return",
    "work.hold",
    "supervisor.notify",
    "main.sync",
    "branch.publish",
    "pull_request.open",
    "pull_request.await_checks",
    "pull_request.merge",
    "pull_request.close",
];
/// The commands a Host10 namespace serves (see [`HOST10_CAPABILITIES`]).
#[cfg(feature = "delivery_playbook")]
pub const HOST10_COMMANDS: [&str; 16] = HOST9_COMMANDS;
#[cfg(feature = "delivery_playbook")]
fn host10_registry() -> Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let registry: Registry = serde_json::from_str(include_str!(
                "../../../vendor/aq-host-catalog-2.3.0/registry.json"
            ))
            .expect("vendored AQ host catalog 2.3.0 parses");
            assert_eq!(
                aq_playbook_host::model::digest(&(HOST10_VERSION, &registry))
                    .ok()
                    .as_deref(),
                Some(HOST10_FINGERPRINT),
                "vendored AQ host catalog 2.3.0 fingerprint"
            );
            registry
        })
        .clone()
}
pub const HOST8_VERSION: &str = "2.1.0";
pub const HOST8_FINGERPRINT: &str =
    "sha256:24407a41481dba1bc9618e4c9239e0282083cca2ef57316a42d5f61df32d1716";
fn host8_registry() -> Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let registry: Registry = serde_json::from_str(include_str!(
                "../../../vendor/aq-host-catalog-2.1.0/registry.json"
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
                "../../../vendor/aq-host-catalog-2.0.0/registry.json"
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
                "../../../vendor/aq-host-catalog-1.0.0/registry.json"
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
            Self::Host9 => 9,
            #[cfg(feature = "delivery_playbook")]
            Self::Host10 => 10,
        }
    }
    /// Catalog 2.3.0 (Epic 0): always false without the feature
    /// `delivery_playbook`.
    pub fn is_delivery(self) -> bool {
        #[cfg(feature = "delivery_playbook")]
        {
            self == Self::Host10
        }
        #[cfg(not(feature = "delivery_playbook"))]
        {
            false
        }
    }
    /// Catalog 2.2.0 and later: `work.read`, `work.route`, `work.unroutable`,
    /// `judgment.assess` and the event `aq.work.route_needed`.
    pub fn has_routing(self) -> bool {
        self == Self::Host9 || self.is_delivery()
    }
    /// Catalog 2.x host profiles (Host7 = 2.0.0, Host8 = 2.1.0, Host9 =
    /// 2.2.0): the asynchronous judgment/check, integration and
    /// operation.get adapters.
    pub fn is_host2(self) -> bool {
        matches!(self, Self::Host7 | Self::Host8 | Self::Host9) || self.is_delivery()
    }
    /// Any vendored host catalog profile (1.0.0 and later) or Monitoring6:
    /// the profiles the host adapters serve.
    pub fn is_host(self) -> bool {
        self.is_host2() || matches!(self, Self::Host5 | Self::Monitoring6)
    }
    /// Catalog 2.1.0 and later: `candidate.prepare` outputs the prepared
    /// source that `check.request` must name.
    pub fn has_prepared_source(self) -> bool {
        matches!(self, Self::Host8 | Self::Host9) || self.is_delivery()
    }
    /// D8 findings: Monitoring6 and every catalog 2.x profile.
    pub fn has_findings(self) -> bool {
        self.is_host2() || self == Self::Monitoring6
    }
    pub fn from_number(number: i16) -> Option<Self> {
        #[cfg(feature = "delivery_playbook")]
        let delivery = [Self::Host10];
        #[cfg(not(feature = "delivery_playbook"))]
        let delivery: [Self; 0] = [];
        [
            Self::Mail1,
            Self::HumanReview2,
            Self::GraphEvents3,
            Self::SettlementEvents4,
            Self::Host5,
            Self::Monitoring6,
            Self::Host7,
            Self::Host8,
            Self::Host9,
        ]
        .into_iter()
        .chain(delivery)
        .find(|p| p.number() == number)
    }
    /// The `[playbooks] profile` value that configures this profile.
    pub fn config_name(self) -> &'static str {
        match self {
            Self::Mail1 => "mail_v1 (not configurable)",
            Self::HumanReview2 => "human_review_v2",
            Self::GraphEvents3 => "graph_events_v3",
            Self::SettlementEvents4 => "settlement_events_v4",
            Self::Host5 => "host_v5 (not configurable)",
            Self::Monitoring6 => "monitoring_v6",
            Self::Host7 => "host_v7",
            Self::Host8 => "host_v8",
            Self::Host9 => "host_v9",
            #[cfg(feature = "delivery_playbook")]
            Self::Host10 => "host_v10",
        }
    }
    /// Why a configured namespace cannot open over the stored row
    /// `(profile number, generation)`, and what the operator does. A
    /// namespace's profile and generation never change (trigger
    /// `playbook_catalog_immutable`), from any profile: a different profile
    /// is a new namespace.
    pub fn namespace_refusal(self, generation: i64, stored: Option<(i16, i64)>) -> Option<String> {
        let (number, stored_generation) = stored?;
        if number == self.number() && stored_generation == generation {
            return None;
        }
        let found = Self::from_number(number).map_or("an unknown profile", Self::config_name);
        Some(format!(
            "this namespace already exists with profile \"{found}\" (generation {stored_generation}); \
             the configuration asks for profile \"{want}\" (generation {generation}). A namespace never \
             changes its profile or generation. To use \"{want}\": stop the server, give \
             [playbooks.namespace] a new namespace_id and a new name with generation=1, keep \
             profile=\"{want}\", and start again; artifacts are installed and activated again in the \
             new namespace. The old namespace's rows are kept and its unfinished runs are no longer \
             driven. To go back, restore the old [playbooks.namespace] and profile=\"{found}\".",
            want = self.config_name(),
        ))
    }
    /// The commands the host adapters serve under this profile.
    pub fn commands(self) -> &'static [&'static str] {
        match self {
            Self::Host9 => &HOST9_COMMANDS,
            #[cfg(feature = "delivery_playbook")]
            Self::Host10 => &HOST10_COMMANDS,
            _ => &[],
        }
    }
    pub fn has_reviews(self) -> bool {
        !(self == Self::Mail1 || self.is_host())
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
        if self == Self::Host9 {
            return host9_registry();
        }
        #[cfg(feature = "delivery_playbook")]
        if self == Self::Host10 {
            return host10_registry();
        }
        let mut registry = crate::registry();
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
    pub fn validate_scope(self, scope: &crate::GrantScope) -> Result<()> {
        if self == Self::Mail1 {
            return scope.validate();
        }
        if self.is_host() {
            let mut allowed: BTreeSet<String> = if self.has_routing() {
                HOST9_CAPABILITIES.iter().map(|c| c.to_string()).collect()
            } else if self.is_host2() {
                HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect()
            } else {
                HOST5_CAPABILITIES.iter().map(|c| c.to_string()).collect()
            };
            if self == Self::Monitoring6 {
                allowed.insert("findings.reconcile".into());
            }
            #[cfg(feature = "delivery_playbook")]
            if self.is_delivery() {
                allowed.extend(HOST10_DELIVERY_CAPABILITIES.iter().map(|c| c.to_string()));
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
        scope: &crate::GrantScope,
    ) -> Result<Authority> {
        self.validate_scope(scope)?;
        let mut authority = crate::authority(id, revision);
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
            ("recipient", crate::recipient_schema()),
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
            ("recipient", crate::recipient_schema()),
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
