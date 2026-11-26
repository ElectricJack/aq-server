//! Continuing health is not candidate selection. Unknown never means available,
//! and this helper cannot create a grant or choose another account/model.
use crate::error::AppError;
use aq_selection::{ArtifactRef, Availability, AvailabilityObservation, Digest, ObservationScope};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;
pub struct Subject<'a> {
    pub profile: &'a ArtifactRef,
    pub revision: u64,
    pub provider: &'a str,
    pub account: &'a str,
    pub endpoint: &'a str,
    pub model: &'a str,
    pub node: Uuid,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Unknown,
    ObservedAvailable,
    Denied,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub state: State,
    pub observations_digest: Digest,
    pub matching_sources: u32,
    pub evaluated_at_ms: u64,
}
fn invalid() -> AppError {
    AppError::Conflict("current continuing health evidence is inconsistent".into())
}
pub fn evaluate(
    subject: Subject<'_>,
    observations: &[AvailabilityObservation],
    maximum_age_ms: u64,
    now: u64,
) -> Result<Evidence, AppError> {
    if observations.len() > 1024 || now == 0 || maximum_age_ms == 0 || maximum_age_ms > 3_600_000 {
        return Err(invalid());
    }
    let mut latest: BTreeMap<Digest, &AvailabilityObservation> = BTreeMap::new();
    for observation in observations {
        observation.validate().map_err(|_| invalid())?;
        if !subject.matches(&observation.scope) {
            continue;
        }
        if observation.received_at_ms > now {
            return Err(invalid());
        }
        retain_latest(&mut latest, observation)?;
    }
    let state = classify(latest.values().copied(), maximum_age_ms, now);
    let matching_sources = latest.len() as u32;
    let observations_digest =
        aq_selection::digest(&latest.values().collect::<Vec<_>>()).map_err(|_| invalid())?;
    Ok(Evidence {
        state,
        observations_digest,
        matching_sources,
        evaluated_at_ms: now,
    })
}
/// Any fresh negative or active cooldown denies; otherwise a fresh positive is
/// available, and no fresh evidence stays unknown.
fn classify<'a>(
    latest: impl Iterator<Item = &'a AvailabilityObservation>,
    maximum_age_ms: u64,
    now: u64,
) -> State {
    let mut state = State::Unknown;
    for observation in latest {
        let live = now < observation.expires_at_ms;
        let fresh = live && now - observation.received_at_ms <= maximum_age_ms;
        let cooling = live && now < observation.cooldown_until_ms;
        if cooling || (fresh && observation.state != Availability::Available) {
            return State::Denied;
        }
        if fresh {
            state = State::ObservedAvailable;
        }
    }
    state
}
impl Subject<'_> {
    fn matches(&self, scope: &ObservationScope) -> bool {
        scope.credential_profile == *self.profile
            && scope.credential_revision == self.revision
            && scope.provider == self.provider
            && scope.account_reference == self.account
            && scope.endpoint_reference == self.endpoint
            && scope.model.as_ref().is_none_or(|model| model == self.model)
            && scope.node_id.is_none_or(|node| node == self.node)
    }
}
fn retain_latest<'a>(
    latest: &mut BTreeMap<Digest, &'a AvailabilityObservation>,
    observation: &'a AvailabilityObservation,
) -> Result<(), AppError> {
    let key =
        aq_selection::digest(&(&observation.scope, &observation.source)).map_err(|_| invalid())?;
    if let Some(old) = latest.get(&key)
        && observation.sequence <= old.sequence
    {
        // An older sequence never wins; a repeated one must be the same report.
        let digest = |value| aq_selection::digest(value).map_err(|_| invalid());
        if observation.sequence == old.sequence && digest(observation)? != digest(*old)? {
            return Err(invalid());
        }
        return Ok(());
    }
    latest.insert(key, observation);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use aq_selection::{ObservationScope, ObservationSource, Text};
    fn observation() -> AvailabilityObservation {
        AvailabilityObservation {
            observation_id: Uuid::new_v4(),
            scope: ObservationScope {
                credential_revision: 1,
                provider: "provider".into(),
                credential_profile: ArtifactRef {
                    id: Text::new("account-profile").unwrap(),
                    version: Text::new("1").unwrap(),
                    digest: Digest::of(b"definition"),
                },
                account_reference: "account".into(),
                endpoint_reference: "endpoint".into(),
                model: Some("model".into()),
                node_id: Some(Uuid::from_u128(1)),
            },
            source: ObservationSource::Operator {
                principal_id: Uuid::from_u128(2),
            },
            sequence: 1,
            revision: 1,
            state: Availability::Available,
            received_at_ms: 100,
            expires_at_ms: 1000,
            cooldown_until_ms: 100,
        }
    }
    fn subject(scope: &ObservationScope) -> Subject<'_> {
        Subject {
            profile: &scope.credential_profile,
            revision: 1,
            provider: "provider",
            account: "account",
            endpoint: "endpoint",
            model: "model",
            node: Uuid::from_u128(1),
        }
    }
    fn run(obs: &[AvailabilityObservation], now: u64) -> Evidence {
        let scope = observation().scope;
        evaluate(subject(&scope), obs, 100, now).unwrap()
    }
    #[test]
    fn unknown_never_becomes_available_or_missing_credentials() {
        assert_eq!(run(&[], 150).state, State::Unknown);
        assert_eq!(run(&[observation()], 201).state, State::Unknown);
        assert_eq!(run(&[observation()], 1000).state, State::Unknown);
        assert_eq!(run(&[observation()], 150).state, State::ObservedAvailable);
    }
    #[test]
    fn fresh_exact_negative_and_active_cooldown_deny_but_unrelated_scope_does_not() {
        let mut negative = observation();
        negative.state = Availability::QuotaUnavailable;
        assert_eq!(run(&[negative.clone()], 150).state, State::Denied);
        for field in [
            "model", "account", "endpoint", "provider", "revision", "node", "profile",
        ] {
            let mut other = negative.clone();
            match field {
                "model" => other.scope.model = Some("other".into()),
                "account" => other.scope.account_reference = "other".into(),
                "endpoint" => other.scope.endpoint_reference = "other".into(),
                "provider" => other.scope.provider = "other".into(),
                "revision" => other.scope.credential_revision = 2,
                "node" => other.scope.node_id = Some(Uuid::from_u128(3)),
                _ => other.scope.credential_profile.digest = Digest::of(b"other"),
            };
            assert_eq!(run(&[other], 150).state, State::Unknown, "{field}");
        }
        negative.cooldown_until_ms = 900;
        assert_eq!(run(&[negative.clone()], 800).state, State::Denied);
        assert_eq!(run(&[negative], 901).state, State::Unknown);
    }
    #[test]
    fn latest_evidence_digest_is_independent_of_input_order_and_duplicates() {
        let old = observation();
        let mut newer = old.clone();
        newer.sequence += 1;
        newer.revision += 1;
        newer.received_at_ms = 90;
        let mut independent = observation();
        independent.scope.model = None;
        independent.scope.node_id = None;
        independent.state = Availability::QuotaUnavailable;
        let expected = run(&[newer.clone(), independent.clone()], 150);
        let reordered = run(&[independent, newer.clone(), old, newer], 150);
        assert_eq!(reordered.matching_sources, 2);
        assert_eq!(reordered.state, State::Denied);
        assert_eq!(reordered.observations_digest, expected.observations_digest);
        assert_eq!(reordered.evaluated_at_ms, expected.evaluated_at_ms);
    }
    #[test]
    fn source_sequence_wins_clock_correction_and_conflicting_same_sequence_refuses() {
        let old = observation();
        let mut newer = old.clone();
        newer.observation_id = Uuid::new_v4();
        newer.sequence = 2;
        newer.revision = 2;
        newer.received_at_ms = 90;
        newer.state = Availability::AuthenticationFailed;
        assert_eq!(run(&[old.clone(), newer], 150).state, State::Denied);
        let mut conflict = old.clone();
        conflict.state = Availability::ModelUnsupported;
        let scope = old.scope.clone();
        assert!(evaluate(subject(&scope), &[old, conflict], 100, 150).is_err());
    }
}
