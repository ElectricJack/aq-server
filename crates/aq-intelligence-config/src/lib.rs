//! Startup-owned configuration. Secret bytes never enter selector snapshots.
use aq_selection::{AuthorizedCredential, InstalledHarness, Validate};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialDeliveryMode {
    ServerMaterialV1,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    #[serde(default)]
    pub kind: aq_selection::CredentialKind,
    #[serde(default = "aq_selection::default_allowed_backends")]
    pub allowed_backends: Vec<aq_selection::BackendKind>,
    /// Material state and the nested backend policy are derived by `load`.
    pub credential: AuthorizedCredential,
    pub allowed_node_ids: Vec<uuid::Uuid>,
    /// Spec 0007 Q3: on by default; `false` restricts the credential to `allowed_node_ids`.
    #[serde(default = "enrolled_by_default")]
    pub allow_enrolled_nodes: bool,
    pub secret_file: Option<PathBuf>,
    #[serde(default)]
    pub delivery_mode: Option<MaterialDeliveryMode>,
}
fn enrolled_by_default() -> bool {
    true
}
/// The credential-wide observation scope (no model, no node) of `c`: the scope
/// of the owner's availability switch (spec 0007 Q2).
pub fn credential_scope(c: &AuthorizedCredential) -> aq_selection::ObservationScope {
    aq_selection::ObservationScope {
        credential_revision: c.revision,
        provider: c.provider.clone(),
        credential_profile: c.profile.clone(),
        account_reference: c.account_reference.clone(),
        endpoint_reference: c.endpoint_reference.clone(),
        model: None,
        node_id: None,
    }
}
/// Lifetime of a synthesized owner attestation (the validator's maximum).
pub const OWNER_ATTESTATION_TTL_MS: u64 = 3_600_000;
/// Spec 0007 Q2: while the owner's switch is on (`switch` = its principal and
/// generation), selection treats every authorized credential as attested
/// available at `now_ms`, with no observation row. A credential with unexpired
/// negative feedback in `observed` (the relevant rows) is not attested, so that
/// feedback still holds it off. The result depends only on its inputs: the
/// admission fences recompute it from the decision's own `now_ms`, so it
/// changes only when the owner flips the switch. Its sequence ranks above any
/// row of the same scope and principal, so a stale or expired row of the old
/// keeper never hides it.
pub fn owner_attestations(
    credentials: &[AuthorizedCredential],
    observed: &[aq_selection::AvailabilityObservation],
    (principal, generation): (uuid::Uuid, u64),
    revision: u64,
    now_ms: u64,
) -> Vec<aq_selection::AvailabilityObservation> {
    use sha2::Digest;
    let negative = |c: &AuthorizedCredential| {
        observed.iter().any(|o| {
            o.state != aq_selection::Availability::Available
                && o.expires_at_ms > now_ms
                && o.scope.credential_profile == c.profile
                && o.scope.credential_revision == c.revision
                && o.scope.provider == c.provider
                && o.scope.account_reference == c.account_reference
                && o.scope.endpoint_reference == c.endpoint_reference
        })
    };
    (credentials.iter().enumerate())
        .filter(|(_, c)| !negative(c))
        .map(|(i, c)| {
            let name = format!("aq.swarm.owner-availability:{generation}:{i}");
            let hash: [u8; 32] = sha2::Sha256::digest(name.as_bytes()).into();
            let id = uuid::Builder::from_custom_bytes(hash[..16].try_into().expect("16 bytes"));
            aq_selection::AvailabilityObservation {
                observation_id: id.into_uuid(),
                scope: credential_scope(c),
                source: aq_selection::ObservationSource::Operator {
                    principal_id: principal,
                },
                sequence: (1 << 62) + generation,
                revision,
                state: aq_selection::Availability::Available,
                received_at_ms: now_ms,
                expires_at_ms: now_ms + OWNER_ATTESTATION_TTL_MS,
                cooldown_until_ms: 0,
            }
        })
        .collect()
}
impl CredentialConfig {
    /// Spec 0007 Q3: every enrolled node, or only the listed ones when the flag is off.
    /// Revoked nodes are refused by authentication and the offer fence, not here.
    pub fn allows(&self, node: uuid::Uuid) -> bool {
        self.allow_enrolled_nodes || self.allowed_node_ids.contains(&node)
    }
}
impl std::fmt::Debug for CredentialConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialConfig")
            .field("credential", &self.credential)
            .field("kind", &self.kind)
            .field("allowed_backends", &self.allowed_backends)
            .field("allowed_node_ids", &self.allowed_node_ids)
            .field("allow_enrolled_nodes", &self.allow_enrolled_nodes)
            .field("delivery_mode", &self.delivery_mode)
            .field(
                "secret_file",
                &self.secret_file.as_ref().map(|_| "<protected>"),
            )
            .finish()
    }
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntelligenceConfig {
    pub revision: u64,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
    #[serde(default)]
    pub qualified_harnesses: Vec<InstalledHarness>,
    /// Trusted accounting catalog (H2). Owner data only: workers never supply
    /// source policy, namespace or parser priority. Excluded from the
    /// selection registry digest, which covers credentials and harnesses.
    #[serde(default)]
    pub accounting: AccountingCatalog,
    /// C5b runtime-writable store for stored subscription-login versions: an
    /// absolute, owner-only (0700) directory. Without it, write-back and
    /// operator import are refused. Excluded
    /// from the registry digest (a local path, not selection policy).
    #[serde(default)]
    pub material_store: Option<PathBuf>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountingCatalog {
    #[serde(default)]
    pub sources: Vec<SourcePolicy>,
    #[serde(default)]
    pub rate_cards: Vec<aq_accounting::pricing::RateCardRevision>,
}
/// Observation source policy for one (provider, harness) pair.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePolicy {
    pub provider: String,
    pub harness: String,
    pub reference: aq_accounting::wire::ArtifactRef,
    pub layout: aq_accounting::Layout,
    pub parsers: Vec<aq_accounting::reconcile::ParserAuthority>,
}
impl AccountingCatalog {
    pub fn validate(&self) -> Result<(), String> {
        if self.sources.len() > 32 {
            return Err("too many accounting sources".into());
        }
        let mut keys = BTreeSet::new();
        for s in &self.sources {
            let policy = aq_accounting::reconcile::Policy {
                reference: s.reference.clone(),
                account_scope: aq_accounting::wire::Digest::of(b"validate"),
                call_namespace: aq_accounting::wire::Digest::of(b"validate"),
                provider: aq_accounting::wire::Text::new(&s.provider)
                    .map_err(|_| "invalid accounting source provider")?,
                layout: s.layout.clone(),
                parsers: s.parsers.clone(),
            };
            if policy.validate().is_err()
                || s.harness.is_empty()
                || s.harness.len() > 512
                || !keys.insert((s.provider.clone(), s.harness.clone()))
            {
                return Err("invalid or duplicate accounting source policy".into());
            }
        }
        self.book().map(|_| ())
    }
    pub fn book(&self) -> Result<aq_accounting::pricing::RateCardBook, String> {
        aq_accounting::pricing::RateCardBook::new(self.rate_cards.clone())
            .map_err(|_| "invalid rate-card revisions".into())
    }
    pub fn source(&self, provider: &str, harness: &str) -> Option<&SourcePolicy> {
        self.sources
            .iter()
            .find(|s| s.provider == provider && s.harness == harness)
    }
}
impl IntelligenceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.revision == 0
            || self.revision > i64::MAX as u64
            || self.credentials.len() > 32
            || self.qualified_harnesses.len() > 16
        {
            return Err("invalid intelligence registry bounds/revision".into());
        }
        self.accounting.validate()?;
        let mut keys = BTreeSet::new();
        for item in &self.credentials {
            let c = &item.credential;
            c.profile
                .id
                .validate()
                .map_err(|_| "invalid credential profile artifact")?;
            c.profile
                .version
                .validate()
                .map_err(|_| "invalid credential profile artifact")?;
            if c.revision == 0
                || c.revision > i64::MAX as u64
                || !aq_selection::valid_allowed_backends(&item.allowed_backends)
                || !aq_selection::unrestricted_backends(&c.allowed_backends)
                || c.provisionable
                || c.material_owner.is_some()
                || (item.delivery_mode.is_some() && item.secret_file.is_none())
                || item.allowed_node_ids.len() > 256
                || item.allowed_node_ids.iter().any(uuid::Uuid::is_nil)
                || item.allowed_node_ids.iter().collect::<BTreeSet<_>>().len()
                    != item.allowed_node_ids.len()
                || !keys.insert((c.profile.id.clone(), c.profile.version.clone()))
            {
                return Err(
                    "invalid credential identity/placement; provisionable is derived".into(),
                );
            }
            for text in [&c.provider, &c.account_reference, &c.endpoint_reference] {
                if text.is_empty() || text.len() > 512 || text.chars().any(char::is_control) {
                    return Err("invalid logical credential reference".into());
                }
            }
            if let Some(path) = &item.secret_file
                && !path.is_absolute()
            {
                return Err("credential file must be absolute".into());
            }
        }
        if let Some(path) = &self.material_store
            && !path.is_absolute()
        {
            return Err("material_store must be absolute".into());
        }
        Ok(())
    }
    /// Call once at daemon startup, never while selecting. Missing files are a
    /// missing-credentials fact; unsafe objects and oversized input fail closed.
    pub fn load(&self) -> Result<LoadedRegistry, String> {
        self.validate()?;
        let mut credentials = Vec::new();
        let mut fingerprints = Vec::new();
        for item in &self.credentials {
            let mut credential = item.credential.clone();
            // Canonical order keeps policy-identical spellings digest-identical.
            credential.allowed_backends =
                aq_selection::canonical_allowed_backends(&item.allowed_backends);
            if credential.allowed_backends.is_empty() {
                // Explicit `allowed_backends = []` is a deliberate deny-all;
                // omitting the key is unrestricted. Surface the difference.
                tracing::warn!(
                    profile = %credential.profile.id.as_str(),
                    "credential allowed_backends is empty; it is never eligible"
                );
            }
            let fingerprint = match &item.secret_file {
                None => None,
                Some(path) => match OpenOptions::new()
                    .read(true)
                    .custom_flags(
                        nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC,
                    )
                    .open(path)
                {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => return Err("cannot open protected credential file".into()),
                    Ok(file) => {
                        let metadata = file
                            .metadata()
                            .map_err(|_| "credential metadata unavailable")?;
                        if !metadata.is_file()
                            || metadata.mode() & 0o077 != 0
                            || metadata.uid() != nix::unistd::geteuid().as_raw()
                        {
                            return Err("credential must be an owner-only regular file".into());
                        }
                        let mut bytes = zeroize::Zeroizing::new(Vec::new());
                        file.take(65_537)
                            .read_to_end(&mut bytes)
                            .map_err(|_| "cannot read protected credential")?;
                        if bytes.is_empty() || bytes.len() > 65_536 {
                            return Err("credential byte bound".into());
                        }
                        use sha2::Digest;
                        Some(hex::encode(sha2::Sha256::digest(&bytes)))
                    }
                },
            };
            credential.provisionable = credential.enabled && fingerprint.is_some();
            if item.delivery_mode.is_some() && credential.provisionable {
                // Deliberately excludes local path, secret values and private
                // fingerprints. The explicit profile revision owns rotation.
                let descriptor = serde_json::json!({
                    "schema_version":1,"adapter":"aq.server-material","adapter_version":1,
                    "profile":credential.profile,"credential_revision":credential.revision,
                    "provider":credential.provider,"account_reference":credential.account_reference,
                    "endpoint_reference":credential.endpoint_reference,
                });
                let owner_digest =
                    aq_selection::digest(&descriptor).map_err(|_| "material owner descriptor")?;
                credential.material_owner = Some(aq_selection::ArtifactRef {
                    id: aq_selection::Text::new(format!(
                        "aq.configured-provider-material/{}",
                        owner_digest.as_str()
                    ))
                    .map_err(|_| "material owner identity")?,
                    version: aq_selection::Text::new("1").map_err(|_| "material owner version")?,
                    digest: owner_digest,
                });
            }

            credentials.push(credential);
            fingerprints.push(fingerprint);
        }
        Ok(LoadedRegistry {
            config: self.clone(),
            credentials,
            fingerprints,
        })
    }
}
/// Private fingerprints are rotation checks only; never inspection output.
#[derive(Clone)]
pub struct LoadedRegistry {
    pub config: IntelligenceConfig,
    pub credentials: Vec<AuthorizedCredential>,
    pub fingerprints: Vec<Option<String>>,
}
impl std::fmt::Debug for LoadedRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedRegistry")
            .field("revision", &self.config.revision)
            .finish_non_exhaustive()
    }
}

