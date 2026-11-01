use super::*;
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
const PREFIX: &str = "aq.phase-execution-qualification/";
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterSet {
    CurrentProjectRegistryPrincipalInventoryHealthUnissuedGraphRetentionPrivateV1,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionQualification {
    pub schema_version: u32,
    pub contract: ArtifactRef,
    pub runtime: ArtifactRef,
    pub runtime_qualification: ArtifactRef,
    pub proposal_policy: ArtifactRef,
    pub adapters: AdapterSet,
}
impl ExecutionQualification {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.contract
                != aq_admission::preparation_v2::contract_artifact()
                    .map_err(|_| "phase execution contract")?
        {
            return Err("unsupported phase execution qualification".into());
        }
        for a in [
            &self.runtime,
            &self.runtime_qualification,
            &self.proposal_policy,
        ] {
            a.id.validate()
                .map_err(|_| "execution qualification identity")?;
            a.version
                .validate()
                .map_err(|_| "execution qualification version")?;
            if a.id.as_str().len() > 512 || a.version.as_str().len() > 512 {
                return Err("execution qualification bound".into());
            }
        }
        Ok(())
    }
}
pub(crate) fn validate_catalog(
    config: &super::super::config::ProfiledConfig,
) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for d in &config.definition_bodies {
        if !d.artifact().id.as_str().starts_with(PREFIX) {
            continue;
        }
        if d.body().len() > 8192 {
            return Err("execution qualification byte bound".into());
        }
        let q: ExecutionQualification =
            serde_json::from_str(d.body()).map_err(|_| "execution qualification body")?;
        q.validate()?;
        if !seen.insert((q.runtime.id.clone(), q.runtime.version.clone()))
            || !config
                .qualified_runtimes
                .iter()
                .any(|r| r.runtime == q.runtime && r.qualification == q.runtime_qualification)
            || !config
                .definition_bodies
                .iter()
                .any(|p| p.artifact() == &q.proposal_policy)
        {
            return Err("unqualified execution runtime/proposal policy".into());
        }
    }
    Ok(())
}
pub(super) fn current(
    state: &AppState,
    history: &super::super::accepted_preparation::AcceptedHistory,
    runtime: &super::super::registration::RegisteredRuntime,
) -> Result<ArtifactRef, AppError> {
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    validate_catalog(config).map_err(|_| refused())?;
    for d in &config.definition_bodies {
        if !d.artifact().id.as_str().starts_with(PREFIX) {
            continue;
        }
        let q: ExecutionQualification = serde_json::from_str(d.body()).map_err(|_| refused())?;
        if q.runtime == runtime.runtime
            && q.runtime_qualification == runtime.qualification
            && q.proposal_policy == history.policy
        {
            return Ok(d.artifact().clone());
        }
    }
    Err(refused())
}

/// Production transport requires a coherent explicitly installed launch/cleanup
/// profile set. It does not advertise or select any runtime on its own.
pub(super) fn transport_enabled(state: &AppState) -> bool {
    let Some(c) = state.config.profiled_execution.as_ref() else {
        return false;
    };
    if validate_catalog(c).is_err() {
        return false;
    }
    let mut found = false;
    for d in &c.definition_bodies {
        if !d.artifact().id.as_str().starts_with(PREFIX) {
            continue;
        }
        let Ok(q) = serde_json::from_str::<ExecutionQualification>(d.body()) else {
            return false;
        };
        if !matches!(
            super::super::phase_terminal::qualification::profile(
                c,
                &q.runtime,
                &q.runtime_qualification
            ),
            Ok(Some(_))
        ) {
            return false;
        }
        found = true;
    }
    found
}
