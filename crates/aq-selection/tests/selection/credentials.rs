use super::*;

fn prepared_credentials(input: &mut SelectionInputs) {
    let delivery = DeliveryCapability {
        adapter: artifact("qualified-private-delivery-adapter"),
        version: 1,
    };
    input.node.material_delivery = Some(delivery.clone());
    for (credential, binding) in input
        .credentials
        .iter_mut()
        .zip(&mut input.node.credential_bindings)
    {
        let owner = artifact(&format!(
            "daemon-file-owner:{}",
            credential.profile.id.as_str()
        ));
        credential.material_owner = Some(owner.clone());
        binding.available = false;
        binding.preparation = Some(ProvisioningReadiness {
            owner,
            credential_profile: credential.profile.clone(),
            credential_revision: credential.revision,
            target: input.project.target.clone(),
            delivery: delivery.clone(),
            observed_at_ms: input.now_ms - 1,
            expires_at_ms: input.now_ms + 1000,
        });
    }
}
#[test]
fn cold_node_selects_only_explicit_owned_preparation_and_preserves_material_provenance() {
    let (catalog, mut input) = fixture();
    prepared_credentials(&mut input);
    let outcome = select(&catalog, &input).unwrap();
    let choice = outcome.resolved.unwrap();
    assert!(matches!(
        choice.choice.credential_delivery,
        Some(CredentialDelivery::ServerLease { version: 1, .. })
    ));
    assert!(
        choice.traces[0]
            .reason_codes
            .contains(&"material_preparation_required".into())
    );
    assert!(
        !input.node.credential_bindings[0].available,
        "readiness never claims installed bytes"
    );
    assert!(
        choice.expires_at_ms
            <= input.node.credential_bindings[0]
                .preparation
                .as_ref()
                .unwrap()
                .expires_at_ms
    );
}
#[test]
fn arbitrary_node_delivery_claims_cannot_manufacture_keys_owner_identity_or_current_boot() {
    let (catalog, mut input) = fixture();
    prepared_credentials(&mut input);
    for credential in &mut input.credentials {
        credential.provisionable = false;
    }
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    for credential in &mut input.credentials {
        credential.provisionable = true;
        credential.material_owner = None;
    }
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    prepared_credentials(&mut input);
    for binding in &mut input.node.credential_bindings {
        binding.preparation.as_mut().unwrap().target = ExecutionTarget::Worker {
            node_id: Uuid::from_u128(9301),
            boot_id: Uuid::from_u128(9302),
        };
    }
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    prepared_credentials(&mut input);
    input.node.material_delivery.as_mut().unwrap().adapter = artifact("unqualified-node-choice");
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    prepared_credentials(&mut input);
    for binding in &mut input.node.credential_bindings {
        binding.preparation.as_mut().unwrap().expires_at_ms = input.now_ms;
    }
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
}

#[test]
fn credential_restricted_backend_is_ineligible() {
    let (catalog, mut input) = fixture();
    input.node.backend_kind = Some(BackendKind::Boat);
    for credential in &mut input.credentials {
        credential.allowed_backends = vec![BackendKind::DockerLocal];
    }
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces
            .iter()
            .all(|t| t.state == CandidateState::PolicyDenied)
    );
    assert!(
        out.traces[0]
            .reason_codes
            .contains(&"credential_backend_not_allowed".into())
    );

    input.node.backend_kind = None;
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces[0]
            .reason_codes
            .contains(&"credential_backend_unknown".into())
    );

    input.node.backend_kind = Some(BackendKind::DockerLocal);
    assert!(select(&catalog, &input).unwrap().resolved.is_some());

    for credential in &mut input.credentials {
        credential.allowed_backends = default_allowed_backends();
    }
    input.node.backend_kind = None;
    assert!(select(&catalog, &input).unwrap().resolved.is_some());
    for backend in default_allowed_backends() {
        input.node.backend_kind = Some(backend);
        assert!(select(&catalog, &input).unwrap().resolved.is_some());
    }
    for credential in &mut input.credentials {
        credential.allowed_backends.clear();
    }
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
}

#[test]
fn credential_backend_denial_keeps_that_account_ineligible_without_blocking_another() {
    let (catalog, mut input) = fixture();
    input.credentials[0].allowed_backends = vec![BackendKind::DockerLocal];
    input.node.backend_kind = Some(BackendKind::Boat);
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.traces[0].state, CandidateState::PolicyDenied);
    assert_eq!(out.resolved.unwrap().choice.model, "model-b");
    let mut policy = policy();
    policy
        .fallback_on
        .retain(|r| *r != FallbackReason::PolicyDenied);
    let catalog = change_policy(&catalog, policy, &mut input);
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.traces[0].state, CandidateState::PolicyDenied);
    assert_eq!(out.resolved.unwrap().choice.provider, "fixture-b");
}

#[test]
fn credential_backend_snapshot_validation_and_legacy_defaults() {
    let (catalog, mut input) = fixture();
    let encoded = serde_json::to_value(&input).unwrap();
    assert!(encoded["node"].get("backend_kind").is_none());
    assert!(encoded["credentials"][0].get("allowed_backends").is_none());
    let decoded: SelectionInputs = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.node.backend_kind, None);
    assert_eq!(
        decoded.credentials[0].allowed_backends,
        default_allowed_backends()
    );
    for (field, value) in [
        ("backend_kind", serde_json::json!("unknown")),
        ("backend_kind", serde_json::json!("x".repeat(513))),
    ] {
        let mut unknown = encoded.clone();
        unknown["node"][field] = value;
        assert!(serde_json::from_value::<SelectionInputs>(unknown).is_err());
    }
    for backends in [vec![BackendKind::Boat; 2], vec![BackendKind::Boat; 4]] {
        input.credentials[0].allowed_backends = backends;
        assert!(select(&catalog, &input).is_err());
    }
}
