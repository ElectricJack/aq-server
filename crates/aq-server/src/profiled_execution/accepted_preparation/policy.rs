use super::phase::{FactKind, OwnerAuthority, PhasePolicy};
use super::*;
use aq_admission::ArtifactRef;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
const PREFIX: &str = super::POLICY_PREFIX;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDefinition {
    pub schema_version: u32,
    pub contract: ArtifactRef,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub phase: PhasePolicy,
    pub source_profile: ArtifactRef,
    pub source_disk_bytes: u64,
    pub owners: Vec<OwnerAuthority>,
}
impl PolicyDefinition {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.contract != phase::contract_artifact().map_err(|_| "phase contract")?
            || self.owners.len() != 9
            || !(1..=68_719_476_736).contains(&self.source_disk_bytes)
        {
            return Err("phase policy shape".into());
        }
        self.phase.validate().map_err(|_| "phase policy limits")?;
        use aq_session_protocol::Validate;
        for a in [&self.runtime, &self.qualification, &self.source_profile] {
            a.id.validate().map_err(|_| "phase profile identity")?;
            a.version.validate().map_err(|_| "phase profile version")?;
            if a.id.as_str().len() > 512 || a.version.as_str().len() > 512 {
                return Err("phase profile bound".into());
            }
        }

        let mut kinds = BTreeSet::new();
        for x in &self.owners {
            use aq_session_protocol::Validate;
            x.authority
                .id
                .validate()
                .map_err(|_| "phase authority identity")?;
            x.authority
                .version
                .validate()
                .map_err(|_| "phase authority version")?;
            if !kinds.insert(x.kind) {
                return Err("duplicate phase authority".into());
            }
        }
        if ![
            FactKind::Project,
            FactKind::Registry,
            FactKind::Principal,
            FactKind::Inventory,
            FactKind::Availability,
            FactKind::Budget,
            FactKind::Context,
            FactKind::Git,
            FactKind::PrivateMaterialBacking,
        ]
        .iter()
        .all(|k| kinds.contains(k))
        {
            return Err("phase authority set".into());
        }
        Ok(())
    }
}
pub fn validate_catalog(config: &super::super::config::ProfiledConfig) -> Result<(), String> {
    let mut runtimes = BTreeSet::new();
    for d in &config.definition_bodies {
        if !d.artifact().id.as_str().starts_with(PREFIX) {
            continue;
        }
        if d.body().len() > 16_384 {
            return Err("phase policy byte bound".into());
        }
        let p: PolicyDefinition =
            serde_json::from_str(d.body()).map_err(|_| "phase policy body")?;
        p.validate()?;
        if p.phase.schema_version == 3
            && !super::super::producer::interactive_runtime(config, &p.runtime)
                .map_err(|_| "interactive phase factory unavailable")?
        {
            return Err("long phase policy requires actual interactive factory".into());
        }
        if !config
            .qualified_runtimes
            .iter()
            .any(|r| r.runtime == p.runtime && r.qualification == p.qualification)
            || !runtimes.insert((p.runtime.id.clone(), p.runtime.version.clone()))
        {
            return Err("unqualified or duplicate phase runtime".into());
        }
    }
    Ok(())
}
pub fn policy_for(
    state: &AppState,
    runtime: &ArtifactRef,
    qualification: &ArtifactRef,
) -> Result<(ArtifactRef, PolicyDefinition), AppError> {
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    validate_catalog(config).map_err(|_| refused())?;
    for d in &config.definition_bodies {
        if d.artifact().id.as_str().starts_with(PREFIX) {
            let p: PolicyDefinition = serde_json::from_str(d.body()).map_err(|_| refused())?;
            if &p.runtime == runtime && &p.qualification == qualification {
                return Ok((d.artifact().clone(), p));
            }
        }
    }
    Err(refused())
}
pub(super) fn enabled(state: &AppState) -> bool {
    state.config.profiled_execution.as_ref().is_some_and(|c| {
        c.definition_bodies
            .iter()
            .any(|d| d.artifact().id.as_str().starts_with(PREFIX))
    })
}
