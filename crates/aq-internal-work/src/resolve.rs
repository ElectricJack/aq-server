//! Resolving a level (an intelligence profile document, spec 0014 §5.2) to
//! the ordered candidates internal work tries (spec 0012 §5, §6). Pure: the
//! role snapshot, registry and observations come in as arguments.
use crate::{Candidate, Reason, Way};
use aq_intelligence_config::LoadedRegistry;
use aq_profiles::roles::Snapshot;
use aq_selection::{
    AuthorizedCredential, Availability, AvailabilityObservation, CredentialKind, ObservationSource,
};
use std::collections::BTreeMap;

/// The level and the candidates left to try, in the level's order.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub profile_id: String,
    pub candidates: Vec<Candidate>,
}

/// The harness a logged-in run can use on the server, as later tasks claim
/// work by it: `codex` or `claude`.
fn login_harness(candidate: &aq_profiles::roles::Candidate) -> Option<&'static str> {
    candidate.harnesses.iter().find_map(|h| match h.as_str() {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        _ => None,
    })
}

/// The availability observations as evaluated at one instant.
pub struct ObservedAt<'a> {
    pub observations: &'a [AvailabilityObservation],
    pub now_ms: u64,
}

/// Same meaning as `aq_selection`'s observation evaluation: the newest
/// observation per scope and source decides. A fresh one counts while it is
/// within `max_age_ms` of being received (the level's
/// `observation_max_age_ms`) or while its cooldown has not passed.
/// Observations a worker made on its own node do not describe the server's
/// use of the credential.
impl ObservedAt<'_> {
    fn blocked(&self, credential: &AuthorizedCredential, model: &str, max_age_ms: u64) -> bool {
        let mut newest = BTreeMap::<String, &AvailabilityObservation>::new();
        for o in self.observations {
            let Some(key) = matching_scope(o, credential, model) else {
                continue;
            };
            let rank =
                |o: &AvailabilityObservation| (o.sequence, o.received_at_ms, o.observation_id);
            if newest.get(&key).is_none_or(|old| rank(o) > rank(old)) {
                newest.insert(key, o);
            }
        }
        newest
            .values()
            .any(|o| blocks_at(o, self.now_ms, max_age_ms))
    }
}

fn blocks_at(o: &AvailabilityObservation, now_ms: u64, max_age_ms: u64) -> bool {
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
}

fn matching_scope(
    o: &AvailabilityObservation,
    credential: &AuthorizedCredential,
    model: &str,
) -> Option<String> {
    let scope = &o.scope;
    let matches = matches!(o.source, ObservationSource::Operator { .. })
        && scope.node_id.is_none()
        && scope.provider == credential.provider
        && scope.credential_profile == credential.profile
        && scope.credential_revision == credential.revision
        && scope.account_reference == credential.account_reference
        && scope.endpoint_reference == credential.endpoint_reference
        && scope.model.as_deref().is_none_or(|m| m == model);
    matches
        .then(|| serde_json::to_string(&(scope, &o.source)).ok())
        .flatten()
}

/// The candidates of the level `profile_id` that can be tried now, each with
/// the reasoning effort its document states; `default` leaves the harness its
/// own setting. A candidate without a usable credential is dropped. A missing
/// level, or one with nothing usable left, is [`Reason::NoCandidate`].
pub fn resolve(
    snapshot: &Snapshot,
    registry: &LoadedRegistry,
    observed: ObservedAt<'_>,
    profile_id: &str,
) -> Result<Resolved, Reason> {
    let profile = snapshot
        .intelligence(profile_id)
        .ok_or(Reason::NoCandidate)?;
    let max_age_ms = snapshot
        .selection_defaults()
        .with(&profile.selection)
        .observation_max_age_ms;
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
        if observed.blocked(credential, &entry.model, max_age_ms) {
            continue;
        }
        candidates.push(Candidate {
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            reasoning: (entry.reasoning != "default").then(|| entry.reasoning.clone()),
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
        candidates,
    })
}