/// Deserialize once at startup; an Arc keeps the validated immutable snapshot
/// shared by request clones without repeated file reads.
pub fn deserialize_registry<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<std::sync::Arc<LoadedRegistry>>, D::Error> {
    Option::<IntelligenceConfig>::deserialize(deserializer)?
        .map(|c| {
            c.load()
                .map(std::sync::Arc::new)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(extra: serde_json::Value) -> CredentialConfig {
        let mut v = serde_json::json!({
            "credential": {
                "profile": {"id":"test", "version":"1", "digest":"a".repeat(64)},
                "provider":"test", "account_reference":"acct", "endpoint_reference":"endpoint",
                "revision":3, "enabled":true, "provisionable":false
            },
            "allowed_node_ids": [uuid::Uuid::from_u128(1)],
            "secret_file": null
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn allows_listed_or_enrolled_nodes() {
        let (listed, other) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
        let default = credential(serde_json::json!({}));
        assert!(default.allow_enrolled_nodes && default.allows(other));
        let restricted = credential(serde_json::json!({"allow_enrolled_nodes": false}));
        assert!(restricted.allows(listed) && !restricted.allows(other));
    }

    #[test]
    fn owner_attestations_follow_the_switch_generation_and_spare_negative_feedback() {
        let c = credential(serde_json::json!({})).credential;
        let who = uuid::Uuid::from_u128(9);
        let on = |g, observed: &[aq_selection::AvailabilityObservation], now| {
            owner_attestations(std::slice::from_ref(&c), observed, (who, g), 4, now)
        };
        let a = on(2, &[], 1_000);
        assert_eq!(a.len(), 1);
        a[0].validate().unwrap();
        assert_eq!(a[0].state, aq_selection::Availability::Available);
        assert_eq!((a[0].received_at_ms, a[0].revision), (1_000, 4));
        // Recomputed later from the decision's own clock: identical bytes.
        let again = on(2, &[], 1_000);
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&again).unwrap()
        );
        // Flipping the switch (a new generation) changes the attestation.
        assert_ne!(on(4, &[], 1_000)[0].observation_id, a[0].observation_id);
        let mut negative = a[0].clone();
        negative.state = aq_selection::Availability::QuotaUnavailable;
        negative.scope.model = Some("m".into());
        negative.expires_at_ms = 2_000;
        assert!(on(2, std::slice::from_ref(&negative), 1_000).is_empty());
        assert_eq!(on(2, std::slice::from_ref(&negative), 2_000).len(), 1);
    }
}
