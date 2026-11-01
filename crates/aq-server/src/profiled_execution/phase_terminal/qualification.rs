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
        if !seen.insert(
            aq_selection::digest(&(&q.runtime, &q.runtime_qualification))
                .map_err(|_| "terminal qualification identity")?,
        ) {
            return Err("duplicate terminal runtime qualification".into());
        }
    }
    Ok(())
}
pub(crate) fn profile(
    c: &super::super::config::ProfiledConfig,
    runtime: &ArtifactRef,
    qualification: &ArtifactRef,
) -> Result<Option<ArtifactRef>, String> {
    validate_catalog(c)?;
    for d in &c.definition_bodies {
        if !d.artifact().id.as_str().starts_with(PREFIX) {
            continue;
        }
        let q: TerminalQualification =
            serde_json::from_str(d.body()).map_err(|_| "terminal qualification body")?;
        if &q.runtime == runtime && &q.runtime_qualification == qualification {
            return Ok(Some(d.artifact().clone()));
        }
    }
    Ok(None)
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
    profile(c, &h.metadata.request.runtime, &h.metadata.qualification)
        .map_err(|_| conflict())?
        .ok_or_else(conflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiled_execution::{config::ProfiledConfig, definitions::Definition};
    use aq_admission::{ArtifactKind, Digest, Text};
    fn reference(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        }
    }
    fn qualification() -> TerminalQualification {
        TerminalQualification {
            schema_version: 1,
            contract: aq_phase_terminal_protocol::contract_artifact().unwrap(),
            runtime: reference("synthetic-runtime"),
            runtime_qualification: reference("synthetic-runtime-qualification"),
            proof_producer: reference("synthetic-proof-producer"),
        }
    }
    fn definition(id: &str, q: &TerminalQualification) -> Definition {
        let body = serde_json::to_string(q).unwrap();
        Definition::new(
            ArtifactRef {
                id: Text::new(format!("{PREFIX}{id}")).unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(body.as_bytes()),
            },
            ArtifactKind::Definition,
            body,
        )
        .unwrap()
    }
    fn config(definition_bodies: Vec<Definition>) -> ProfiledConfig {
        ProfiledConfig {
            revision: 1,
            qualified_runtimes: vec![],
            projects: vec![],
            context_qualifications: vec![],
            definition_bodies,
        }
    }
    #[test]
    fn unsupported_contract_and_unknown_policy_fields_refuse() {
        let mut q = qualification();
        q.contract.version = Text::new("2").unwrap();
        assert!(validate_catalog(&config(vec![definition("unsupported", &q)])).is_err());
        let mut body = serde_json::to_value(qualification()).unwrap();
        body["authorize"] = true.into();
        assert!(serde_json::from_value::<TerminalQualification>(body).is_err());
    }
    #[test]
    fn retired_runtime_cleanup_profile_remains_explicit_and_unambiguous() {
        let first = qualification();
        let mut second = first.clone();
        second.runtime_qualification = reference("synthetic-second-qualification");
        // Keeping historical verifier definitions does not restore launch eligibility.
        let c = config(vec![
            definition("first", &first),
            definition("second", &second),
        ]);
        c.validate().unwrap();
        assert!(c.qualified_runtimes.is_empty());
        assert!(
            validate_catalog(&config(vec![
                definition("one", &first),
                definition("duplicate", &first)
            ]))
            .is_err()
        );
    }
    #[test]
    fn oversized_policy_body_refuses_before_owner_lookup() {
        for (length, valid) in [(4096, true), (4097, false)] {
            let mut body = serde_json::to_string(&qualification()).unwrap();
            body.extend(std::iter::repeat_n(' ', length - body.len()));
            let d = Definition::new(
                ArtifactRef {
                    id: Text::new(format!("{PREFIX}bounded")).unwrap(),
                    version: Text::new("1").unwrap(),
                    digest: Digest::of(body.as_bytes()),
                },
                ArtifactKind::Definition,
                body,
            )
            .unwrap();
            assert_eq!(validate_catalog(&config(vec![d])).is_ok(), valid);
        }
    }
}
