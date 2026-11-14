use crate::*;
/// Convert a recomputed bounded decision to the independently published human
/// API selection shape. Provenance is data, not a claim of daemon authority.
pub fn human_selection(
    catalog: &aq_profiles::ValidatedCatalog,
    input: &SelectionInputs,
    outcome: &SelectionOutcome,
) -> Result<aq_session_api::Selection> {
    ensure(
        digest(&select(catalog, input)?)? == digest(outcome)?,
        "explanation must match recomputed decision",
    )?;
    let source = catalog
        .source("intelligence", input.intelligence_key())
        .map_err(|e| SelectionError::Invalid(e.to_string()))?;
    let definition = catalog
        .artifact()
        .catalog
        .intelligence
        .iter()
        .find(|i| aq_profiles::key(&i.id, i.version) == input.intelligence_key())
        .ok_or_else(|| SelectionError::Invalid("intelligence source".into()))?;
    let candidates = outcome
        .traces
        .iter()
        .map(|trace| {
            let reasoning = definition
                .candidates
                .iter()
                .find(|c| c.id == trace.profile_candidate_id)
                .map(|c| c.reasoning_configuration_reference.clone())
                .ok_or_else(|| SelectionError::Invalid("candidate source".into()))?;
            let observations = trace
                .observation_ids
                .iter()
                .map(|id| {
                    let observation = input
                        .observations
                        .iter()
                        .find(|o| o.observation_id == *id)
                        .ok_or_else(|| SelectionError::Invalid("observation provenance".into()))?;
                    Ok(aq_session_api::Observation {
                        source: aq_session_api::Source {
                            identity: format!("availability:{}", observation.observation_id),
                            revision: observation.revision.to_string(),
                            digest: digest(observation)?.as_str().into(),
                        },
                        sequence: observation.sequence,
                        observed_at_ms: observation.received_at_ms,
                        expires_at_ms: observation.expires_at_ms,
                        account_profile_reference: format!(
                            "{}@{}/account:{}",
                            observation.scope.credential_profile.id.as_str(),
                            observation.scope.credential_profile.version.as_str(),
                            observation.scope.account_reference
                        ),
                        endpoint_reference: observation.scope.endpoint_reference.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let eligibility = match trace.state {
                CandidateState::Eligible => aq_session_api::Eligibility::Eligible,
                CandidateState::MissingCredentials => aq_session_api::Eligibility::Unavailable,
                CandidateState::Unavailable => aq_session_api::Eligibility::Unavailable,
                CandidateState::Unsupported => aq_session_api::Eligibility::Unsupported,
                CandidateState::StaleUnknown => aq_session_api::Eligibility::StaleUnknown,
                CandidateState::PolicyDenied => aq_session_api::Eligibility::PolicyDenied,
            };
            Ok(aq_session_api::Candidate {
                candidate_id: trace.candidate_id.clone(),
                provider: trace.provider.clone(),
                model: trace.model.clone(),
                harness: trace.harness.clone(),
                reasoning_configuration_reference: reasoning,
                eligibility,
                reason_codes: trace.reason_codes.clone(),
                observations,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(aq_session_api::Selection {
        intelligence_profile: source,
        selected_candidate_id: outcome
            .resolved
            .as_ref()
            .map(|r| r.choice.candidate_id.clone()),
        snapshot_digest: digest(input)?.as_str().into(),
        decision_trace: outcome.reason_codes.clone(),
        candidates,
    })
}
