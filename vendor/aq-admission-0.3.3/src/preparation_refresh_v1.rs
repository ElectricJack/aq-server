//! Explicit observation-continuity profile; data validation grants no authority.
mod facts;
crate::preparation_shared::phase_profile! {
    PhaseFeature: "PreparationRefreshV1PhaseFeature",
    Negotiation: "PreparationRefreshV1Negotiation",
    PhasePolicy: "PreparationRefreshV1PhasePolicy",
    ChoicePins: "PreparationRefreshV1ChoicePins",
    OwnerAuthority: "PreparationRefreshV1OwnerAuthority",
    Proposal: "PreparationRefreshV1Proposal",
    ResourceHold: "PreparationRefreshV1ResourceHold",
    AcceptRequest: "PreparationRefreshV1AcceptRequest",
    AcceptedPreparationLease: "PreparationRefreshV1AcceptedPreparationLease",
    AcceptedReceipt: "PreparationRefreshV1AcceptedReceipt",
    AcceptedDelivery: "PreparationRefreshV1AcceptedDelivery",
    PrepareRequest: "PreparationRefreshV1PrepareRequest",
    FreshConfirmation: "PreparationRefreshV1FreshConfirmation",
    ExecutionReceipt: "PreparationRefreshV1ExecutionReceipt",
    ExecutionDelivery: "PreparationRefreshV1ExecutionDelivery",
    Document: "PreparationRefreshV1Document",
    schema_prefix: "PreparationRefreshV1",
    schema_urn: "urn:aq:admission:preparation-registry-refresh:1",
    publication: "aq-admission/0.1.10",
    contract_id: "aq.admission.preparation-registry-refresh-v1-contract",
    extra_features: [RegistryObservationContinuityV1],
    lease_fields: [registry_at_accept: crate::ArtifactRef],
}

fn profile_domain(domain: &str) -> &str {
    match domain {
        "aq.phase.accept.v2" => "aq.phase-refresh.accept.v1",
        "aq.phase.accepted-receipt.v2" => "aq.phase-refresh.accepted-receipt.v1",
        "aq.phase.choice.v2" => "aq.phase.choice.v2",
        "aq.phase.confirmation.v2" => "aq.phase-refresh.confirmation.v1",
        "aq.phase.contract.v2" => "aq.phase-refresh.contract.v1",
        "aq.phase.execution-receipt.v2" => "aq.phase-refresh.execution-receipt.v1",
        "aq.phase.owner-fact.v2" => "aq.phase-refresh.owner-fact.v1",
        "aq.phase.policy.v2" => "aq.phase-refresh.policy.v1",
        "aq.phase.prepare.v2" => "aq.phase-refresh.prepare.v1",
        "aq.phase.proposal.v2" => "aq.phase-refresh.proposal.v1",
        _ => unreachable!("private phase digest domain"),
    }
}

/// Canonical aggregate revision for this explicitly qualified registry owner.
/// Shape only; the caller still must obtain actual authenticated owner records.
pub fn registry_revision(value: &ArtifactRef) -> Result<u64> {
    reference(value)?;
    require(
        value.id.as_str() == "aq.intelligence-registry",
        "refresh registry owner identity",
    )?;
    let n = value
        .version
        .as_str()
        .parse::<u64>()
        .map_err(|_| Invalid("refresh registry revision"))?;
    revision(n)?;
    require(
        value.version.as_str() == n.to_string(),
        "refresh canonical registry revision",
    )?;
    Ok(n)
}
/// Actual owner reads must supply each ref; this helper cannot authenticate them.
pub fn validate_registry_chain(
    selected: &ArtifactRef,
    accepted: &ArtifactRef,
    preflight: &ArtifactRef,
    current: &ArtifactRef,
) -> Result<()> {
    let mut previous = registry_revision(selected)?;
    for value in [accepted, preflight, current] {
        let next = registry_revision(value)?;
        require(
            value.id == selected.id && value.digest == selected.digest && next >= previous,
            "refresh registry configuration/rollback",
        )?;
        previous = next;
    }
    Ok(())
}
fn validate_registry_lease_shape(lease: &AcceptedPreparationLease) -> Result<()> {
    registry_revision(&lease.registry_at_accept).map(|_| ())
}
fn validate_registry_lease(lease: &AcceptedPreparationLease, selected: &ArtifactRef) -> Result<()> {
    validate_registry_chain(
        selected,
        &lease.registry_at_accept,
        &lease.registry_at_accept,
        &lease.registry_at_accept,
    )
}
fn validate_registry_record_shape(body: &FactBody) -> Result<()> {
    if let FactValue::Registry {
        selected_registry,
        accepted_registry,
        preflight_registry,
        current_registry,
        credential_profile,
        credential_revision,
    } = &body.value
    {
        validate_registry_chain(
            selected_registry,
            accepted_registry,
            preflight_registry,
            current_registry,
        )?;
        reference(credential_profile)?;
        revision(*credential_revision)?;
        require(
            body.revision == registry_revision(current_registry)?,
            "refresh actual registry fact revision",
        )?;
    }
    Ok(())
}
fn validate_registry_value(fact: &OwnerFact, p: &Proposal, a: &AcceptedReceipt) -> Result<()> {
    validate_registry_record_shape(&fact.body)?;
    let FactValue::Registry {
        selected_registry,
        accepted_registry,
        credential_profile,
        credential_revision,
        ..
    } = &fact.body.value
    else {
        unreachable!()
    };
    require(
        *selected_registry == p.choice.registry
            && *accepted_registry == a.lease.registry_at_accept
            && *credential_profile == p.choice.credential_profile
            && *credential_revision == p.choice.credential_revision,
        "refresh original registry/accepted floor/credential",
    )
}

fn validate_registry_choice(choice: &ChoicePins) -> Result<()> {
    registry_revision(&choice.registry).map(|_| ())
}
