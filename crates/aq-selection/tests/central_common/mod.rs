#![allow(dead_code)]
use aq_profiles::{central::*, *};
use aq_selection::{central::*, *};
use aq_session_protocol::Digest;
use uuid::Uuid;
fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
pub fn fixture() -> (ValidatedCatalog, CentralSelectionInputs) {
    let original =
        ValidatedCatalog::from_artifact(include_bytes!("../fixtures/central-catalog-v1.json"))
            .unwrap();
    let mut definitions = original.artifact().catalog.clone();
    let mut profile_policy = super::common::policy();
    profile_policy.max_attempts = 1;
    profile_policy.budget.unknown_cost = UnknownCostPolicy::AllowBounded;
    definitions.intelligence[0].version = 2;
    definitions.intelligence[0].selection_policy = Some(profile_policy);
    definitions.intelligence[0].candidates = vec![
        ModelCandidate {
            id: "primary".into(),
            provider: "fixture-a".into(),
            model: "model-a".into(),
            reasoning_configuration_reference: "reasoning-a@1".into(),
            harnesses: vec!["codex".into()],
        },
        ModelCandidate {
            id: "fallback".into(),
            provider: "fixture-b".into(),
            model: "model-b".into(),
            reasoning_configuration_reference: "reasoning-b@1".into(),
            harnesses: vec!["codex".into()],
        },
    ];
    let agent = definitions
        .agents
        .iter_mut()
        .find(|a| a.id == "central-supervisor")
        .unwrap();
    agent.intelligence = "engineering@2".into();
    agent.version = 2;
    let execution = definitions
        .execution
        .iter_mut()
        .find(|a| a.id == "central-execution")
        .unwrap();
    execution.version = 2;
    execution.credential_profile_references = vec!["account-a@1".into(), "account-b@1".into()];
    let agent = definitions
        .agents
        .iter_mut()
        .find(|a| a.id == "central-supervisor")
        .unwrap();
    agent.execution = "central-execution@2".into();
    definitions
        .intelligence
        .insert(0, original.artifact().catalog.intelligence[0].clone());
    let catalog =
        aq_profiles::validate(definitions, original.artifact().instructions.clone()).unwrap();
    let data: serde_json::Value =
        serde_json::from_slice(include_bytes!("../fixtures/central-request-v1.json")).unwrap();
    let mut request: CentralCompileRequest = serde_json::from_value(data["data"].clone()).unwrap();
    request.policy.agent_profile = "central-supervisor@2".into();
    request.policy.catalog_digest = catalog.digest().into();
    request.compatibility.catalog_digest = catalog.digest().into();
    request.compatibility.agent_profile = catalog.source("agent", "central-supervisor@2").unwrap();
    request.compatibility.intelligence_profile =
        catalog.source("intelligence", "engineering@2").unwrap();
    request.compatibility.execution_profile =
        catalog.source("execution", "central-execution@2").unwrap();
    request.policy.compatibility_digest = aq_selection::digest(&request.compatibility).unwrap();
    request.subject = CentralProfileSubject::CentralConversation {
        actor_id: request.policy.actor_id,
        conversation_id: request.policy.conversation_id,
        conversation_revision: request.policy.conversation_revision,
        policy: request.policy.artifact().unwrap().reference,
    };
    let bundle = compile_central(&catalog, &request).unwrap().bundle.unwrap();
    let reservation_policy = Pinned::<ReservationPolicy>::new(ReservationPolicy {
        schema_version: 1,
        kind: ReservationPolicyKind::GlobalSupervisorReservation,
        actor_id: request.policy.actor_id,
        conversation_id: request.policy.conversation_id,
        enabled: true,
    })
    .unwrap();
    let reservation = Pinned::<ReservationSnapshot>::new(ReservationSnapshot {
        reservation_id: Uuid::from_u128(500),
        actor_id: request.policy.actor_id,
        conversation_id: request.policy.conversation_id,
        conversation_revision: request.policy.conversation_revision,
        actor_revision: 2,
        reservation_revision: 1,
        policy_revision: 1,
        policy: reservation_policy.clone(),
        through_cursor: request.turn.through_cursor,
        messages: request
            .turn
            .messages
            .iter()
            .map(|m| ReservedMail {
                message_id: m.message_id,
                cursor: m.cursor,
                payload_digest: m.payload_digest.clone(),
            })
            .collect(),
    })
    .unwrap();
    let (harness, credentials, observations, bindings) = {
        let (_, i) = super::common::fixture();
        (
            i.node.harnesses[0].clone(),
            i.credentials,
            i.observations,
            i.node.credential_bindings,
        )
    };
    let mut harness = harness;
    harness.environment = artifact("central-private");
    harness.limits.context_bytes = 1_048_576;
    let policy = Pinned::<CentralTurnPolicy>::new(CentralTurnPolicy {
        schema_version: 1,
        revision: 1,
        kind: CentralPolicyKind::CentralTurnSelection,
        subject: request.subject.clone(),
        reservation: reservation.reference.clone(),
        reservation_policy: reservation_policy.reference,
        composition_policy: bundle.policy.reference.clone(),
        compatibility: bundle.compatibility.reference.clone(),
        catalog: bundle.composition.catalog.clone(),
        agent_profile: bundle.composition.agent_profile.clone(),
        intelligence_profile: bundle.composition.intelligence_profile.clone(),
        execution_profile: bundle.composition.execution_profile.clone(),
        enabled: true,
        attempts: CentralAttemptPolicy::InitialSelectionOnly,
        allow_unknown_cost: true,
        ceilings: ResourceCeilings {
            context_bytes: 1_048_576,
            wall_time_ms: 60_000,
            memory_bytes: 268435456,
            cpu_millis: 60000,
            process_limit: 8,
            cost_limit_microunits: None,
            currency: None,
            require_hard_cost_enforcement: false,
        },
        max_output_tokens: 512,
        allowed_environments: vec![harness.environment.clone()],
    })
    .unwrap();
    let registry = Pinned::<RegistrySnapshot>::new(RegistrySnapshot {
        revision: 1,
        observed_at_ms: 1000,
        expires_at_ms: 20000,
        credentials,
        observations,
    })
    .unwrap();
    let installation = Pinned::<InstallationSnapshot>::new(InstallationSnapshot {
        installation_id: Uuid::from_u128(600),
        incarnation_id: Uuid::from_u128(601),
        revision: 1,
        observed_at_ms: 1000,
        expires_at_ms: 20000,
        harnesses: vec![harness],
        credential_bindings: bindings
            .into_iter()
            .map(|b| CentralCredentialBinding {
                credential_revision: b.credential_revision,
                credential_profile: b.credential_profile,
                available: b.available,
                observed_at_ms: b.observed_at_ms,
                expires_at_ms: b.expires_at_ms,
            })
            .collect(),
    })
    .unwrap();
    (
        catalog,
        CentralSelectionInputs {
            schema_version: 1,
            reservation,
            policy,
            registry,
            installation,
            compositions: vec![bundle],
            now_ms: 2000,
            original_expires_at_ms: 12000,
        },
    )
}
pub fn repin(i: &mut CentralSelectionInputs) {
    i.reservation.value.policy =
        Pinned::<ReservationPolicy>::new(i.reservation.value.policy.value.clone()).unwrap();
    i.reservation = Pinned::<ReservationSnapshot>::new(i.reservation.value.clone()).unwrap();
    i.policy.value.reservation = i.reservation.reference.clone();
    i.policy.value.reservation_policy = i.reservation.value.policy.reference.clone();
    i.policy = Pinned::<CentralTurnPolicy>::new(i.policy.value.clone()).unwrap();
    i.registry = Pinned::<RegistrySnapshot>::new(i.registry.value.clone()).unwrap();
    i.installation = Pinned::<InstallationSnapshot>::new(i.installation.value.clone()).unwrap();
}
