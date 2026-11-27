use super::*;
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
const PREFIX: &str = super::EXECUTION_PREFIX;
pub use super::AdapterSet;
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
            || (self.contract
                != phase::contract_artifact().map_err(|_| "phase execution contract")?
                && self.contract
                    != phase::long_contract_artifact().map_err(|_| "long execution contract")?)
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
    fn has_qualified_owners(&self, config: &super::super::config::ProfiledConfig) -> bool {
        config
            .qualified_runtimes
            .iter()
            .any(|r| r.runtime == self.runtime && r.qualification == self.runtime_qualification)
            && config.definition_bodies.iter().any(|p| {
                p.artifact() == &self.proposal_policy
                    && serde_json::from_str::<serde_json::Value>(p.body())
                        .ok()
                        .is_some_and(|value| value["contract"] == serde_json::json!(self.contract))
            })
    }
}
pub(crate) fn validate_catalog(
    config: &super::super::config::ProfiledConfig,
) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for d in execution_definitions(config) {
        if d.body().len() > 8192 {
            return Err("execution qualification byte bound".into());
        }
        let q: ExecutionQualification =
            serde_json::from_str(d.body()).map_err(|_| "execution qualification body")?;
        q.validate()?;
        if !seen.insert((q.runtime.id.clone(), q.runtime.version.clone()))
            || !q.has_qualified_owners(config)
        {
            return Err("unqualified execution runtime/proposal policy".into());
        }
    }
    Ok(())
}
pub(super) fn current(
    state: &AppState,
    history: &accepted_owner::AcceptedHistory,
    runtime: &super::super::registration::RegisteredRuntime,
) -> Result<ArtifactRef, AppError> {
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(|| refused())?;
    validate_catalog(config).map_err(|_| refused())?;
    for d in execution_definitions(config) {
        let q: ExecutionQualification = serde_json::from_str(d.body()).map_err(|_| refused())?;
        if q.runtime == runtime.runtime
            && q.runtime_qualification == runtime.qualification
            && q.proposal_policy == history.policy
            && q.contract == history.proposal.negotiation.contract
        {
            return Ok(d.artifact().clone());
        }
    }
    Err(refused())
}

/// Production transport requires a coherent explicitly installed launch/cleanup
/// profile set. It does not advertise or select any runtime on its own.
pub(crate) fn transport_enabled(state: &AppState) -> bool {
    let Some(c) = state.config.profiled_execution.as_ref() else {
        return false;
    };
    if validate_catalog(c).is_err() {
        return false;
    }
    let mut found = false;
    for d in execution_definitions(c) {
        let Ok(q) = serde_json::from_str::<ExecutionQualification>(d.body()) else {
            return false;
        };
        if !matches!(
            super::terminal_profile(c, &q.runtime, &q.runtime_qualification),
            Ok(Some(_))
        ) {
            return false;
        }
        found = true;
    }
    found
}

fn execution_definitions(
    config: &super::super::config::ProfiledConfig,
) -> impl Iterator<Item = &super::super::definitions::Definition> {
    config
        .definition_bodies
        .iter()
        .filter(|definition| definition.artifact().id.as_str().starts_with(PREFIX))
}
