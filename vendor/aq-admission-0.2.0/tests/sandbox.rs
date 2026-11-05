use aq_admission::sandbox;
#[test]
fn sandbox_schema_drift() {
    let published: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.schema.json")).unwrap();
    assert_eq!(sandbox::schema(), published);
}

use aq_admission::{
    ArtifactRef, Digest, Text,
    transport::{Register, WorkerIdentity},
};
use sandbox::*;
use uuid::Uuid;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn registration() -> Registration {
    Registration {
        schema_version: 2,
        registration: Register {
            schema_version: 1,
            operation_id: id(1),
            identity: WorkerIdentity {
                node_id: id(2),
                boot_id: id(3),
            },
            runtime: ArtifactRef {
                id: Text::new("runtime").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"runtime"),
            },
            capabilities: vec![],
            features: vec![],
        },
        backend: BackendAdvertisement {
            kind: BackendKind::DockerLocal,
            capabilities: vec![BackendCapability::NestedDocker],
            max_concurrent_sandboxes: 4,
            spend_ceiling: None,
            templates: vec![TemplateAdvertisement {
                digest: Digest::of(b"template"),
                harnesses: vec![HarnessVersion {
                    harness: "codex".into(),
                    version: "1.2.3".into(),
                }],
                estimated_cold_start_ms: 1200,
                estimated_cold_start_cost: Money {
                    currency: "USD".into(),
                    micros: 0,
                },
            }],
        },
    }
}
#[test]
fn sandbox_registration_rejects_duplicates_bounds_and_false_capabilities() {
    let mut reg = registration();
    reg.validate().unwrap();
    let doc = SandboxDocument::Register(reg.clone());
    let bytes = serde_json::to_vec(&doc).unwrap();
    decode_document(&bytes).unwrap();
    // Frozen readers explicitly reject this version instead of silently dropping requirements.
    assert!(aq_admission::transport::decode_transport(&bytes).is_err());
    reg.backend.max_concurrent_sandboxes = 0;
    assert!(reg.validate().is_err());
    reg = registration();
    reg.backend
        .capabilities
        .push(BackendCapability::VmIsolation);
    assert!(reg.validate().is_err());
    reg = registration();
    reg.backend.templates.push(reg.backend.templates[0].clone());
    assert!(reg.validate().is_err());
    reg = registration();
    let duplicate = reg.backend.templates[0].harnesses[0].clone();
    reg.backend.templates[0].harnesses.push(duplicate);
    assert!(reg.validate().is_err());
    reg = registration();
    reg.backend.spend_ceiling = Some(Money {
        currency: "usd".into(),
        micros: 0,
    });
    assert!(reg.validate().is_err());
}
#[test]
fn sandbox_decoding_rejects_unknown_duplicate_fields_and_oversized_frames() {
    let value = serde_json::to_value(SandboxDocument::Register(registration())).unwrap();
    let mut unknown = value.clone();
    unknown["body"]["unexpected"] = true.into();
    assert!(decode_document(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let raw = serde_json::to_string(&value).unwrap();
    let duplicate = raw.replacen(
        "\"schema_version\":2",
        "\"schema_version\":2,\"schema_version\":2",
        1,
    );
    assert_ne!(duplicate, raw);
    assert!(decode_document(duplicate.as_bytes()).is_err());
    assert!(decode_document(&vec![b' '; MAX_SANDBOX_FRAME_BYTES + 1]).is_err());
}
#[test]
fn sandbox_awaiting_readoption_has_no_deadline_extension() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.fixtures.json")).unwrap();
    let FixtureCase::Permit { permit, .. } = &fixtures[0].case else {
        panic!()
    };
    let reply = RenewalReply::AwaitingNodeReadoption {
        operation_id: id(90),
        binding: permit.binding.clone(),
    };
    reply.validate().unwrap();
    let mut raw = serde_json::to_value(&reply).unwrap();
    assert!(raw.get("expires_at_ms").is_none());
    raw["expires_at_ms"] = 9000.into();
    assert!(serde_json::from_value::<RenewalReply>(raw).is_err());
}
#[test]
fn sandbox_published_fixtures_conform() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.fixtures.json")).unwrap();
    assert!(fixtures.len() >= 20);
    for fixture in fixtures {
        assert_eq!(
            validate_fixture(&fixture),
            fixture.expected,
            "{}",
            fixture.name
        );
    }
}

