crate::preparation_shared::phase_facts! {
    FactKind: "PreparationRefreshV1FactKind",
    AvailabilityState: "PreparationRefreshV1AvailabilityState",
    InventoryState: "PreparationRefreshV1InventoryState",
    GlobalCredentialSemantics: "PreparationRefreshV1GlobalCredentialSemantics",
    RetentionState: "PreparationRefreshV1RetentionState",
    FactValue: "PreparationRefreshV1FactValue",
    FactBody: "PreparationRefreshV1FactBody",
    OwnerFact: "PreparationRefreshV1OwnerFact",
    registry_fields: [selected_registry: crate::ArtifactRef, accepted_registry: crate::ArtifactRef, preflight_registry: crate::ArtifactRef, current_registry: crate::ArtifactRef, credential_profile: crate::ArtifactRef, credential_revision: u64],
}
