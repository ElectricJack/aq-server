//! Actual private file observations with a synthetic sealed selection. No lease
//! persistence or grant authority follows from this component helper.
use crate::common;
use aq_server::{
    intelligence::config::{CredentialConfig, IntelligenceConfig, MaterialDeliveryMode},
    profiled_execution::material::{self, MaterialError},
};
use common::profiled::{NOW, fixture};
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;
#[tokio::test]
async fn selected_account_node_and_current_private_bytes_are_required() {
    let f = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential");
    std::fs::write(&path, b"synthetic private material").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let choice = &f.selection.choice;
    let registry = IntelligenceConfig {
        accounting: Default::default(),
        material_store: None,
        revision: 1,
        credentials: vec![CredentialConfig {
            kind: Default::default(),
            allowed_backends: aq_selection::default_allowed_backends(),
            credential: aq_selection::AuthorizedCredential {
                profile: choice.credential_profile.clone(),
                provider: choice.provider.clone(),
                account_reference: choice.account_reference.clone(),
                endpoint_reference: choice.endpoint_reference.clone(),
                revision: f.selection.credential_revision,
                enabled: true,
                provisionable: false,
                material_owner: None,
                allowed_backends: aq_selection::default_allowed_backends(),
            },
            allowed_node_ids: vec![f.node.node_id],
            allow_enrolled_nodes: false,
            secret_file: Some(path.clone()),
            delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
        }],
        qualified_harnesses: vec![],
    }
    .load()
    .unwrap();
    let lease = material::issue_for_selection(
        &registry,
        &f.selection,
        &f.profiled.offer.binding,
        Uuid::new_v4(),
        NOW,
        NOW + 1000,
        None,
    )
    .unwrap();
    assert_eq!(lease.material_bytes, 26);
    assert_eq!(
        lease.material_digest,
        aq_admission::Digest::of(b"synthetic private material")
    );
    let mut other = f.selection.clone();
    other.choice.account_reference = "different-account".into();
    assert!(matches!(
        material::issue_for_selection(
            &registry,
            &other,
            &f.profiled.offer.binding,
            Uuid::new_v4(),
            NOW,
            NOW + 1000,
            None
        ),
        Err(MaterialError::Scope)
    ));
    let mut other = f.selection.clone();
    other.target = aq_selection::ExecutionTarget::Worker {
        node_id: Uuid::new_v4(),
        boot_id: f.node.boot_id,
    };
    assert!(matches!(
        material::issue_for_selection(
            &registry,
            &other,
            &f.profiled.offer.binding,
            Uuid::new_v4(),
            NOW,
            NOW + 1000,
            None
        ),
        Err(MaterialError::Scope)
    ));
    std::fs::write(&path, b"rotated bytes").unwrap();
    assert!(matches!(
        material::issue_for_selection(
            &registry,
            &f.selection,
            &f.profiled.offer.binding,
            Uuid::new_v4(),
            NOW,
            NOW + 1000,
            None
        ),
        Err(MaterialError::Changed)
    ));
}

#[tokio::test]
async fn restricted_backend_cannot_issue_material_lease() {
    use aq_selection::{BackendKind, CredentialKind};
    let f = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential");
    std::fs::write(&path, b"synthetic").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    for kind in [CredentialKind::ApiKey, CredentialKind::SubscriptionLogin] {
        let choice = &f.selection.choice;
        let registry = IntelligenceConfig {
            accounting: Default::default(),
            material_store: None,
            revision: 1,
            credentials: vec![CredentialConfig {
                kind,
                allowed_backends: vec![BackendKind::DockerLocal],
                credential: aq_selection::AuthorizedCredential {
                    profile: choice.credential_profile.clone(),
                    provider: choice.provider.clone(),
                    account_reference: choice.account_reference.clone(),
                    endpoint_reference: choice.endpoint_reference.clone(),
                    revision: f.selection.credential_revision,
                    enabled: true,
                    provisionable: false,
                    material_owner: None,
                    allowed_backends: aq_selection::default_allowed_backends(),
                },
                allowed_node_ids: vec![f.node.node_id],
                allow_enrolled_nodes: false,
                secret_file: Some(path.clone()),
                delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
            }],
            qualified_harnesses: vec![],
        }
        .load()
        .unwrap();
        for backend in [
            None,
            Some(BackendKind::Boat),
            Some(BackendKind::CloudflareContainers),
        ] {
            assert!(matches!(
                material::issue_for_selection(
                    &registry,
                    &f.selection,
                    &f.profiled.offer.binding,
                    Uuid::new_v4(),
                    NOW,
                    NOW + 1000,
                    backend
                ),
                Err(MaterialError::Scope)
            ));
        }
        assert!(
            material::issue_for_selection(
                &registry,
                &f.selection,
                &f.profiled.offer.binding,
                Uuid::new_v4(),
                NOW,
                NOW + 1000,
                Some(BackendKind::DockerLocal)
            )
            .is_ok()
        );
    }
}
