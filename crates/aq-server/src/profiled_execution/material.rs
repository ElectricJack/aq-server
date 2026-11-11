//! Private configured-file owner. No caller path, URL, command or secret value
//! enters a lease request. Read outside SQL locks; the caller must subsequently
//! fence current assignment, registry revision, lease and expiry before delivery.
use super::login_refresh::HeadSource;
use crate::intelligence::config::LoadedRegistry;
use aq_admission::{MaterialLease, SessionIdentity};
use uuid::Uuid;

use aq_intelligence_config::material::read_configured;
pub use aq_intelligence_config::material::{MaterialError, ProtectedMaterial};

pub fn read_for_lease(
    registry: &LoadedRegistry,
    lease: &MaterialLease,
    node_id: Uuid,
) -> Result<ProtectedMaterial, MaterialError> {
    read_for_lease_from(registry, lease, node_id, None)
}
/// As `read_for_lease`, serving a subscription-login lineage revision when the
/// lease was issued from one. The configured file must still match its
/// startup fingerprint either way, so an operator rotation is never masked.
pub fn read_for_lease_from(
    registry: &LoadedRegistry,
    lease: &MaterialLease,
    node_id: Uuid,
    source: Option<&HeadSource>,
) -> Result<ProtectedMaterial, MaterialError> {
    lease.validate().map_err(|_| MaterialError::Scope)?;
    if !matches!(lease.binding.session,SessionIdentity::Remote {node_id:assigned,..} if assigned==node_id)
    {
        return Err(MaterialError::Scope);
    }
    let (index, credential) = registry
        .credentials
        .iter()
        .enumerate()
        .find(|(_, credential)| {
            credential.profile == lease.credential_profile
                && credential.revision == lease.credential_revision
        })
        .ok_or(MaterialError::Scope)?;
    let configured = registry
        .config
        .credentials
        .get(index)
        .ok_or(MaterialError::Unavailable)?;
    if !credential.enabled
        || !credential.provisionable
        || credential.material_owner.is_none()
        || !configured.allows(node_id)
        || credential.provider != lease.provider.as_str()
        || credential.account_reference != lease.account_reference.as_str()
        || credential.endpoint_reference != lease.endpoint_reference.as_str()
    {
        return Err(MaterialError::Scope);
    }
    let bytes = source_or_configured(registry, index, source)?;
    if aq_admission::Digest::of(bytes.bytes()) != lease.material_digest
        || bytes.bytes().len() as u64 != lease.material_bytes
    {
        return Err(MaterialError::Changed);
    }
    Ok(bytes)
}

pub(crate) fn source_or_configured(
    registry: &LoadedRegistry,
    index: usize,
    source: Option<&HeadSource>,
) -> Result<ProtectedMaterial, MaterialError> {
    let configured = read_configured(registry, index)?;
    match source {
        None => Ok(configured),
        Some(s) => {
            drop(configured);
            super::login_refresh::store::read(&s.dir, s.revision_id, &s.digest, s.bytes)
        }
    }
}