#[test]
fn sandbox_context_is_validated_and_secret_debug_is_redacted() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.fixtures.json")).unwrap();
    let FixtureCase::Permit {
        permit,
        context,
        operation,
        now_ms,
    } = &fixtures[0].case
    else {
        panic!()
    };
    assert!(!format!("{permit:?}").contains("fixture_only"));
    let mut forged = permit.clone();
    let mut ctx = context.clone();
    forged.binding.attempt_id = Uuid::nil();
    ctx.binding.attempt_id = Uuid::nil();
    assert_eq!(
        forged.validate_for(&ctx, *operation, *now_ms),
        Err(ContractError::InvalidShape)
    );
    let mut ctx = context.clone();
    ctx.grant_expires_at_ms = 9000;
    assert_eq!(
        permit.validate_for(&ctx, *operation, *now_ms),
        Err(ContractError::DeadlineMismatch)
    );
    for op in [
        RunnerOperation::Started,
        RunnerOperation::Renew,
        RunnerOperation::Observe,
        RunnerOperation::Terminal,
        RunnerOperation::FetchExecutionMaterial,
        RunnerOperation::UploadArtifact,
        RunnerOperation::ReportResult,
    ] {
        permit.validate_for(context, op, *now_ms).unwrap();
    }
    let mut prep = permit.clone();
    prep.scope = PermitScope::Preparation;
    let mut preparing = context.clone();
    preparing.phase = GrantPhase::Preparing;
    for op in [
        RunnerOperation::Started,
        RunnerOperation::Renew,
        RunnerOperation::Terminal,
        RunnerOperation::FetchExecutionMaterial,
        RunnerOperation::ReportResult,
    ] {
        assert_eq!(
            prep.validate_for(&preparing, op, *now_ms),
            Err(ContractError::ScopeDenied)
        );
    }
}

mod preparation_refresh_common;
#[test]
fn sandbox_phase_receipt_projections_keep_native_identity_and_deadlines() {
    let frozen = preparation_refresh_common::frozen_case();
    frozen
        .execution
        .validate_for(
            &frozen.prepare,
            &frozen.accepted,
            &frozen.accept,
            &frozen.proposal,
        )
        .unwrap();
    let refresh = preparation_refresh_common::case();
    refresh
        .execution
        .validate_for(
            &refresh.prepare,
            &refresh.accepted,
            &refresh.accept,
            &refresh.proposal,
        )
        .unwrap();
    let a = PhaseExecutionAuthority::from_phase_v2(&frozen.execution).unwrap();
    let b = PhaseExecutionAuthority::from_phase_refresh(&refresh.execution).unwrap();
    assert_eq!(a.receipt_digest, frozen.execution.digest().unwrap());
    assert_eq!(b.receipt_digest, refresh.execution.digest().unwrap());
    assert_eq!(a.profile, PhaseProfile::PhaseV2);
    assert_eq!(b.profile, PhaseProfile::PhaseRefreshV1);
    for authority in [a, b] {
        let fixtures: Vec<Fixture> =
            serde_json::from_str(include_str!("../artifacts/sandbox-v1.fixtures.json")).unwrap();
        let FixtureCase::Permit { mut permit, .. } = fixtures[0].case.clone() else {
            panic!()
        };
        let aq_admission::SessionIdentity::Remote {
            node_id, boot_id, ..
        } = authority.grant.binding.session
        else {
            panic!()
        };
        permit.binding.node = WorkerIdentity { node_id, boot_id };
        permit.binding.attempt_id = authority.grant.binding.attempt_id;
        permit.binding.grant_generation = authority.grant.binding.assignment_generation;
        permit.issued_at_ms = authority.issued_at_ms;
        permit.expires_at_ms = authority.grant.expires_at_ms;
        permit.hard_deadline_ms = authority.hard_deadline_ms;
        authority
            .validate_handoff(&permit, authority.issued_at_ms)
            .unwrap();
        permit.binding.node.boot_id = id(999999);
        assert_eq!(
            authority.validate_handoff(&permit, authority.issued_at_ms),
            Err(ContractError::BindingMismatch)
        );
    }
}
#[test]
fn sandbox_renewal_delivery_cannot_retime_replay_or_revive_local_expiry() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.fixtures.json")).unwrap();
    let FixtureCase::RunnerRenewal { response, .. } = &fixtures
        .iter()
        .find(|f| f.name == "phase_v2_renewal")
        .unwrap()
        .case
    else {
        panic!()
    };
    let sent = 1_000_000_000;
    let received = 1_100_000_000;
    let prior = 2_000_000_000;
    let hard = 5_000_000_000;
    assert_eq!(
        response
            .client_deadline_ns(sent, received, prior, hard, 1)
            .unwrap(),
        3_000_000_000
    );
    assert_eq!(
        response
            .client_deadline_ns(sent, received, prior, hard, 2)
            .unwrap(),
        prior
    );
    assert_eq!(
        response.client_deadline_ns(sent, prior, prior, hard, 1),
        Err(ContractError::Expired)
    );
    assert_eq!(
        response.client_deadline_ns(sent, received, prior, hard, 3),
        Err(ContractError::ReplayConflict)
    );
    let FixtureCase::RunnerRenewal { response, .. } = &fixtures
        .iter()
        .find(|f| f.name == "renewal_awaiting_readoption")
        .unwrap()
        .case
    else {
        panic!()
    };
    assert_eq!(
        response
            .client_deadline_ns(sent, received, prior, hard, 1)
            .unwrap(),
        prior
    );
}
