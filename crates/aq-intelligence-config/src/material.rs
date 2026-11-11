//! Private registry material, bounded and checked against the startup fingerprint.
use crate::LoadedRegistry;
use anyhow::bail;
use sha2::Digest;
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};
use uuid::Uuid;
use zeroize::Zeroizing;
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MaterialError {
    #[error("configured material owner unavailable")]
    Unavailable,
    #[error("material scope is not authorized")]
    Scope,
    #[error("configured material changed; registry reload required")]
    Changed,
}
/// Intentionally no Debug, Serialize or Clone. Only the authenticated private
/// response producer may borrow bytes; zeroization covers this owner buffer,
/// not copies owned by the HTTP stack or the remote receiving process.
pub struct ProtectedMaterial(Zeroizing<Vec<u8>>);
impl ProtectedMaterial {
    pub fn bytes(&self) -> &[u8] {
        self.0.as_slice()
    }
    pub fn owned(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self(bytes)
    }
}

pub fn read_configured(
    registry: &LoadedRegistry,
    index: usize,
) -> Result<ProtectedMaterial, MaterialError> {
    let expected = registry
        .fingerprints
        .get(index)
        .and_then(Option::as_ref)
        .ok_or(MaterialError::Unavailable)?;
    let path = registry
        .config
        .credentials
        .get(index)
        .and_then(|c| c.secret_file.as_ref())
        .ok_or(MaterialError::Unavailable)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| MaterialError::Unavailable)?;
    let metadata = file.metadata().map_err(|_| MaterialError::Unavailable)?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
    {
        return Err(MaterialError::Unavailable);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| MaterialError::Unavailable)?;
    if bytes.is_empty() || bytes.len() > 65_536 {
        return Err(MaterialError::Unavailable);
    }
    if hex::encode(sha2::Sha256::digest(&bytes)) != *expected {
        return Err(MaterialError::Changed);
    }
    // A Claude subscription login (version 0 here) is delivered exactly like
    // an imported one: trailing CR/LF trimmed, any other shape refused. Other
    // credentials (API keys included) stay byte-exact. The fingerprint
    // above stays over the configured file's bytes (rotation check).
    if registry
        .config
        .credentials
        .get(index)
        .is_some_and(is_claude_setup_token)
    {
        let bytes = normalize_claude_token(bytes).map_err(|_| MaterialError::Unavailable)?;
        return Ok(ProtectedMaterial(bytes));
    }
    Ok(ProtectedMaterial(bytes))
}
pub fn is_claude_setup_token(c: &crate::CredentialConfig) -> bool {
    c.kind == aq_selection::CredentialKind::SubscriptionLogin
        && c.credential.provider.as_str() == "anthropic"
}

/// A Claude login is the long-lived `claude setup-token` value, delivered
/// env-only. `claude setup-token > file` leaves a trailing newline that the
/// runner's install (ASCII-graphic only) would refuse later, so trailing CR/LF
/// is trimmed here and any other shape is refused before storage (C5b/MS1).
pub fn normalize_claude_token(bytes: Zeroizing<Vec<u8>>) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    if end == 0 || !bytes[..end].iter().all(u8::is_ascii_graphic) {
        bail!("a Claude login must be one line of printable ASCII (the setup-token value)");
    }
    Ok(Zeroizing::new(bytes[..end].to_vec()))
}

/// Trusted daemon facts only. Call outside SQL locks, then fence the exact
/// admitted runtime, registry and observation age in the write transaction.
/// File presence yields preparation readiness, never installed availability.
pub fn provisioning_bindings(
    registry: &LoadedRegistry,
    node: Uuid,
    boot: Uuid,
    delivery: &aq_selection::DeliveryCapability,
    observed: u64,
    expires: u64,
    backend_kind: Option<aq_selection::BackendKind>,
) -> Vec<aq_selection::CredentialBinding> {
    if node.is_nil()
        || boot.is_nil()
        || observed == 0
        || expires <= observed
        || expires - observed > 120_000
        || delivery.version != 1
    {
        return vec![];
    }
    registry
        .credentials
        .iter()
        .enumerate()
        .filter_map(|(index, c)| {
            let cfg = registry.config.credentials.get(index)?;
            let owner = c.material_owner.as_ref()?;
            if !c.enabled
                || !c.provisionable
                || cfg.delivery_mode.is_none()
                || !cfg.allows(node)
                || aq_selection::credential_backend_policy(&cfg.allowed_backends, backend_kind)
                    .is_err()
                || read_configured(registry, index).is_err()
            {
                return None;
            }
            Some(aq_selection::CredentialBinding {
                credential_revision: c.revision,
                credential_profile: c.profile.clone(),
                available: false,
                preparation: Some(aq_selection::ProvisioningReadiness {
                    owner: owner.clone(),
                    credential_profile: c.profile.clone(),
                    credential_revision: c.revision,
                    target: aq_selection::ExecutionTarget::Worker {
                        node_id: node,
                        boot_id: boot,
                    },
                    delivery: delivery.clone(),
                    observed_at_ms: observed,
                    expires_at_ms: expires,
                }),
                observed_at_ms: observed,
                expires_at_ms: expires,
            })
        })
        .collect()
}

impl LoadedRegistry {
    /// Deterministic eligible server login for owner-requested docker-local delivery.
    /// Callers must separately authenticate actual enrollment and fence revocation.
    pub fn subscription_for(
        &self,
        node: Uuid,
        provider: &str,
    ) -> Option<(usize, &aq_selection::AuthorizedCredential)> {
        self.credentials.iter().enumerate().find(|(i, c)| {
            let cfg = &self.config.credentials[*i];
            c.enabled
                && c.provisionable
                && c.provider == provider
                && c.material_owner.is_some()
                && cfg.kind == aq_selection::CredentialKind::SubscriptionLogin
                && cfg.allows(node)
                && aq_selection::credential_backend_policy(
                    &cfg.allowed_backends,
                    Some(aq_selection::BackendKind::DockerLocal),
                )
                .is_ok()
        })
    }
}
