//! Pure project policy and admission snapshots. These types confer no authority.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub mod decision;
mod eligibility;
mod validation;
pub use eligibility::{EligibilityEvidence, decide, local_reasons};
use uuid::Uuid;
#[cfg(feature = "project_environments")]
pub mod environments;
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProjectConfig {
    pub worker_groups: BTreeMap<String, Vec<Uuid>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub identity: String,
    pub revision: String,
    pub digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationScope {
    pub operation: String,
    pub scope_reference: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCeilings {
    pub permissions: Vec<OperationScope>,
    pub context_bytes: u64,
    pub wall_time_ms: u64,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub process_limit: u32,
    pub cost_limit_microunits: Option<u64>,
    pub currency: Option<String>,
    pub require_hard_cost_enforcement: bool,
}
fn enrolled_by_default() -> bool {
    true
}
fn is_true(v: &bool) -> bool {
    *v
}
/// The role of a task that names none (spec 0014 §5.4).
pub const DEFAULT_ROLE: &str = "developer";
/// The intelligence profile of a task that was not routed to a level.
pub const DEFAULT_INTELLIGENCE: &str = "standard-high";
fn default_role() -> String {
    DEFAULT_ROLE.into()
}
fn default_intelligence() -> String {
    DEFAULT_INTELLIGENCE.into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBinding {
    pub schema_version: u32,
    pub project_id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub repository_id: Uuid,
    pub repository_binding_generation: u64,
    /// Fixed logical references to the server-configured adapters, never credentials.
    pub repository_adapter_reference: String,
    pub graph_adapter_reference: String,
    pub graph_workspace_id: Uuid,
    /// The role of a task that names none (spec 0014 §5.4). A request that
    /// leaves it out gets [`DEFAULT_ROLE`]; a stored record always names it.
    #[serde(default = "default_role")]
    pub default_role: String,
    /// The intelligence profile used when a task was not routed to a level.
    #[serde(default = "default_intelligence")]
    pub default_intelligence: String,
    /// Markdown per role id, added after the role's prompt; it never replaces
    /// it. Each note is bounded by the role store's document byte limit,
    /// which the server checks when the binding is saved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_notes: BTreeMap<String, String>,
    pub allowed_node_ids: Vec<Uuid>,
    /// Spec 0007 Q3: on by default; `false` restricts the binding to its listed nodes and groups.
    #[serde(default = "enrolled_by_default", skip_serializing_if = "is_true")]
    pub allow_enrolled_nodes: bool,
    pub allowed_worker_groups: Vec<String>,
    pub allowed_environments: Vec<String>,
    /// Spec 0017 §5.2: where the project's tasks run unless their needs place
    /// them elsewhere; one of `allowed_environments`. Absent on older records.
    #[cfg(feature = "project_environments")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_environment: Option<String>,
    pub policy: Reference,
    pub context: Vec<Reference>,
    pub ceilings: ProjectCeilings,
    /// Absent on legacy records, which keeps their serialized digest unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<aq_delivery::PublicationPolicy>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRecord {
    pub revision: u64,
    pub binding: ProjectBinding,
    pub validated_at_ms: u64,
    pub validation: BindingObservation,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalStatus {
    Verified,
    Absent,
    Stale,
    Forbidden,
    Unavailable,
    Unsupported,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingObservation {
    pub repository_id: Uuid,
    pub graph_workspace_id: Uuid,
    pub repository_adapter_reference: String,
    pub graph_adapter_reference: String,
    pub repository: ExternalStatus,
    pub graph: ExternalStatus,
    pub repository_generation: Option<u64>,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EligibilityRequest {
    pub expected_project_revision: u64,
    pub node_id: Uuid,
    pub environment: String,
    /// The role, intelligence profile and execution profile of the launch
    /// (spec 0014), by document id, in the role snapshot `snapshot_digest`.
    pub role: String,
    pub intelligence_profile: String,
    pub execution_profile: String,
    pub snapshot_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenProjectBinding {
    pub project: ProjectRecord,
    /// The role snapshot the launch was resolved from. An offer stays on it:
    /// a later save of a document does not change what this binding names.
    pub snapshot_digest: String,
    pub worker_membership_digest: String,
    pub binding_observation: BindingObservation,
    pub node_id: Uuid,
    pub environment: String,
    pub role: String,
    pub intelligence_profile: String,
    pub execution_profile: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    pub eligible: bool,
    pub reasons: Vec<String>,
    pub frozen_binding: Option<FrozenProjectBinding>,
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn binding() -> ProjectBinding {
        ProjectBinding {
            schema_version: 1,
            project_id: Uuid::from_u128(1),
            name: "Example project".into(),
            enabled: true,
            repository_id: Uuid::from_u128(2),
            repository_binding_generation: 1,
            repository_adapter_reference: "aq.git.configured".into(),
            graph_adapter_reference: "aq.graph.configured".into(),
            graph_workspace_id: Uuid::from_u128(3),
            default_role: DEFAULT_ROLE.into(),
            default_intelligence: DEFAULT_INTELLIGENCE.into(),
            role_notes: BTreeMap::new(),
            allowed_node_ids: vec![Uuid::from_u128(4)],
            allow_enrolled_nodes: false,
            allowed_worker_groups: vec![],
            allowed_environments: vec!["confined-worktree".into()],
            #[cfg(feature = "project_environments")]
            default_environment: None,
            policy: Reference {
                identity: "project-policy".into(),
                revision: "1".into(),
                digest: "b".repeat(64),
            },
            context: vec![],
            ceilings: ProjectCeilings {
                permissions: vec![OperationScope {
                    operation: "task.read".into(),
                    scope_reference: "project:example".into(),
                }],
                context_bytes: 8192,
                wall_time_ms: 60000,
                memory_bytes: 268435456,
                cpu_millis: 60000,
                process_limit: 8,
                cost_limit_microunits: None,
                currency: None,
                require_hard_cost_enforcement: false,
            },
            publication: None,
        }
    }
    fn record() -> ProjectRecord {
        ProjectRecord {
            revision: 1,
            binding: binding(),
            validated_at_ms: 1000,
            validation: observation(),
        }
    }
    fn request() -> EligibilityRequest {
        EligibilityRequest {
            expected_project_revision: 1,
            node_id: Uuid::from_u128(4),
            environment: "confined-worktree".into(),
            role: "developer".into(),
            intelligence_profile: "standard-medium".into(),
            execution_profile: "sandbox".into(),
            snapshot_digest: "c".repeat(64),
        }
    }
    fn observation() -> BindingObservation {
        BindingObservation {
            repository_id: Uuid::from_u128(2),
            graph_workspace_id: Uuid::from_u128(3),
            repository_adapter_reference: "aq.git.configured".into(),
            graph_adapter_reference: "aq.graph.configured".into(),
            repository: ExternalStatus::Verified,
            graph: ExternalStatus::Verified,
            repository_generation: Some(1),
            observed_at_ms: 1000,
            expires_at_ms: 31000,
        }
    }
    #[test]
    fn pure_gate_freezes_exact_revision_and_never_rewrites_old_snapshots() {
        let mut record = record();
        let config = ProjectConfig::default();
        let request = request();
        let observation = observation();
        let decision = decide(
            &record,
            &request,
            &config,
            EligibilityEvidence::new(&observation, true, 2000),
        );
        assert!(decision.eligible);
        let frozen = decision.frozen_binding.unwrap();
        record.revision = 2;
        record.binding.ceilings.context_bytes = 4096;
        assert!(
            !decide(
                &record,
                &request,
                &config,
                EligibilityEvidence::new(&observation, true, 2000)
            )
            .eligible
        );
        assert_eq!(frozen.project.revision, 1);
        assert_eq!(frozen.project.binding.ceilings.context_bytes, 8192);
    }
    #[test]
    fn external_absence_staleness_forbidden_and_cross_identity_never_admit() {
        for status in [
            ExternalStatus::Absent,
            ExternalStatus::Stale,
            ExternalStatus::Forbidden,
            ExternalStatus::Unavailable,
            ExternalStatus::Unsupported,
        ] {
            let mut observation = observation();
            observation.repository = status.clone();
            assert!(
                !decide(
                    &record(),
                    &request(),
                    &ProjectConfig::default(),
                    EligibilityEvidence::new(&observation, true, 2000)
                )
                .eligible
            );
            observation.repository = ExternalStatus::Verified;
            observation.graph = status;
            assert!(
                !decide(
                    &record(),
                    &request(),
                    &ProjectConfig::default(),
                    EligibilityEvidence::new(&observation, true, 2000)
                )
                .eligible
            );
        }
        let mut o = observation();
        o.repository_id = Uuid::from_u128(99);
        assert!(
            !decide(
                &record(),
                &request(),
                &ProjectConfig::default(),
                EligibilityEvidence::new(&o, true, 2000)
            )
            .eligible
        );
        let o = observation();
        assert!(
            !decide(
                &record(),
                &request(),
                &ProjectConfig::default(),
                EligibilityEvidence::new(&o, true, 31000)
            )
            .eligible
        );
    }
    #[test]
    fn operator_groups_and_environment_profile_ceilings_fail_closed() {
        let mut record = record();
        record.binding.allowed_node_ids.clear();
        record.binding.allowed_worker_groups = vec!["engineering".into()];
        let mut config = ProjectConfig::default();
        assert!(record.binding.validate(&config).is_err());
        config
            .worker_groups
            .insert("engineering".into(), vec![Uuid::from_u128(4)]);
        assert!(record.binding.validate(&config).is_ok());
        assert!(
            decide(
                &record,
                &request(),
                &config,
                EligibilityEvidence::new(&observation(), true, 2000)
            )
            .eligible
        );
        let mut r = request();
        r.environment = "host-unconfined".into();
        assert!(
            !decide(
                &record,
                &r,
                &config,
                EligibilityEvidence::new(&observation(), true, 2000)
            )
            .eligible
        );
        // A launch names its role, level and execution profile by id; the
        // binding holds no list of them (spec 0014 §5.4).
        let mut r = request();
        r.role = "not a label".into();
        assert!(
            !decide(
                &record,
                &r,
                &config,
                EligibilityEvidence::new(&observation(), true, 2000)
            )
            .eligible
        );
        let mut r = request();
        r.snapshot_digest = "short".into();
        assert!(
            !decide(
                &record,
                &r,
                &config,
                EligibilityEvidence::new(&observation(), true, 2000)
            )
            .eligible
        );
    }
    #[test]
    fn enrolled_flag_makes_every_node_eligible_and_validates_without_lists() {
        let (config, stranger) = (ProjectConfig::default(), Uuid::from_u128(99));
        let mut b = binding();
        assert!(!b.eligible_node(&config, stranger));
        b.allow_enrolled_nodes = true;
        b.allowed_node_ids.clear();
        assert!(b.eligible_node(&config, stranger));
        assert!(b.validate(&config).is_ok());
        b.allow_enrolled_nodes = false;
        assert!(b.validate(&config).is_err());
    }
    #[test]
    fn role_defaults_are_always_written_and_a_request_without_them_gets_the_defaults() {
        let config = ProjectConfig::default();
        let mut stored = serde_json::to_value(binding()).unwrap();
        assert_eq!(stored["default_role"], "developer");
        assert_eq!(stored["default_intelligence"], "standard-high");
        assert!(stored.get("role_notes").is_none());
        // A request that names neither default gets them.
        let request = stored.as_object_mut().unwrap();
        request.remove("default_role");
        request.remove("default_intelligence");
        let loaded: ProjectBinding = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(
            (
                loaded.default_role.as_str(),
                loaded.default_intelligence.as_str()
            ),
            ("developer", "standard-high")
        );
        // The fields spec 0014 removed are unknown and refused by name.
        for field in [
            "catalog_digest",
            "allowed_agent_profiles",
            "allowed_intelligence_profiles",
            "allowed_execution_profiles",
        ] {
            let mut old = stored.clone();
            old[field] = serde_json::json!([]);
            let error = serde_json::from_value::<ProjectBinding>(old).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("unknown field `{field}`")),
                "{error}"
            );
        }
        assert!(loaded.role_notes.is_empty() && loaded.validate(&config).is_ok());
        let mut b = binding();
        b.default_role = "reviewer".into();
        b.default_intelligence = "deep-high".into();
        b.role_notes
            .insert("reviewer".into(), "Review the migration first.".into());
        let value = serde_json::to_value(&b).unwrap();
        assert_eq!(value["default_role"], "reviewer");
        assert_eq!(value["default_intelligence"], "deep-high");
        assert_eq!(
            value["role_notes"]["reviewer"],
            "Review the migration first."
        );
        assert!(b.validate(&config).is_ok());
        let again: ProjectBinding = serde_json::from_value(value).unwrap();
        assert_eq!(again.role_notes, b.role_notes);
        for (role, intelligence, note_role, note) in [
            ("", "deep-high", "reviewer", "x"),
            ("reviewer", "two words", "reviewer", "x"),
            ("reviewer", "deep-high", "bad role", "x"),
            ("reviewer", "deep-high", "reviewer", "nul\0"),
        ] {
            let mut b = binding();
            b.default_role = role.into();
            b.default_intelligence = intelligence.into();
            b.role_notes.insert(note_role.into(), note.into());
            assert!(
                b.validate(&config).is_err(),
                "{role} {intelligence} {note_role}"
            );
        }
    }
    #[test]
    fn binding_without_flag_serializes_unchanged() {
        let mut v = serde_json::to_value(binding()).unwrap();
        v.as_object_mut().unwrap().remove("allow_enrolled_nodes");
        let b: ProjectBinding = serde_json::from_value(v.clone()).unwrap();
        assert!(b.allow_enrolled_nodes);
        assert_eq!(serde_json::to_value(&b).unwrap(), v);
    }
    #[test]
    fn request_fields_cannot_install_urls_credentials_or_worker_claimed_groups() {
        let mut v = serde_json::to_value(binding()).unwrap();
        v["credential"] = "secret".into();
        assert!(serde_json::from_value::<ProjectBinding>(v).is_err());
        let mut b = binding();
        b.graph_adapter_reference = "https://evil.example".into();
        assert!(b.validate(&ProjectConfig::default()).is_err());
        let mut b = binding();
        b.ceilings.require_hard_cost_enforcement = true;
        assert!(b.validate(&ProjectConfig::default()).is_err());
        let mut b = binding();
        b.allowed_node_ids = vec![Uuid::nil()];
        assert!(b.validate(&ProjectConfig::default()).is_err());
    }
}
