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
    pub secret_file: Option<PathBuf>,
    #[serde(default)]
    pub delivery_mode: Option<MaterialDeliveryMode>,
}
impl std::fmt::Debug for CredentialConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialConfig")
            .field("credential", &self.credential)
            .field("kind", &self.kind)
            .field("allowed_backends", &self.allowed_backends)
            .field("allowed_node_ids", &self.allowed_node_ids)
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
    pub(crate) fingerprints: Vec<Option<String>>,
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
