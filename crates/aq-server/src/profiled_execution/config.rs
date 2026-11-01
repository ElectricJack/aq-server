//! Daemon-owned admission policy. Paths, URLs and arbitrary executable hooks are
//! deliberately absent; actual Git/Graph/credential adapters come from Config.
use aq_admission::{ArtifactRef, SourceSelection, transport::Feature};
use aq_selection::DeliveryCapability;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QualifiedRuntime {
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub capabilities: Vec<aq_session_protocol::Capability>,
    pub features: Vec<Feature>,
    pub material_delivery: Option<DeliveryCapability>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPolicy {
    pub project_id: Uuid,
    pub enabled: bool,
    pub priority: i32,
    pub source: SourceSelection,
    pub retention_handle: Uuid,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledConfig {
    pub revision: u64,
    pub qualified_runtimes: Vec<QualifiedRuntime>,
    pub projects: Vec<ProjectPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_qualifications: Vec<super::context_budget::Qualification>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub definition_bodies: Vec<super::definitions::Definition>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPolicyRecord {
    pub schema_version: u32,
    pub project_id: Uuid,
    pub revision: u64,
    pub configuration_digest: aq_admission::Digest,
    pub enabled: bool,
    pub priority: i32,
    pub source: SourceSelection,
    pub retention_handle: Uuid,
}
impl ProjectPolicyRecord {
    pub fn artifact(&self) -> Result<ArtifactRef, String> {
        if self.schema_version != 1
            || self.project_id.is_nil()
            || self.revision == 0
            || self.revision > i64::MAX as u64
        {
            return Err("project policy record identity".into());
        }
        Ok(ArtifactRef {
            id: aq_admission::Text::new(format!("aq.project-source-policy/{}", self.project_id))
                .map_err(|_| "policy artifact identity")?,
            version: aq_admission::Text::new(self.revision.to_string())
                .map_err(|_| "policy artifact version")?,
            digest: aq_selection::digest(self).map_err(|_| "policy artifact encoding")?,
        })
    }
}
fn artifact(value: &ArtifactRef) -> Result<(), String> {
    use aq_session_protocol::Validate;
    value
        .id
        .validate()
        .map_err(|_| "invalid configured artifact identity")?;
    value
        .version
        .validate()
        .map_err(|_| "invalid configured artifact version")?;
    if value.id.as_str().len() > 512 || value.version.as_str().len() > 512 {
        return Err("configured artifact bound".into());
    }
    Ok(())
}
impl ProfiledConfig {
    pub fn validate(&self) -> Result<(), String> {
        super::definitions::validate_catalog(&self.definition_bodies)?;
        super::accepted_preparation::policy::validate_catalog(self)?;
        super::phase_execution::policy::validate_catalog(self)?;
        super::phase_terminal::qualification::validate_catalog(self)?;
        super::context_budget::validate_catalog(&self.context_qualifications)
            .map_err(|_| "invalid context qualifications".to_string())?;
        if self.revision == 0
            || self.revision > i64::MAX as u64
            || self.qualified_runtimes.len() > 32
            || self.projects.len() > 128
        {
            return Err("profiled policy bounds".into());
        }
        let mut runtimes = BTreeSet::new();
        for item in &self.qualified_runtimes {
            artifact(&item.runtime)?;
            artifact(&item.qualification)?;
            if !runtimes.insert((item.runtime.id.clone(), item.runtime.version.clone()))
                || item.features.len() > 4
                || item.features.iter().collect::<BTreeSet<_>>().len() != item.features.len()
            {
                return Err("duplicate or unbounded qualified runtime".into());
            }
            aq_session_protocol::require_capabilities(&item.capabilities, &item.capabilities)
                .map_err(|_| "invalid qualified runtime capabilities")?;
            if let Some(delivery) = &item.material_delivery {
                artifact(&delivery.adapter)?;
                if delivery.version != 1 || !item.features.contains(&Feature::ServerMaterialV1) {
                    return Err("unsupported configured material delivery".into());
                }
            }
        }
        for fact in &self.context_qualifications {
            if !self.qualified_runtimes.iter().any(|runtime| {
                runtime.runtime == fact.subject.runtime
                    && runtime.qualification == fact.subject.runtime_qualification
            }) {
                return Err("context qualification references an unqualified runtime".into());
            }
        }
        let mut projects = BTreeSet::new();
        for project in &self.projects {
            if project.project_id.is_nil()
                || !projects.insert(project.project_id)
                || project.retention_handle.is_nil()
                || !(-1_000_000..=1_000_000).contains(&project.priority)
            {
                return Err("project source policy bounds".into());
            }
            project
                .source
                .validate()
                .map_err(|_| "explicit immutable object or safe source ref required")?;
        }
        Ok(())
    }
    /// Compact authority anchor. Private immutable Definition construction proves
    /// each reference digest equals its body, so request paths never rehash MiBs.
    /// Input/output serde still retains complete bodies for exact roundtrips.
    pub fn configuration_digest(&self) -> Result<aq_admission::Digest, String> {
        #[derive(Serialize)]
        struct Anchor<'a> {
            revision: u64,
            qualified_runtimes: &'a [QualifiedRuntime],
            projects: &'a [ProjectPolicy],
            #[serde(skip_serializing_if = "Vec::is_empty")]
            context_qualifications: Vec<&'a super::context_budget::Qualification>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            definition_bodies: Vec<super::definitions::DefinitionReference<'a>>,
        }
        aq_selection::digest(&Anchor {
            revision: self.revision,
            qualified_runtimes: &self.qualified_runtimes,
            projects: &self.projects,
            context_qualifications: self.context_qualifications.iter().collect(),
            definition_bodies: super::definitions::references(&self.definition_bodies),
        })
        .map_err(|_| "profiled configuration digest".into())
    }
    pub fn project_policy_record(&self, project_id: Uuid) -> Result<ProjectPolicyRecord, String> {
        self.validate()?;
        let policy = self
            .projects
            .iter()
            .find(|policy| policy.project_id == project_id)
            .ok_or("project has no configured source policy")?;
        Ok(ProjectPolicyRecord {
            schema_version: 1,
            project_id,
            revision: self.revision,
            configuration_digest: self.configuration_digest()?,
            enabled: policy.enabled,
            priority: policy.priority,
            source: policy.source.clone(),
            retention_handle: policy.retention_handle,
        })
    }
    pub fn project_policy_ref(&self, project_id: Uuid) -> Result<ArtifactRef, String> {
        self.project_policy_record(project_id)?.artifact()
    }
    pub fn qualified(
        &self,
        request: &aq_admission::transport::Register,
    ) -> Result<&QualifiedRuntime, String> {
        self.validate()?;
        aq_admission::transport::TransportDocument::Register(request.clone())
            .validate()
            .map_err(|_| "invalid runtime registration")?;
        let runtime = self
            .qualified_runtimes
            .iter()
            .find(|runtime| runtime.runtime == request.runtime)
            .ok_or("runtime has no exact daemon-owned qualification")?;
        aq_session_protocol::require_capabilities(&runtime.capabilities, &request.capabilities)
            .map_err(|_| "qualified runtime capability mismatch")?;
        if request
            .features
            .iter()
            .any(|feature| !runtime.features.contains(feature))
            || runtime
                .features
                .iter()
                .any(|feature| !request.features.contains(feature))
        {
            return Err("qualified runtime feature mismatch".into());
        }
        Ok(runtime)
    }
}
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<std::sync::Arc<ProfiledConfig>>, D::Error> {
    Option::<ProfiledConfig>::deserialize(deserializer)?
        .map(|config| {
            config.validate().map_err(serde::de::Error::custom)?;
            Ok(std::sync::Arc::new(config))
        })
        .transpose()
}
