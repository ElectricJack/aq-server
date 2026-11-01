//! One subject-neutral candidate evaluator. Domain wrappers own admission facts.
use crate::validate::fresh;
use crate::*;
use aq_profiles::{FallbackReason, SelectionPolicy};
use aq_session_protocol::require_capabilities;
use std::collections::BTreeMap;
pub(crate) enum ObservationTarget {
    Worker {
        node_id: uuid::Uuid,
        boot_id: uuid::Uuid,
    },
    CentralInstallation,
}
pub(crate) struct CoreComposition<'a> {
    pub digest: &'a Digest,
    pub harness: &'a str,
    pub environment: &'a str,
    pub context_limit_bytes: u64,
    pub startup_bytes: u64,
}
pub(crate) struct CoreInstallation<'a> {
    pub harnesses: &'a [InstalledHarness],
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
pub(crate) type CandidateResult = std::result::Result<(), (CandidateState, &'static str)>;
pub(crate) type DeliveryCheck<'a> =
    dyn Fn(&AuthorizedCredential) -> Option<Option<CredentialDelivery>> + 'a;
pub(crate) type CostCheck<'a> =
    dyn Fn(&InstalledHarness, Option<&CandidateEstimate>) -> CandidateResult + 'a;
pub(crate) struct CoreInputs<'a> {
    pub target: ObservationTarget,
    pub installation: CoreInstallation<'a>,
    pub credentials: &'a [AuthorizedCredential],
    pub observations: &'a [AvailabilityObservation],
    pub estimates: &'a [CandidateEstimate],
    pub compositions: &'a [CoreComposition<'a>],
    pub ceilings: &'a ResourceCeilings,
    pub allowed_environments: &'a [String],
    pub now_ms: u64,
    pub global: Option<&'static str>,
    pub environment_denied: &'static str,
    pub installation_stale: &'static str,
    pub delivery: &'a DeliveryCheck<'a>,
    pub cost: &'a CostCheck<'a>,
}
pub(crate) struct CoreOutcome {
    pub selected: Option<(ConcreteChoice, u64, usize)>,
    pub traces: Vec<CandidateTrace>,
    pub reason_codes: Vec<String>,
}
pub(crate) fn profile_key(profile: &aq_session_protocol::ArtifactRef) -> String {
    format!("{}@{}", profile.id.as_str(), profile.version.as_str())
}
fn scope_matches(
    scope: &ObservationScope,
    credential: &AuthorizedCredential,
    model: &str,
    target: &ObservationTarget,
) -> bool {
    scope.provider == credential.provider
        && scope.credential_profile == credential.profile
        && scope.credential_revision == credential.revision
        && scope.account_reference == credential.account_reference
        && scope.endpoint_reference == credential.endpoint_reference
        && scope.model.as_deref().is_none_or(|m| m == model)
        && match (&scope.node_id, target) {
            (None, _) => true,
            (Some(id), ObservationTarget::Worker { node_id, .. }) => id == node_id,
            _ => false,
        }
}
fn availability(
    input: &CoreInputs<'_>,
    credential: &AuthorizedCredential,
    model: &str,
    policy: &SelectionPolicy,
) -> Result<(CandidateState, Vec<String>, Vec<uuid::Uuid>)> {
    let mut groups = BTreeMap::<String, &AvailabilityObservation>::new();
    for observation in input.observations {
        if let ObservationSource::Worker {
            node_id, boot_id, ..
        } = &observation.source
            && !matches!(&input.target,ObservationTarget::Worker{node_id:current_node,boot_id:current_boot} if current_node==node_id && current_boot==boot_id)
        {
            continue;
        }
        if !scope_matches(&observation.scope, credential, model, &input.target) {
            continue;
        }
        let scope = digest(&(&observation.scope, &observation.source))?
            .as_str()
            .to_string();
        let rank = |o: &AvailabilityObservation| (o.sequence, o.received_at_ms, o.observation_id);
        if groups
            .get(&scope)
            .is_none_or(|old| rank(observation) > rank(old))
        {
            groups.insert(scope, observation);
        }
    }
    let mut positive = false;
    let mut ids = Vec::new();
    let mut blocked = None;
    for observation in groups.into_values() {
        ids.push(observation.observation_id);
        let timely = fresh(
            observation.received_at_ms,
            observation.expires_at_ms,
            input.now_ms,
        );
        let cooldown = timely && observation.cooldown_until_ms > input.now_ms;
        let observed =
            timely && input.now_ms - observation.received_at_ms <= policy.observation_max_age_ms;
        if !observed && !cooldown {
            continue;
        }
        let state = match observation.state {
            Availability::Available => {
                positive = true;
                continue;
            }
            Availability::AuthenticationFailed => CandidateState::MissingCredentials,
            Availability::ModelUnsupported => CandidateState::Unsupported,
            Availability::QuotaUnavailable | Availability::TemporaryUnavailable => {
                CandidateState::Unavailable
            }
        };
        // Stable, conservative precedence across independent matching scopes.
        let priority = |s: CandidateState| match s {
            CandidateState::MissingCredentials => 3,
            CandidateState::Unsupported => 2,
            _ => 1,
        };
        if blocked.is_none_or(|old| priority(state) > priority(old)) {
            blocked = Some(state);
        }
    }
    ids.sort();
    if let Some(state) = blocked {
        return Ok((
            state,
            vec![
                match state {
                    CandidateState::MissingCredentials => "provider_authentication_failed",
                    CandidateState::Unsupported => "provider_model_unsupported",
                    _ => "provider_cooldown_or_unavailable",
                }
                .into(),
            ],
            ids,
        ));
    }
    if positive {
        return Ok((
            CandidateState::Eligible,
            vec!["availability_observed".into()],
            ids,
        ));
    }
    if policy.allow_unknown_availability {
        Ok((
            CandidateState::Eligible,
            vec!["unknown_availability_bounded_attempt".into()],
            ids,
        ))
    } else {
        Ok((
            CandidateState::StaleUnknown,
            vec!["availability_stale_or_unknown".into()],
            ids,
        ))
    }
}
fn fallback(state: CandidateState) -> Option<FallbackReason> {
    match state {
        CandidateState::Eligible => None,
        CandidateState::MissingCredentials => Some(FallbackReason::MissingCredentials),
        CandidateState::Unavailable => Some(FallbackReason::Unavailable),
        CandidateState::Unsupported => Some(FallbackReason::Unsupported),
        CandidateState::StaleUnknown => Some(FallbackReason::StaleUnknown),
        CandidateState::PolicyDenied => Some(FallbackReason::PolicyDenied),
    }
}
fn trace(
    id: String,
    c: &aq_profiles::ModelCandidate,
    harness: &str,
    environment: &str,
    credential: Option<&AuthorizedCredential>,
    state: CandidateState,
    reason: &str,
) -> CandidateTrace {
    CandidateTrace {
        candidate_id: id,
        profile_candidate_id: c.id.clone(),
        provider: c.provider.clone(),
        model: c.model.clone(),
        harness: harness.into(),
        environment: environment.into(),
        credential_profile: credential.map(|c| c.profile.clone()),
        state,
        reason_codes: vec![reason.into()],
        observation_ids: vec![],
    }
}
pub(crate) fn evaluate(
    intelligence: &aq_profiles::Intelligence,
    execution: &aq_profiles::Execution,
    policy: &SelectionPolicy,
    input: &CoreInputs<'_>,
) -> Result<CoreOutcome> {
    let global = input.global;
    let mut rows: Vec<(CandidateTrace, Option<ConcreteChoice>, u64)> = Vec::new();
    let mut harnesses: Vec<_> = input.installation.harnesses.iter().collect();
    harnesses.sort_by_key(|h| {
        (
            &h.harness,
            h.environment.digest.clone(),
            h.adapter.digest.clone(),
            h.cli_version.clone(),
        )
    });
    for candidate in &intelligence.candidates {
        let before = rows.len();
        for name in &execution.harnesses {
            if !candidate.harnesses.contains(name) {
                continue;
            }
            for harness in harnesses.iter().filter(|h| {
                &h.harness == name
                    && (execution
                        .environments
                        .contains(&profile_key(&h.environment))
                        || execution
                            .environments
                            .iter()
                            .any(|e| e == h.environment.id.as_str()))
            }) {
                let Some(composition) = input.compositions.iter().find(|c| {
                    c.harness == name.as_str()
                        && (c.environment == harness.environment.id.as_str()
                            || c.environment == profile_key(&harness.environment))
                }) else {
                    continue;
                };
                let references = if execution.credential_profile_references.is_empty() {
                    vec!["unconfigured".to_string()]
                } else {
                    execution.credential_profile_references.clone()
                };
                for reference in references {
                    if input.credentials.iter().any(|c| {
                        profile_key(&c.profile) == reference && c.provider != candidate.provider
                    }) {
                        continue;
                    }
                    ensure(rows.len() < 128, "candidate fanout exceeds published bound")?;
                    let credential = input.credentials.iter().find(|c| {
                        profile_key(&c.profile) == reference && c.provider == candidate.provider
                    });
                    let id = format!(
                        "choice:{}",
                        digest(&(
                            candidate.id.as_str(),
                            &harness.adapter,
                            &harness.environment,
                            &harness.cli_version,
                            &reference
                        ))?
                        .as_str()
                    );
                    let mut row = trace(
                        id,
                        candidate,
                        name,
                        &profile_key(&harness.environment),
                        credential,
                        CandidateState::Eligible,
                        "candidate_evaluated",
                    );
                    let mut choice = None;
                    let evaluated =
                        (|| -> std::result::Result<(), (CandidateState, &'static str)> {
                            if let Some(reason) = global {
                                return Err((CandidateState::PolicyDenied, reason));
                            }
                            if !input.allowed_environments.iter().any(|e| {
                                e == harness.environment.id.as_str()
                                    || e == &profile_key(&harness.environment)
                            }) {
                                return Err((
                                    CandidateState::PolicyDenied,
                                    input.environment_denied,
                                ));
                            }
                            if !fresh(
                                input.installation.observed_at_ms,
                                input.installation.expires_at_ms,
                                input.now_ms,
                            ) {
                                return Err((
                                    CandidateState::StaleUnknown,
                                    input.installation_stale,
                                ));
                            }
                            let credential = credential.ok_or((
                                CandidateState::MissingCredentials,
                                "credential_profile_unregistered",
                            ))?;
                            if !credential.enabled {
                                return Err((
                                    CandidateState::PolicyDenied,
                                    "credential_profile_disabled",
                                ));
                            }
                            if !credential.provisionable {
                                return Err((
                                    CandidateState::MissingCredentials,
                                    "credential_not_provisionable",
                                ));
                            }
                            let missing = if credential.material_owner.is_some() {
                                "credential_preparation_missing_or_stale"
                            } else {
                                "installed_credential_missing_or_stale"
                            };
                            let delivery = (input.delivery)(credential)
                                .ok_or((CandidateState::MissingCredentials, missing))?;
                            require_capabilities(
                                &policy.required_capabilities,
                                &harness.capabilities,
                            )
                            .map_err(|_| {
                                (
                                    CandidateState::Unsupported,
                                    "required_capability_unsupported",
                                )
                            })?;
                            let model = harness
                                .models
                                .iter()
                                .find(|m| {
                                    m.provider == candidate.provider
                                        && m.model == candidate.model
                                        && m.reasoning_configuration_reference
                                            == candidate.reasoning_configuration_reference
                                })
                                .ok_or((
                                    CandidateState::Unsupported,
                                    "model_or_reasoning_unsupported",
                                ))?;
                            let limits = &harness.limits;
                            if limits.context_bytes == 0
                                || limits.wall_time_ms == 0
                                || limits.memory_bytes == 0
                                || limits.cpu_millis == 0
                                || limits.process_limit == 0
                                || limits.max_output_tokens < policy.max_output_tokens
                            {
                                return Err((
                                    CandidateState::Unsupported,
                                    "required_native_limit_unsupported",
                                ));
                            }
                            let mut resources = input.ceilings.clone();
                            resources.context_bytes = resources
                                .context_bytes
                                .min(limits.context_bytes)
                                .min(composition.context_limit_bytes);
                            resources.wall_time_ms =
                                resources.wall_time_ms.min(limits.wall_time_ms);
                            resources.memory_bytes =
                                resources.memory_bytes.min(limits.memory_bytes);
                            resources.cpu_millis = resources.cpu_millis.min(limits.cpu_millis);
                            resources.process_limit =
                                resources.process_limit.min(limits.process_limit);
                            let bytes = composition.startup_bytes;
                            if bytes > resources.context_bytes {
                                return Err((
                                    CandidateState::Unsupported,
                                    "required_context_limit_unsupported",
                                ));
                            }
                            let estimate = input.estimates.iter().find(|e| {
                                e.credential_profile == credential.profile
                                    && e.provider == candidate.provider
                                    && e.model == candidate.model
                            });
                            (input.cost)(harness, estimate)?;
                            let (state, reasons, ids) =
                                availability(input, credential, &candidate.model, policy).map_err(
                                    |_| (CandidateState::PolicyDenied, "observation_invalid"),
                                )?;
                            row.state = state;
                            row.reason_codes = reasons;
                            row.observation_ids = ids;
                            if state != CandidateState::Eligible {
                                return Ok(());
                            }
                            if delivery.is_some() {
                                row.reason_codes
                                    .push("material_preparation_required".into());
                            }
                            choice = Some(ConcreteChoice {
                                candidate_id: row.candidate_id.clone(),
                                profile_candidate_id: candidate.id.clone(),
                                provider: candidate.provider.clone(),
                                model: candidate.model.clone(),
                                native_reasoning: model.native_reasoning.clone(),
                                reasoning_configuration_reference: candidate
                                    .reasoning_configuration_reference
                                    .clone(),
                                harness: harness.harness.clone(),
                                cli_version: harness.cli_version.clone(),
                                adapter: harness.adapter.clone(),
                                environment: harness.environment.clone(),
                                qualification: harness.qualification.clone(),
                                failure_classifier: harness.failure_classifier.clone(),
                                credential_profile: credential.profile.clone(),
                                credential_delivery: delivery,
                                account_reference: credential.account_reference.clone(),
                                endpoint_reference: credential.endpoint_reference.clone(),
                                composition_digest: composition.digest.clone(),
                                resources,
                                max_output_tokens: policy.max_output_tokens,
                                estimate: estimate.cloned(),
                            });
                            Ok(())
                        })();
                    if let Err((state, reason)) = evaluated {
                        row.state = state;
                        row.reason_codes = vec![reason.into()];
                    }
                    rows.push((row, choice, credential.map_or(0, |c| c.revision)));
                }
            }
        }
        if before == rows.len() {
            ensure(rows.len() < 128, "candidate fanout exceeds published bound")?;
            let id = format!(
                "choice:{}",
                digest(&(&candidate.id, "no-compatible-composition"))?.as_str()
            );
            rows.push((
                trace(
                    id,
                    candidate,
                    "unavailable",
                    "unavailable",
                    None,
                    CandidateState::Unsupported,
                    "compatible_installed_composition_unavailable",
                ),
                None,
                0,
            ));
        }
    }
    let mut blocked = false;
    let mut selected = None;
    for (index, (row, choice, _)) in rows.iter_mut().enumerate() {
        if selected.is_some() {
            continue;
        }
        if blocked {
            row.state = CandidateState::PolicyDenied;
            row.reason_codes.push("fallback_not_permitted".into());
            continue;
        }
        if row.state == CandidateState::Eligible && choice.is_some() {
            selected = Some(index);
        } else if fallback(row.state).is_some_and(|reason| !policy.fallback_on.contains(&reason)) {
            blocked = true;
        }
    }
    let traces: Vec<_> = rows.iter().map(|(trace, _, _)| trace.clone()).collect();
    let resolved = selected.map(|index| (rows[index].1.clone().unwrap(), rows[index].2, index));
    Ok(CoreOutcome {
        reason_codes: if resolved.is_some() {
            vec![]
        } else if blocked {
            vec!["no_permitted_fallback".into()]
        } else {
            vec!["no_eligible_candidate".into()]
        },
        selected: resolved,
        traces,
    })
}
/// Expiration of every influencing observation may only shorten a decision.
pub(crate) fn influencing_expiry(
    observations: &[AvailabilityObservation],
    now_ms: u64,
    policy: &SelectionPolicy,
    traces: &[CandidateTrace],
    index: usize,
    mut expires: u64,
) -> u64 {
    for id in traces
        .iter()
        .take(index + 1)
        .flat_map(|t| &t.observation_ids)
    {
        if let Some(o) = observations.iter().find(|o| &o.observation_id == id) {
            if o.expires_at_ms > now_ms {
                expires = expires.min(o.expires_at_ms);
            }
            let age = o
                .received_at_ms
                .saturating_add(policy.observation_max_age_ms)
                .saturating_add(1);
            if age > now_ms {
                expires = expires.min(age);
            } else if o.cooldown_until_ms > now_ms {
                expires = expires.min(o.cooldown_until_ms);
            }
        }
    }
    expires
}
