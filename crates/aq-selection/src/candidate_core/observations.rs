use super::*;
use std::collections::BTreeMap;
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
impl ObservationTarget {
    fn accepts_source(&self, source: &ObservationSource) -> bool {
        match source {
            ObservationSource::Operator { .. } => true,
            ObservationSource::Worker {
                node_id, boot_id, ..
            } => {
                matches!(self, ObservationTarget::Worker { node_id: current, boot_id: boot } if current == node_id && boot == boot_id)
            }
        }
    }
}
fn matching_observations<'a>(
    input: &'a CoreInputs<'_>,
    credential: &AuthorizedCredential,
    model: &str,
) -> Result<BTreeMap<String, &'a AvailabilityObservation>> {
    let mut groups = BTreeMap::<String, &AvailabilityObservation>::new();
    for observation in input.observations {
        if !input.target.accepts_source(&observation.source) {
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
    Ok(groups)
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Evidence {
    Unknown,
    Available,
    Unavailable,
    Unsupported,
    MissingCredentials,
}
impl Evidence {
    fn from_observation(observation: &AvailabilityObservation, now: u64, max_age: u64) -> Self {
        if !fresh(observation.received_at_ms, observation.expires_at_ms, now) {
            return Self::Unknown;
        }
        if now - observation.received_at_ms > max_age && observation.cooldown_until_ms <= now {
            return Self::Unknown;
        }
        match observation.state {
            Availability::Available => Self::Available,
            Availability::AuthenticationFailed => Self::MissingCredentials,
            Availability::ModelUnsupported => Self::Unsupported,
            Availability::QuotaUnavailable | Availability::TemporaryUnavailable => {
                Self::Unavailable
            }
        }
    }

    fn decision(self, allow_unknown: bool) -> (CandidateState, &'static str) {
        match self {
            Self::MissingCredentials => (
                CandidateState::MissingCredentials,
                "provider_authentication_failed",
            ),
            Self::Unsupported => (CandidateState::Unsupported, "provider_model_unsupported"),
            Self::Unavailable => (
                CandidateState::Unavailable,
                "provider_cooldown_or_unavailable",
            ),
            Self::Available => (CandidateState::Eligible, "availability_observed"),
            Self::Unknown if allow_unknown => (
                CandidateState::Eligible,
                "unknown_availability_bounded_attempt",
            ),
            Self::Unknown => (
                CandidateState::StaleUnknown,
                "availability_stale_or_unknown",
            ),
        }
    }
}
pub(super) fn availability(
    input: &CoreInputs<'_>,
    credential: &AuthorizedCredential,
    model: &str,
    policy: &SelectionPolicy,
) -> Result<(CandidateState, Vec<String>, Vec<uuid::Uuid>)> {
    let observations = matching_observations(input, credential, model)?;
    let mut ids = Vec::with_capacity(observations.len());
    let mut strongest = Evidence::Unknown;
    for observation in observations.into_values() {
        ids.push(observation.observation_id);
        strongest = strongest.max(Evidence::from_observation(
            observation,
            input.now_ms,
            policy.observation_max_age_ms,
        ));
    }
    ids.sort();
    let (state, reason) = strongest.decision(policy.allow_unknown_availability);
    Ok((state, vec![reason.into()], ids))
}
/// Expiration of every influencing observation may only shorten a decision.
pub(crate) fn influencing_expiry(
    input: &SelectionInputs,
    policy: &SelectionPolicy,
    traces: &[CandidateTrace],
    mut expires: u64,
) -> u64 {
    for id in traces.iter().flat_map(|t| &t.observation_ids) {
        if let Some(o) = input.observations.iter().find(|o| &o.observation_id == id) {
            expires = o.next_deadline(input.now_ms, policy.observation_max_age_ms, expires);
        }
    }
    expires
}
impl AvailabilityObservation {
    fn next_deadline(&self, now_ms: u64, max_age: u64, current: u64) -> u64 {
        let age = self
            .received_at_ms
            .saturating_add(max_age)
            .saturating_add(1);
        let effective_age = if age > now_ms {
            age
        } else {
            self.cooldown_until_ms
        };
        [self.expires_at_ms, effective_age]
            .into_iter()
            .filter(|&time| time > now_ms)
            .fold(current, u64::min)
    }
}
