//! Credential metadata and backend restrictions shared by selection and material owners.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(JsonSchema, Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    #[default]
    ApiKey,
    SubscriptionLogin,
}

#[derive(
    JsonSchema, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    DockerLocal,
    Boat,
    CloudflareContainers,
}

impl BackendKind {
    /// Every supported backend in canonical (`Ord`) order. A unit test checks
    /// this against the derived schema's variant list, so a new variant cannot
    /// silently turn every unrestricted credential into a restricted one.
    pub const ALL: [BackendKind; 3] = [
        BackendKind::DockerLocal,
        BackendKind::Boat,
        BackendKind::CloudflareContainers,
    ];
}

/// Unrestricted policy: eligibility of the backend itself is owned by placement.
pub fn default_allowed_backends() -> Vec<BackendKind> {
    BackendKind::ALL.to_vec()
}

/// Bounded and distinct. An empty list is valid and denies every backend;
/// an absent configuration key means unrestricted.
pub fn valid_allowed_backends(backends: &[BackendKind]) -> bool {
    backends.len() <= BackendKind::ALL.len()
        && backends.iter().collect::<BTreeSet<_>>().len() == backends.len()
}

/// Order-insensitive: any distinct list naming every supported backend.
pub fn unrestricted_backends(backends: &[BackendKind]) -> bool {
    backends.len() == BackendKind::ALL.len() && valid_allowed_backends(backends)
}

/// Canonical `Ord` order, so equal policies have equal digests regardless of
/// configuration spelling. Callers validate distinctness separately.
pub fn canonical_allowed_backends(backends: &[BackendKind]) -> Vec<BackendKind> {
    let mut backends = backends.to_vec();
    backends.sort_unstable();
    backends.dedup();
    backends
}

/// Missing backend identity is compatible only with unrestricted credentials.
/// Callers supply trusted node facts, never a caller-provided placement hint.
pub fn credential_backend_policy(
    allowed: &[BackendKind],
    backend: Option<BackendKind>,
) -> Result<(), &'static str> {
    if !valid_allowed_backends(allowed) {
        return Err("credential_backend_policy_invalid");
    }
    match backend {
        Some(backend) if allowed.contains(&backend) => Ok(()),
        Some(_) => Err("credential_backend_not_allowed"),
        None if unrestricted_backends(allowed) => Ok(()),
        None => Err("credential_backend_unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_all_is_exhaustive_and_canonical() {
        // The derive enumerates every variant; `ALL` must match it exactly.
        let schema = serde_json::to_value(schemars::schema_for!(BackendKind)).unwrap();
        assert_eq!(
            schema["enum"],
            serde_json::to_value(BackendKind::ALL).unwrap(),
            "{schema}"
        );
        assert!(BackendKind::ALL.is_sorted());
        assert!(unrestricted_backends(&BackendKind::ALL));
        let mut reversed = BackendKind::ALL.to_vec();
        reversed.reverse();
        assert!(unrestricted_backends(&reversed));
        assert_eq!(canonical_allowed_backends(&reversed), BackendKind::ALL);
        assert!(valid_allowed_backends(&[]));
        assert_eq!(
            credential_backend_policy(&[], Some(BackendKind::Boat)),
            Err("credential_backend_not_allowed")
        );
        assert_eq!(
            credential_backend_policy(&[], None),
            Err("credential_backend_unknown")
        );
    }
}
