//! Pure project policy and admission snapshots. These types confer no authority.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProjectConfig {
    pub worker_groups: BTreeMap<String, Vec<Uuid>>,
}
impl ProjectConfig {
    pub fn validate(&self) -> Result<(), String> {
        require(self.worker_groups.len() <= 64, "worker group bound")?;
        for (id, nodes) in &self.worker_groups {
            label(id)?;
            ids(nodes, 256)?;
            require(!nodes.is_empty(), "empty worker group")?;
        }
        Ok(())
    }
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
fn is_default_role(v: &String) -> bool {
    v == DEFAULT_ROLE
}
fn default_intelligence() -> String {
    DEFAULT_INTELLIGENCE.into()
}
fn is_default_intelligence(v: &String) -> bool {
    v == DEFAULT_INTELLIGENCE
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
    // Spec 0014 §5.4: `catalog_digest` and the three `allowed_*_profiles`
    // lists are removed in step 3c; until then the offer path reads them.
    pub catalog_digest: String,
    pub allowed_agent_profiles: Vec<String>,
    pub allowed_intelligence_profiles: Vec<String>,
    pub allowed_execution_profiles: Vec<String>,
    /// The role of a task that names none. Absent on records written before
    /// spec 0014, and not serialized at its default, like the fields below,
    /// so those records load and keep their serialized digest.
    #[serde(default = "default_role", skip_serializing_if = "is_default_role")]
    pub default_role: String,
    /// The intelligence profile used when a task was not routed to a level.
    #[serde(
        default = "default_intelligence",
        skip_serializing_if = "is_default_intelligence"
    )]
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
fn require(ok: bool, reason: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(reason.into()) }
}
fn label(s: &str) -> Result<(), String> {
    require(
        !s.is_empty()
            && s.len() <= 128
            && s.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@' | b':')
            }),
        "bounded logical reference required",
    )
}
fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn ids(values: &[Uuid], max: usize) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    require(values.len() <= max, "identity list bound")?;
    for id in values {
        require(!id.is_nil() && seen.insert(id), "nil or duplicate identity")?;
    }
    Ok(())
}
fn labels(values: &[String], max: usize) -> Result<(), String> {
    require(values.len() <= max, "reference list bound")?;
    let mut seen = BTreeSet::new();
    for s in values {
        label(s)?;
        require(seen.insert(s), "duplicate reference")?;
    }
    Ok(())
}
impl ProjectBinding {
    pub fn validate(&self, config: &ProjectConfig) -> Result<(), String> {
        config.validate()?;
        if let Some(p) = &self.publication {
            p.validate().map_err(|e| e.to_string())?;
        }
        require(
            self.schema_version == 1
                && !self.project_id.is_nil()
                && !self.repository_id.is_nil()
                && !self.graph_workspace_id.is_nil()
                && self.repository_binding_generation > 0
                && self.repository_binding_generation <= i64::MAX as u64,
            "project identity/version",
        )?;
        require(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            "project name",
        )?;
        require(
            self.repository_adapter_reference == "aq.git.configured"
                && self.graph_adapter_reference == "aq.graph.configured",
            "unknown configured adapter reference",
        )?;
        require(hex(&self.catalog_digest), "catalog digest")?;
        for list in [
            &self.allowed_agent_profiles,
            &self.allowed_intelligence_profiles,
            &self.allowed_execution_profiles,
            &self.allowed_environments,
        ] {
            labels(list, 32)?;
            require(!list.is_empty(), "empty profile/environment allowlist")?;
        }
        for list in [
            &self.allowed_agent_profiles,
            &self.allowed_intelligence_profiles,
            &self.allowed_execution_profiles,
        ] {
            for profile in list {
                let (id, version) = profile
                    .rsplit_once('@')
                    .ok_or("profile needs exact version")?;
                label(id)?;
                require(
                    version.parse::<u32>().is_ok_and(|v| v > 0),
                    "profile version",
                )?;
            }
        }
        label(&self.default_role).map_err(|_| "default role")?;
        label(&self.default_intelligence).map_err(|_| "default intelligence profile")?;
        for (role, note) in &self.role_notes {
            label(role).map_err(|_| "role notes role")?;
            require(!note.contains('\0'), "role notes text")?;
        }
        ids(&self.allowed_node_ids, 256)?;
        labels(&self.allowed_worker_groups, 32)?;
        require(
            self.allow_enrolled_nodes
                || !self.allowed_node_ids.is_empty()
                || !self.allowed_worker_groups.is_empty(),
            "no eligible worker identities",
        )?;
        for group in &self.allowed_worker_groups {
            require(
                config.worker_groups.contains_key(group),
                "unknown worker group",
            )?;
        }
        require(self.context.len() <= 32, "context reference bound")?;
        let mut seen = BTreeSet::new();
        for r in std::iter::once(&self.policy).chain(&self.context) {
            label(&r.identity)?;
            label(&r.revision)?;
            require(
                hex(&r.digest) && seen.insert((&r.identity, &r.revision)),
                "invalid or duplicate policy/context reference",
            )?;
        }
        let c = &self.ceilings;
        require(c.permissions.len() <= 128, "permission bound")?;
        let mut permissions = BTreeSet::new();
        for p in &c.permissions {
            label(&p.operation)?;
            label(&p.scope_reference)?;
            require(
                p.scope_reference.contains(':')
                    && permissions.insert((&p.operation, &p.scope_reference)),
                "invalid or duplicate permission",
            )?;
        }
        require(
            c.context_bytes > 0
                && c.context_bytes <= 1_048_576
                && c.wall_time_ms > 0
                && c.wall_time_ms <= 604_800_000
                && c.memory_bytes > 0
                && c.memory_bytes <= i64::MAX as u64
                && c.cpu_millis > 0
                && c.cpu_millis <= i64::MAX as u64
                && c.process_limit > 0
                && c.process_limit <= 65_536,
            "resource ceiling bounds",
        )?;
        require(
            c.cost_limit_microunits.is_some() == c.currency.is_some()
                && (!c.require_hard_cost_enforcement || c.cost_limit_microunits.is_some()),
            "cost ceiling basis",
        )?;
        if let Some(currency) = &c.currency {
            require(
                currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()),
                "currency code",
            )?;
        }
        Ok(())
    }
    pub fn eligible_node(&self, config: &ProjectConfig, node: Uuid) -> bool {
        self.allow_enrolled_nodes
            || self.allowed_node_ids.contains(&node)
            || self.allowed_worker_groups.iter().any(|g| {
                config
                    .worker_groups
                    .get(g)
                    .is_some_and(|ids| ids.contains(&node))
            })
    }
}
pub fn local_reasons(
    record: &ProjectRecord,
    request: &EligibilityRequest,
    config: &ProjectConfig,
    node_live: bool,
) -> Vec<String> {
    let b = &record.binding;
    let mut reasons = Vec::new();
    if b.validate(config).is_err() {
        reasons.push("project_policy_invalid".into());
    }
    if !b.enabled {
        reasons.push("project_disabled".into());
    }
    if record.revision != request.expected_project_revision {
        reasons.push("project_revision_changed".into());
    }
    if request.node_id.is_nil() || !node_live || !b.eligible_node(config, request.node_id) {
        reasons.push("node_not_eligible".into());
    }
    if !b.allowed_environments.contains(&request.environment) {
        reasons.push("environment_denied".into());
    }
    for value in [
        &request.role,
        &request.intelligence_profile,
        &request.execution_profile,
    ] {
        if label(value).is_err() {
            reasons.push("launch_reference_invalid".into());
        }
    }
    if !hex(&request.snapshot_digest) {
        reasons.push("snapshot_digest_invalid".into());
    }
    reasons
}
pub fn decide(
    record: &ProjectRecord,
    request: &EligibilityRequest,
    config: &ProjectConfig,
    observation: &BindingObservation,
    node_live: bool,
    now_ms: u64,
) -> Eligibility {
    let b = &record.binding;
    let mut reasons = local_reasons(record, request, config, node_live);
    if observation.repository_id != b.repository_id
        || observation.graph_workspace_id != b.graph_workspace_id
        || observation.repository_adapter_reference != b.repository_adapter_reference
        || observation.graph_adapter_reference != b.graph_adapter_reference
    {
        reasons.push("binding_observation_identity_mismatch".into());
    }
    if observation.observed_at_ms == 0
        || now_ms < observation.observed_at_ms
        || now_ms >= observation.expires_at_ms
        || observation
            .expires_at_ms
            .saturating_sub(observation.observed_at_ms)
            > 30_000
    {
        reasons.push("binding_observation_stale".into());
    }
    if observation.repository != ExternalStatus::Verified {
        reasons.push(format!("repository_{:?}", observation.repository).to_lowercase());
    } else if observation.repository_generation != Some(b.repository_binding_generation) {
        reasons.push("repository_stale".into());
    }
    if observation.graph != ExternalStatus::Verified {
        reasons.push(format!("graph_{:?}", observation.graph).to_lowercase());
    }
    let eligible = reasons.is_empty();
    Eligibility {
        eligible,
        reasons,
        frozen_binding: eligible.then(|| FrozenProjectBinding {
            project: record.clone(),
            snapshot_digest: request.snapshot_digest.clone(),
            worker_membership_digest: aq_profiles::digest(config)
                .expect("bounded membership serializes"),
            binding_observation: observation.clone(),
            node_id: request.node_id,
            environment: request.environment.clone(),
            role: request.role.clone(),
            intelligence_profile: request.intelligence_profile.clone(),
            execution_profile: request.execution_profile.clone(),
        }),
    }
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
            catalog_digest: "a".repeat(64),
            allowed_agent_profiles: vec!["implementer@1".into()],
            allowed_intelligence_profiles: vec!["engineering@1".into()],
            allowed_execution_profiles: vec!["confined-development@1".into()],
            default_role: DEFAULT_ROLE.into(),
            default_intelligence: DEFAULT_INTELLIGENCE.into(),
            role_notes: BTreeMap::new(),
            allowed_node_ids: vec![Uuid::from_u128(4)],
            allow_enrolled_nodes: false,
            allowed_worker_groups: vec![],
            allowed_environments: vec!["confined-worktree".into()],
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
        let decision = decide(&record, &request, &config, &observation, true, 2000);
        assert!(decision.eligible);
        let frozen = decision.frozen_binding.unwrap();
        record.revision = 2;
        record.binding.ceilings.context_bytes = 4096;
        assert!(!decide(&record, &request, &config, &observation, true, 2000).eligible);
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
                    &observation,
                    true,
                    2000
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
                    &observation,
                    true,
                    2000
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
                &o,
                true,
                2000
            )
            .eligible
        );
        let o = observation();
        assert!(
            !decide(
                &record(),
                &request(),
                &ProjectConfig::default(),
                &o,
                true,
                31000
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
        assert!(decide(&record, &request(), &config, &observation(), true, 2000).eligible);
        let mut r = request();
        r.environment = "host-unconfined".into();
        assert!(!decide(&record, &r, &config, &observation(), true, 2000).eligible);
        // A launch names its role, level and execution profile by id; the
        // binding holds no list of them (spec 0014 §5.4).
        let mut r = request();
        r.role = "not a label".into();
        assert!(!decide(&record, &r, &config, &observation(), true, 2000).eligible);
        let mut r = request();
        r.snapshot_digest = "short".into();
        assert!(!decide(&record, &r, &config, &observation(), true, 2000).eligible);
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
    fn role_defaults_are_absent_on_old_records_and_not_written_at_their_default() {
        let config = ProjectConfig::default();
        let old = serde_json::to_value(binding()).unwrap();
        for key in ["default_role", "default_intelligence", "role_notes"] {
            assert!(old.get(key).is_none(), "{key}");
        }
        // A record written before spec 0014 loads with the defaults.
        let loaded: ProjectBinding = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            (
                loaded.default_role.as_str(),
                loaded.default_intelligence.as_str()
            ),
            ("developer", "standard-high")
        );
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
