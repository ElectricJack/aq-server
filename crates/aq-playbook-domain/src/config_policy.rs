//! Catalog selection, integration policy and validated startup deployments.
pub mod integration;
use crate::{GrantScope, Namespace, review_domain::CatalogProfile};
use aq_playbook_host::model::{Artifact, schedules::BindingSchedule};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;
type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Copy, Deserialize)]
pub enum Profile {
    #[serde(rename = "human_review_v2")]
    HumanReview2,
    #[serde(rename = "graph_events_v3")]
    GraphEvents3,
    #[serde(rename = "settlement_events_v4")]
    SettlementEvents4,
    #[serde(rename = "monitoring_v6")]
    Monitoring6,
    #[serde(rename = "host_v7")]
    Host7,
    #[serde(rename = "host_v8")]
    Host8,
    #[serde(rename = "host_v9")]
    Host9,
    #[cfg(feature = "delivery_playbook")]
    #[serde(rename = "host_v10")]
    Host10,
    #[serde(rename = "host_v11")]
    Host11,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentInput {
    artifact_file: PathBuf,
    binding: String,
    #[serde(default)]
    schedules: Vec<BindingSchedule>,
    grant_id: Uuid,
    scope: GrantScope,
    expires_at_ms: i64,
}
pub struct Deployment {
    pub artifact: Artifact,
    pub binding: String,
    pub schedules: Vec<BindingSchedule>,
    pub grant_id: Uuid,
    pub scope: GrantScope,
    pub expires_at_ms: i64,
}
impl From<Profile> for CatalogProfile {
    fn from(value: Profile) -> Self {
        match value {
            Profile::HumanReview2 => CatalogProfile::HumanReview2,
            Profile::GraphEvents3 => CatalogProfile::GraphEvents3,
            Profile::SettlementEvents4 => CatalogProfile::SettlementEvents4,
            Profile::Monitoring6 => CatalogProfile::Monitoring6,
            Profile::Host7 => CatalogProfile::Host7,
            Profile::Host8 => CatalogProfile::Host8,
            Profile::Host9 => CatalogProfile::Host9,
            #[cfg(feature = "delivery_playbook")]
            Profile::Host10 => CatalogProfile::Host10,
            Profile::Host11 => CatalogProfile::Host11,
        }
    }
}
pub fn valid_binding(value: &str) -> bool {
    (1..=128).contains(&value.len()) && !value.contains('\0')
}
impl DeploymentInput {
    fn belongs_to(&self, n: &Namespace) -> bool {
        let valid_grant = !self.grant_id.is_nil() && self.expires_at_ms > 0;
        valid_grant
            && (self.scope.project_id, self.scope.workspace_id) == (n.project_id, n.workspace_id)
    }
}
pub fn load_deployments<B: AsRef<[u8]>>(
    entries: Vec<DeploymentInput>,
    n: &Namespace,
    profile: CatalogProfile,
    read_artifact: impl Fn(&Path) -> Result<B>,
) -> Result<Vec<Deployment>> {
    let mut deployments = vec![];
    let mut bindings = std::collections::BTreeSet::new();
    let mut total = 0;
    for d in entries {
        if !valid_binding(&d.binding) || !bindings.insert(d.binding.clone()) {
            return Err("invalid playbook deployment binding");
        }
        if !d.belongs_to(n) {
            return Err("invalid playbook deployment binding");
        }
        BindingSchedule::validate_all(&d.schedules)
            .map_err(|_| "invalid playbook deployment schedules")?;
        profile
            .validate_scope_with_extensions(&d.scope, |_| true)
            .map_err(|_| "invalid playbook deployment scope")?;
        let raw = read_artifact(&d.artifact_file)?;
        let raw = raw.as_ref();
        total += raw.len();
        if total > 1048576 {
            return Err("playbook deployment byte bound");
        }
        let artifact: Artifact =
            serde_json::from_slice(raw).map_err(|_| "invalid compiled playbook artifact")?;
        artifact
            .verify()
            .map_err(|_| "invalid compiled playbook artifact")?;
        deployments.push(Deployment {
            artifact,
            binding: d.binding,
            schedules: d.schedules,
            grant_id: d.grant_id,
            scope: d.scope,
            expires_at_ms: d.expires_at_ms,
        });
    }
    Ok(deployments)
}
