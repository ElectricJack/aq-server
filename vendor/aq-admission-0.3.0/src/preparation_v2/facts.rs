crate::preparation_shared::phase_facts! {
    FactKind: "PreparationV2FactKind",
    AvailabilityState: "PreparationV2AvailabilityState",
    InventoryState: "PreparationV2InventoryState",
    GlobalCredentialSemantics: "PreparationV2GlobalCredentialSemantics",
    RetentionState: "PreparationV2RetentionState",
    FactValue: "PreparationV2FactValue",
    FactBody: "PreparationV2FactBody",
    OwnerFact: "PreparationV2OwnerFact",
    registry_fields: [registry: crate::ArtifactRef, credential_profile: crate::ArtifactRef, credential_revision: u64],
}