pub(crate) use aq_intelligence_config::material::provisioning_bindings;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::config::{CredentialConfig, IntelligenceConfig, MaterialDeliveryMode};
    use std::os::unix::fs::PermissionsExt;
    fn lease() -> MaterialLease {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../aq-admission/artifacts/fixtures.json"
        ))
        .unwrap();
        serde_json::from_value(
            fixtures
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["kind"] == "material_lease")
                .unwrap()["body"]
                .clone(),
        )
        .unwrap()
    }
    fn config(path: std::path::PathBuf, lease: &MaterialLease, node: Uuid) -> IntelligenceConfig {
        IntelligenceConfig {
            accounting: Default::default(),
            material_store: None,
            revision: 1,
            credentials: vec![CredentialConfig {
                kind: Default::default(),
                allowed_backends: aq_selection::default_allowed_backends(),
                credential: aq_selection::AuthorizedCredential {
                    profile: lease.credential_profile.clone(),
                    provider: lease.provider.as_str().into(),
                    account_reference: lease.account_reference.as_str().into(),
                    endpoint_reference: lease.endpoint_reference.as_str().into(),
                    revision: lease.credential_revision,
                    enabled: true,
                    provisionable: false,
                    material_owner: None,
                    allowed_backends: aq_selection::default_allowed_backends(),
                },
                allowed_node_ids: vec![node],
                allow_enrolled_nodes: false,
                secret_file: Some(path),
                delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
            }],
            qualified_harnesses: vec![],
        }
    }
    #[test]
    fn credential_backend_policy_gates_provisioning_before_private_reads() {
        use aq_selection::{BackendKind, CredentialKind, DeliveryCapability};
        let lease = lease();
        let SessionIdentity::Remote {
            node_id, boot_id, ..
        } = lease.binding.session
        else {
            unreachable!()
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-key");
        std::fs::write(&path, b"synthetic-key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let delivery = DeliveryCapability {
            adapter: lease.credential_profile.clone(),
            version: 1,
        };
        for kind in [CredentialKind::ApiKey, CredentialKind::SubscriptionLogin] {
            let mut cfg = config(path.clone(), &lease, node_id);
            cfg.credentials[0].kind = kind;
            cfg.credentials[0].allowed_backends = vec![BackendKind::DockerLocal];
            let registry = cfg.load().unwrap();
            for backend in [
                None,
                Some(BackendKind::Boat),
                Some(BackendKind::CloudflareContainers),
            ] {
                assert!(
                    provisioning_bindings(
                        &registry, node_id, boot_id, &delivery, 1000, 2000, backend
                    )
                    .is_empty()
                );
            }
            assert_eq!(
                provisioning_bindings(
                    &registry,
                    node_id,
                    boot_id,
                    &delivery,
                    1000,
                    2000,
                    Some(BackendKind::DockerLocal)
                )
                .len(),
                1
            );
            cfg.credentials[0].allowed_backends = aq_selection::default_allowed_backends();
            assert_eq!(
                provisioning_bindings(
                    &cfg.load().unwrap(),
                    node_id,
                    boot_id,
                    &delivery,
                    1000,
                    2000,
                    None
                )
                .len(),
                1
            );
            cfg.credentials[0].allowed_node_ids.clear();
            assert!(
                provisioning_bindings(
                    &cfg.load().unwrap(),
                    node_id,
                    boot_id,
                    &delivery,
                    1000,
                    2000,
                    Some(BackendKind::DockerLocal)
                )
                .is_empty()
            );
        }
    }
    #[test]
    fn delivery_requires_exact_scope_and_current_bounded_file_bytes() {
        let lease = lease();
        let SessionIdentity::Remote { node_id, .. } = lease.binding.session else {
            unreachable!()
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-key");
        std::fs::write(&path, b"synthetic-key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let registry = config(path.clone(), &lease, node_id).load().unwrap();
        assert_eq!(
            read_for_lease(&registry, &lease, node_id).unwrap().bytes(),
            b"synthetic-key"
        );
        assert!(matches!(
            read_for_lease(&registry, &lease, Uuid::new_v4()),
            Err(MaterialError::Scope)
        ));
        let mut wrong = lease.clone();
        wrong.account_reference = aq_admission::Label::new("other-account").unwrap();
        assert!(matches!(
            read_for_lease(&registry, &wrong, node_id),
            Err(MaterialError::Scope)
        ));
        std::fs::write(&path, b"different-key").unwrap();
        assert!(matches!(
            read_for_lease(&registry, &lease, node_id),
            Err(MaterialError::Changed)
        ));
        std::fs::write(&path, vec![b'x'; 65_537]).unwrap();
        assert!(matches!(
            read_for_lease(&registry, &lease, node_id),
            Err(MaterialError::Unavailable)
        ));
        std::fs::write(&path, b"synthetic-key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_for_lease(&registry, &lease, node_id),
            Err(MaterialError::Unavailable)
        ));
        std::fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(matches!(
            read_for_lease(&registry, &lease, node_id),
            Err(MaterialError::Unavailable)
        ));
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(matches!(
            read_for_lease(&registry, &lease, node_id),
            Err(MaterialError::Unavailable)
        ));
        let mut installed = config(path, &lease, node_id);
        installed.credentials[0].delivery_mode = None;
        installed.credentials[0].secret_file = None;
        assert!(matches!(
            read_for_lease(&installed.load().unwrap(), &lease, node_id),
            Err(MaterialError::Scope)
        ));
    }
}

mod issuance;
pub use issuance::{issue_for_selection, issue_for_selection_from};
