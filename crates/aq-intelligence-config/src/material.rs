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
    let config = registry
        .config
        .credentials
        .get(index)
        .ok_or(MaterialError::Unavailable)?;
    let path = config
        .secret_file
        .as_ref()
        .ok_or(MaterialError::Unavailable)?;
    let bytes = read_protected_file(path)
        .map_err(|_| MaterialError::Unavailable)?
        .ok_or(MaterialError::Unavailable)?;
    if fingerprint(&bytes) != *expected {
        return Err(MaterialError::Changed);
    }
    // A Claude subscription login (version 0 here) is delivered exactly like
    // an imported one: trailing CR/LF trimmed, any other shape refused. Other
    // credentials (API keys included) stay byte-exact. The fingerprint
    // above stays over the configured file's bytes (rotation check).
    if is_claude_setup_token(config) {
        let bytes = normalize_claude_token(bytes).map_err(|_| MaterialError::Unavailable)?;
        return Ok(ProtectedMaterial(bytes));
    }
    Ok(ProtectedMaterial(bytes))
}
pub(super) fn fingerprint(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

pub(super) fn read_protected_file(
    path: &std::path::Path,
) -> Result<Option<Zeroizing<Vec<u8>>>, &'static str> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot open protected credential file"),
    };
    let metadata = file
        .metadata()
        .map_err(|_| "credential metadata unavailable")?;
    let private = metadata.mode() & 0o077 == 0 && metadata.uid() == nix::unistd::geteuid().as_raw();
    if !metadata.is_file() || !private {
        return Err("credential must be an owner-only regular file");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read protected credential")?;
    if bytes.is_empty() || bytes.len() > 65_536 {
        return Err("credential byte bound");
    }
    Ok(Some(bytes))
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

pub struct ProvisioningSnapshot<'a> {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub delivery: &'a aq_selection::DeliveryCapability,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub backend_kind: Option<aq_selection::BackendKind>,
}

/// Trusted daemon facts only. Call outside SQL locks, then fence the exact
/// admitted runtime, registry and observation age in the write transaction.
/// File presence yields preparation readiness, never installed availability.
pub fn provisioning_bindings(
    registry: &LoadedRegistry,
    snapshot: ProvisioningSnapshot<'_>,
) -> Vec<aq_selection::CredentialBinding> {
    let ProvisioningSnapshot {
        node_id: node,
        boot_id: boot,
        delivery,
        observed_at_ms: observed,
        expires_at_ms: expires,
        backend_kind,
    } = snapshot;
    if !valid_delivery_target(node, boot, delivery) || !valid_delivery_window(observed, expires) {
        return Vec::new();
    }
    registry
        .credentials
        .iter()
        .enumerate()
        .filter_map(|(index, c)| {
            let owner = registry.delivery_owner(index, node, backend_kind)?;
            Some(aq_selection::CredentialBinding {
                credential_revision: c.revision,
                credential_profile: c.profile.clone(),
                available: false,
                preparation: Some(aq_selection::ProvisioningReadiness {
                    owner,
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

fn valid_delivery_target(
    node: Uuid,
    boot: Uuid,
    delivery: &aq_selection::DeliveryCapability,
) -> bool {
    !node.is_nil() && !boot.is_nil() && delivery.version == 1
}
fn valid_delivery_window(observed: u64, expires: u64) -> bool {
    observed > 0
        && expires
            .checked_sub(observed)
            .is_some_and(|age| (1..=120_000).contains(&age))
}
impl crate::CredentialConfig {
    fn permits_delivery(&self, node: Uuid, backend: Option<aq_selection::BackendKind>) -> bool {
        self.delivery_mode.is_some()
            && self.allows(node)
            && aq_selection::credential_backend_policy(&self.allowed_backends, backend).is_ok()
    }
}

impl LoadedRegistry {
    fn delivery_owner(
        &self,
        index: usize,
        node: Uuid,
        backend_kind: Option<aq_selection::BackendKind>,
    ) -> Option<aq_selection::ArtifactRef> {
        let c = self.credentials.get(index)?;
        let cfg = self.config.credentials.get(index)?;
        let owner = c.material_owner.as_ref()?;
        if !c.enabled || !c.provisionable {
            return None;
        }
        if !cfg.permits_delivery(node, backend_kind) {
            return None;
        }
        read_configured(self, index).ok()?;
        Some(owner.clone())
    }
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
