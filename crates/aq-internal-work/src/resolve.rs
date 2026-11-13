//! Resolving an intelligence profile to the ordered candidates internal work
//! tries (spec 0012 §5, §6). Pure: catalog, registry, model choices and
//! observations come in as arguments.
use crate::{Candidate, Reason, Way};
use aq_intelligence_config::LoadedRegistry;
use aq_profile_settings::ModelChoice;
use aq_profiles::{ModelCandidate, ValidatedCatalog};
use aq_selection::{
    AuthorizedCredential, Availability, AvailabilityObservation, CredentialKind, ObservationSource,
};
use std::collections::BTreeMap;

/// The profile and the candidates left to try, in profile order.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub profile_id: String,
    pub profile_version: u32,
    pub candidates: Vec<Candidate>,
}

/// A catalog harness id as Settings' model choices name it: the catalog may
/// spell the Claude harness `claude-code`.
fn harness_name(harness: &str) -> &str {
    match harness {
        "claude-code" => "claude",
        other => other,
    }
}

/// Harness ids a logged-in run can use on the server.
/// Returned as the name later tasks claim work by: `codex` or `claude`.
fn login_harness(candidate: &ModelCandidate) -> Option<&'static str> {
    candidate
        .harnesses
        .iter()
        .find_map(|h| match harness_name(h) {
            "codex" => Some("codex"),
            "claude" => Some("claude"),
            _ => None,
        })
}

/// The model choice `candidate`'s reference names, the way manual sessions
/// resolve it ([`aq_profile_settings::choice_for`]): on the logged-in
/// `harness`, or for an API candidate on any harness the catalog lists for
/// it. `None` when no choice matches or its harness refuses its effort.
fn model_choice<'a>(
    choices: &'a [ModelChoice],
    candidate: &ModelCandidate,
    harness: Option<&str>,
) -> Option<&'a ModelChoice> {
    let names = (
        candidate.provider.as_str(),
        candidate.model.as_str(),
        candidate.reasoning_configuration_reference.as_str(),
    );
    match harness {
        Some(harness) => aq_profile_settings::choice_for(choices, names, harness),
        None => (candidate.harnesses.iter())
            .find_map(|h| aq_profile_settings::choice_for(choices, names, harness_name(h))),
    }
}

/// Same meaning as `aq_selection`'s observation evaluation: the newest
/// observation per scope and source decides. A fresh one counts while it is
/// within `max_age_ms` of being received (the profile's
/// `observation_max_age_ms`; no limit when the profile has no selection
/// policy) or while its cooldown has not passed. Observations a worker made on its own node
/// do not describe the server's use of the credential.
fn blocked(
    observations: &[AvailabilityObservation],
    credential: &AuthorizedCredential,
    model: &str,
    now_ms: u64,
    max_age_ms: u64,
) -> bool {
    let mut newest = BTreeMap::<String, &AvailabilityObservation>::new();
    for o in observations {
        let scope = &o.scope;
        let matches = matches!(o.source, ObservationSource::Operator { .. })
            && scope.node_id.is_none()
            && scope.provider == credential.provider
            && scope.credential_profile == credential.profile
            && scope.credential_revision == credential.revision
            && scope.account_reference == credential.account_reference
            && scope.endpoint_reference == credential.endpoint_reference
            && scope.model.as_deref().is_none_or(|m| m == model);
        if !matches {
            continue;
        }
        let Ok(key) = serde_json::to_string(&(scope, &o.source)) else {
            continue;
        };
        let rank = |o: &AvailabilityObservation| (o.sequence, o.received_at_ms, o.observation_id);
        if newest.get(&key).is_none_or(|old| rank(o) > rank(old)) {
            newest.insert(key, o);
        }
    }
    newest.values().any(|o| {
        let fresh = o.received_at_ms > 0 && o.received_at_ms <= now_ms && now_ms < o.expires_at_ms;
        let observed = now_ms - o.received_at_ms.min(now_ms) <= max_age_ms;
        fresh
            && (observed || o.cooldown_until_ms > now_ms)
            && matches!(
                o.state,
                Availability::QuotaUnavailable
                    | Availability::AuthenticationFailed
                    | Availability::TemporaryUnavailable
            )
    })
}

/// The candidates of `profile_id` that can be tried now, each with the
/// reasoning effort and options of the model choice in `choices` its
/// reference names. A candidate without such a choice is dropped like one
/// without a usable credential. A missing profile, or one with nothing usable
/// left, is [`Reason::NoCandidate`].
pub fn resolve(
    catalog: &ValidatedCatalog,
    registry: &LoadedRegistry,
    choices: &[ModelChoice],
    observations: &[AvailabilityObservation],
    profile_id: &str,
    now_ms: u64,
) -> Result<Resolved, Reason> {
    let profile = catalog
        .artifact()
        .catalog
        .intelligence
        .iter()
        .find(|p| p.id == profile_id)
        .ok_or(Reason::NoCandidate)?;
    let max_age_ms =
        (profile.selection_policy.as_ref()).map_or(u64::MAX, |p| p.observation_max_age_ms);
    let mut candidates = Vec::new();
    for entry in &profile.candidates {
        let Some((index, credential)) = registry
            .credentials
            .iter()
            .enumerate()
            .find(|(_, c)| c.enabled && c.provider == entry.provider)
        else {
            continue;
        };
        let (way, harness) = match registry.config.credentials.get(index).map(|c| c.kind) {
            Some(CredentialKind::ApiKey) => (Way::Api, None),
            Some(CredentialKind::SubscriptionLogin) => match login_harness(entry) {
                Some(harness) => (Way::Login, Some(harness.to_string())),
                None => continue,
            },
            None => continue,
        };
        let Some(choice) = model_choice(choices, entry, harness.as_deref()) else {
            continue;
        };
        if blocked(observations, credential, &entry.model, now_ms, max_age_ms) {
            continue;
        }
        candidates.push(Candidate {
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            reasoning: choice.reasoning.clone(),
            reasoning_reference: entry.reasoning_configuration_reference.clone(),
            options: choice.options.clone(),
            harness,
            way,
            credential_index: index,
        });
    }
    if candidates.is_empty() {
        return Err(Reason::NoCandidate);
    }
    Ok(Resolved {
        profile_id: profile.id.clone(),
        profile_version: profile.version,
        candidates,
    })
}
