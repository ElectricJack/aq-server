//! Frozen 0.1.9 behavior over shared private algorithms.
mod facts;
crate::preparation_shared::phase_profile! {
    PhaseFeature: "PreparationV2PhaseFeature",
    Negotiation: "PreparationV2Negotiation",
    PhasePolicy: "PreparationV2PhasePolicy",
    ChoicePins: "PreparationV2ChoicePins",
    OwnerAuthority: "PreparationV2OwnerAuthority",
    Proposal: "PreparationV2Proposal",
    ResourceHold: "PreparationV2ResourceHold",
    AcceptRequest: "PreparationV2AcceptRequest",
    AcceptedPreparationLease: "PreparationV2AcceptedPreparationLease",
    AcceptedReceipt: "PreparationV2AcceptedReceipt",
    AcceptedDelivery: "PreparationV2AcceptedDelivery",
    PrepareRequest: "PreparationV2PrepareRequest",
    FreshConfirmation: "PreparationV2FreshConfirmation",
    ExecutionReceipt: "PreparationV2ExecutionReceipt",
    ExecutionDelivery: "PreparationV2ExecutionDelivery",
    Document: "PreparationV2Document",
    schema_prefix: "PreparationV2",
    schema_urn: "urn:aq:admission:preparation-v2:1",
    publication: "aq-admission/0.1.9",
    contract_id: "aq.admission.preparation-v2-contract",
    extra_features: [],
    lease_fields: [],
}

fn profile_domain(domain: &str) -> &str {
    domain
}
fn validate_registry_lease(_: &AcceptedPreparationLease, _: &ArtifactRef) -> Result<()> {
    Ok(())
}
fn validate_registry_lease_shape(_: &AcceptedPreparationLease) -> Result<()> {
    Ok(())
}
fn validate_registry_record_shape(_: &FactBody) -> Result<()> {
    Ok(())
}
fn validate_registry_value(fact: &OwnerFact, p: &Proposal, _: &AcceptedReceipt) -> Result<()> {
    let FactValue::Registry {
        registry,
        credential_profile,
        credential_revision,
    } = &fact.body.value
    else {
        unreachable!()
    };
    require(
        *registry == p.choice.registry
            && *credential_profile == p.choice.credential_profile
            && *credential_revision == p.choice.credential_revision,
        "phase current registry/credential",
    )
}

fn validate_registry_choice(_: &ChoicePins) -> Result<()> {
    Ok(())
}
