//! Protected operator qualification. Historical cleanup is independent of active launch eligibility.
use super::{super::closure::history::RegistrationHistory, common::conflict};
use crate::{AppState, error::AppError};
use aq_admission::ArtifactRef;
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
const PREFIX: &str = "aq.phase-terminal-qualification/";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalQualification {
    pub schema_version: u32,
    pub contract: ArtifactRef,
    pub runtime: ArtifactRef,
    pub runtime_qualification: ArtifactRef,
    /// Exact independently qualified opaque host/proof producer, not a DTO claim.
    pub proof_producer: ArtifactRef,
}
impl TerminalQualification {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.contract
                != aq_phase_terminal_protocol::contract_artifact()
                    .map_err(|_| "terminal contract")?
        {
            return Err("terminal contract mismatch".into());
        }
        for a in [
            &self.runtime,
            &self.runtime_qualification,
            &self.proof_producer,
        ] {
            a.id.validate().map_err(|_| "terminal artifact id")?;
            a.version
                .validate()
                .map_err(|_| "terminal artifact version")?;
            if a.id.as_str().len() > 512 || a.version.as_str().len() > 512 {
                return Err("terminal artifact bounds".into());
            }
        }
        Ok(())
    }
}
pub(crate) fn validate_catalog(c: &super::super::config::ProfiledConfig) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for d in &c.definition_bodies {
        if !d.artifact().id.as_str().starts_with(PREFIX) {
            continue;
        }
        if d.body().len() > 4096 {
            return Err("terminal qualification bound".into());
        }
        let q: TerminalQualification =
            serde_json::from_str(d.body()).map_err(|_| "terminal qualification body")?;
        q.validate()?;
        if !seen.insert((q.runtime.id.clone(), q.runtime.version.clone())) {
            return Err("duplicate terminal runtime qualification".into());
        }
    }
    Ok(())
}
pub(super) fn for_history(
    state: &AppState,
    h: &RegistrationHistory,
) -> Result<ArtifactRef, AppError> {
    let c = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(conflict)?;
    validate_catalog(c).map_err(|_| conflict())?;
    for d in &c.definition_bodies {
        if !d.artifact().id.as_str().starts_with(PREFIX) {
            continue;
        }
        let q: TerminalQualification = serde_json::from_str(d.body()).map_err(|_| conflict())?;
        if q.runtime == h.metadata.request.runtime
            && q.runtime_qualification == h.metadata.qualification
        {
            return Ok(d.artifact().clone());
        }
    }
    Err(conflict())
}
