use aq_admission::sandbox;
#[test]
fn sandbox_schema_drift() {
    let published: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.schema.json")).unwrap();
    assert_eq!(sandbox::schema(), published);
}

#[test]
fn sandbox_schema_constrains_versions_and_missing_capability_bounds() {
    let schema = sandbox::schema();
    let mut versions = std::collections::BTreeMap::new();
    for (name, definition) in schema["$defs"].as_object().unwrap() {
        if let Some(version) = definition["properties"].get("schema_version") {
            let expected = if matches!(name.as_str(), "Registration" | "RegistrationResponse") {
                2
            } else {
                1
            };
            assert_eq!(version["const"], expected, "{name}");
            versions.insert(name.as_str(), expected);
        }
    }
    assert_eq!(versions.len(), 13);
    let missing = &schema["$defs"]["CompatibilityReply"]["oneOf"][1]["properties"]["missing"];
    assert_eq!(missing["minItems"], 1);
    assert_eq!(missing["maxItems"], 64);
    for count in [0, 65] {
        let reply = sandbox::CompatibilityReply::UnsupportedRequiredCapability {
            missing: (0..count).map(|n| format!("capability-{n}")).collect(),
        };
        assert_eq!(reply.validate(), Err(sandbox::ContractError::InvalidShape));
    }
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

#[test]
fn sandbox_renewal_accepts_only_live_correlated_extensions_from_prior_state() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.fixtures.json")).unwrap();
    for name in [
        "phase_v2_renewal",
        "phase_refresh_renewal",
        "phase_renewal_after_readoption",
    ] {
        let FixtureCase::RunnerRenewal {
            request,
            response,
            context,
            nonce,
            now_ms,
        } = &fixtures.iter().find(|f| f.name == name).unwrap().case
        else {
            panic!()
        };
        assert_eq!(context.grant_expires_at_ms, request.permit.expires_at_ms);
        response
            .validate_for(request, context, *nonce, *now_ms)
            .unwrap();
        // Ordinary operation validation continues to enforce the installed
        // grant expiry; only the correlated renewal can extend it.
        let RenewalReply::Renewed { permit, .. } = &response.reply else {
            panic!()
        };
        assert!(permit.expires_at_ms > context.grant_expires_at_ms);
        assert_eq!(
            permit.validate_for(context, RunnerOperation::Renew, *now_ms),
            Err(ContractError::DeadlineMismatch)
        );
    }
}

struct Chain {
    issue_request: PermitIssuanceRequest,
    issue_response: PermitIssuanceResponse,
    issued_now: u64,
    upgrade_request: PermitUpgradeRequest,
    upgrade_response: PermitUpgradeResponse,
    preparation: PreparationAuthority,
    preparation_permit: DelegatedPermit,
    upgrade_now: u64,
}
fn credential(tag: &str) -> PermitCredential {
    serde_json::from_value(serde_json::json!(format!(
        "fixture_only_not_a_real_secret_{tag}"
    )))
    .unwrap()
}
macro_rules! chain {
    ($c:expr, $prep:expr, $exec:expr, $accept:ident, $prepare:ident) => {{
        let c = $c;
        let preparation = $prep(&c.accepted).unwrap();
        let execution = $exec(&c.execution).unwrap();
        let node = c.accept.original.identity.clone();
        let binding = PermitBinding {
            attempt_id: preparation.binding.attempt_id,
            grant_generation: preparation.binding.assignment_generation,
            sandbox_id: id(0x7002),
            node: node.clone(),
            channel_id: id(0x7001),
        };
        let preparation_permit = DelegatedPermit {
            schema_version: 1,
            permit_id: id(0x7003),
            credential: credential("preparation_0000"),
            binding: binding.clone(),
            scope: PermitScope::Preparation,
            issued_at_ms: preparation.issued_at_ms,
            expires_at_ms: preparation.preparation_deadline_ms,
            hard_deadline_ms: preparation.preparation_deadline_ms,
        };
        let issue_request = PermitIssuanceRequest {
            schema_version: 1,
            operation_id: c.accept.original.operation_id,
            profile: preparation.profile,
            accept_request_digest: c.accept.digest().unwrap(),
            attempt_id: binding.attempt_id,
            grant_generation: binding.grant_generation,
            sandbox_id: binding.sandbox_id,
            node,
        };
        issue_request.$accept(&c.accept).unwrap();
        let upgrade_request = PermitUpgradeRequest {
            schema_version: 1,
            operation_id: c.prepare.original.operation_id,
            profile: preparation.profile,
            prepare_request_digest: c.prepare.digest().unwrap(),
            accepted_receipt_digest: c.prepare.accepted_receipt_digest.clone(),
            permit_id: preparation_permit.permit_id,
            binding: binding.clone(),
        };
        upgrade_request.$prepare(&c.prepare).unwrap();
        let execution_permit = DelegatedPermit {
            credential: credential("execution_00000"),
            scope: PermitScope::Execution,
            issued_at_ms: execution.issued_at_ms,
            expires_at_ms: execution.grant.expires_at_ms,
            hard_deadline_ms: execution.hard_deadline_ms,
            ..preparation_permit.clone()
        };
        Chain {
            issue_response: PermitIssuanceResponse {
                schema_version: 1,
                operation_id: issue_request.operation_id,
                authority: preparation.clone(),
                permit: preparation_permit.clone(),
            },
            issue_request,
            issued_now: preparation.issued_at_ms,
            upgrade_now: execution.issued_at_ms,
            upgrade_response: PermitUpgradeResponse {
                schema_version: 1,
                operation_id: upgrade_request.operation_id,
                authority: execution,
                permit: execution_permit,
            },
            upgrade_request,
            preparation,
            preparation_permit,
        }
    }};
}
fn chains() -> [Chain; 2] {
    [
        chain!(
            preparation_refresh_common::frozen_case(),
            PreparationAuthority::from_phase_v2,
            PhaseExecutionAuthority::from_phase_v2,
            validate_accept_v2,
            validate_prepare_v2
        ),
        chain!(
            preparation_refresh_common::case(),
            PreparationAuthority::from_phase_refresh,
            PhaseExecutionAuthority::from_phase_refresh,
            validate_accept_refresh,
            validate_prepare_refresh
        ),
    ]
}
fn fixture(name: &str, case: FixtureCase, expected: &str) -> Fixture {
    Fixture {
        name: name.into(),
        case,
        expected: expected.into(),
    }
}
fn issuance(
    c: &Chain,
    edit: impl FnOnce(&mut PermitIssuanceRequest, &mut PermitIssuanceResponse, &mut u64),
) -> FixtureCase {
    let (mut request, mut response, mut now) = (
        c.issue_request.clone(),
        c.issue_response.clone(),
        c.issued_now,
    );
    edit(&mut request, &mut response, &mut now);
    FixtureCase::PermitIssuance {
        request,
        response: Box::new(response),
        now_ms: now,
    }
}
fn upgrade(
    c: &Chain,
    edit: impl FnOnce(
        &mut PermitUpgradeRequest,
        &mut PermitUpgradeResponse,
        &mut DelegatedPermit,
        &mut u64,
    ),
) -> FixtureCase {
    let (mut request, mut response, mut permit, mut now) = (
        c.upgrade_request.clone(),
        c.upgrade_response.clone(),
        c.preparation_permit.clone(),
        c.upgrade_now,
    );
    edit(&mut request, &mut response, &mut permit, &mut now);
    FixtureCase::PermitUpgrade {
        request,
        response: Box::new(response),
        preparation: c.preparation.clone(),
        preparation_permit: permit,
        now_ms: now,
    }
}
fn preparation_scope(
    c: &Chain,
    operation: RunnerOperation,
    edit: impl FnOnce(&mut DelegatedPermit),
) -> FixtureCase {
    let mut permit = c.preparation_permit.clone();
    edit(&mut permit);
    FixtureCase::PreparationPermit {
        authority: c.preparation.clone(),
        permit,
        operation,
        now_ms: c.issued_now,
    }
}
/// B1b vectors, generated only from the frozen phase-v2 and refresh-v1 cases.
fn handoff_fixtures(published: &[Fixture]) -> Vec<Fixture> {
    let [v2, refresh] = chains();
    let base = |name: &str| {
        published
            .iter()
            .find(|f| f.name == name)
            .unwrap()
            .case
            .clone()
    };
    let mut out = vec![];
    for (profile, c) in [("phase_v2", &v2), ("phase_refresh", &refresh)] {
        out.push(fixture(
            &format!("{profile}_issuance"),
            issuance(c, |_, _, _| {}),
            "accepted",
        ));
        out.push(fixture(
            &format!("{profile}_upgrade"),
            upgrade(c, |_, _, _, _| {}),
            "accepted",
        ));
    }
    let c = &v2;
    out.extend([
        fixture(
            "issuance_wrong_operation",
            issuance(c, |_, r, _| r.operation_id = id(0x7100)),
            "binding_mismatch",
        ),
        fixture(
            "issuance_wrong_sandbox",
            issuance(c, |q, _, _| q.sandbox_id = id(0x7101)),
            "binding_mismatch",
        ),
        fixture(
            "issuance_wrong_boot",
            issuance(c, |q, _, _| q.node.boot_id = id(0x7102)),
            "binding_mismatch",
        ),
        fixture(
            "issuance_wrong_generation",
            issuance(c, |q, _, _| q.grant_generation += 1),
            "binding_mismatch",
        ),
        fixture(
            "issuance_wrong_profile",
            issuance(c, |q, _, _| q.profile = PhaseProfile::PhaseRefreshV1),
            "binding_mismatch",
        ),
        fixture(
            "issuance_execution_scope",
            issuance(c, |_, r, _| r.permit.scope = PermitScope::Execution),
            "scope_denied",
        ),
        fixture(
            "issuance_outlives_preparation",
            issuance(c, |_, r, _| {
                r.permit.hard_deadline_ms += 1;
                r.permit.expires_at_ms += 1
            }),
            "deadline_mismatch",
        ),
        fixture(
            "issuance_expired",
            issuance(c, |_, r, n| *n = r.authority.preparation_deadline_ms),
            "expired",
        ),
        fixture(
            "issuance_unversioned",
            issuance(c, |q, _, _| q.schema_version = 2),
            "invalid_shape",
        ),
        fixture(
            "issuance_replay",
            FixtureCase::PermitIssuanceReplay {
                original_request: c.issue_request.clone(),
                original_response: Box::new(c.issue_response.clone()),
                request: c.issue_request.clone(),
                response: Box::new(c.issue_response.clone()),
                now_ms: c.issued_now,
            },
            "accepted",
        ),
        fixture(
            "issuance_changed_replay",
            FixtureCase::PermitIssuanceReplay {
                original_request: c.issue_request.clone(),
                original_response: Box::new(c.issue_response.clone()),
                request: PermitIssuanceRequest {
                    sandbox_id: id(0x7103),
                    ..c.issue_request.clone()
                },
                response: Box::new(c.issue_response.clone()),
                now_ms: c.issued_now,
            },
            "replay_conflict",
        ),
        fixture(
            "issuance_response_replay_conflict",
            FixtureCase::PermitIssuanceReplay {
                original_request: c.issue_request.clone(),
                original_response: Box::new(c.issue_response.clone()),
                request: c.issue_request.clone(),
                response: Box::new(PermitIssuanceResponse {
                    permit: DelegatedPermit {
                        credential: credential("preparation_0001"),
                        ..c.issue_response.permit.clone()
                    },
                    ..c.issue_response.clone()
                }),
                now_ms: c.issued_now,
            },
            "replay_conflict",
        ),
        fixture(
            "preparation_permits_prepared",
            preparation_scope(c, RunnerOperation::Prepared, |_| {}),
            "accepted",
        ),
        fixture(
            "preparation_fetches_material",
            preparation_scope(c, RunnerOperation::FetchPreparationMaterial, |_| {}),
            "accepted",
        ),
        fixture(
            "preparation_cannot_poll_mailbox",
            preparation_scope(c, RunnerOperation::MailboxPoll, |_| {}),
            "scope_denied",
        ),
        fixture(
            "preparation_cannot_ack_mailbox",
            preparation_scope(c, RunnerOperation::MailboxAck, |_| {}),
            "scope_denied",
        ),
        fixture(
            "preparation_authority_cannot_start",
            preparation_scope(c, RunnerOperation::Started, |_| {}),
            "scope_denied",
        ),
        fixture(
            "preparation_authority_cannot_accept",
            preparation_scope(c, RunnerOperation::AcceptOffer, |_| {}),
            "scope_denied",
        ),
        fixture(
            "preparation_foreign_attempt",
            preparation_scope(c, RunnerOperation::Prepared, |p| {
                p.binding.attempt_id = id(0x7104)
            }),
            "binding_mismatch",
        ),
        fixture(
            "preparation_wrong_boot",
            preparation_scope(c, RunnerOperation::Prepared, |p| {
                p.binding.node.boot_id = id(0x7105)
            }),
            "binding_mismatch",
        ),
        fixture(
            "preparation_execution_permit",
            preparation_scope(c, RunnerOperation::Prepared, |p| {
                p.scope = PermitScope::Execution
            }),
            "scope_denied",
        ),
        fixture(
            "upgrade_changes_permit_identity",
            upgrade(c, |_, r, _, _| r.permit.permit_id = id(0x7106)),
            "binding_mismatch",
        ),
        fixture(
            "upgrade_changes_channel",
            upgrade(c, |_, r, _, _| r.permit.binding.channel_id = id(0x7107)),
            "binding_mismatch",
        ),
        fixture(
            "upgrade_wrong_boot",
            upgrade(c, |_, r, _, _| r.permit.binding.node.boot_id = id(0x7108)),
            "binding_mismatch",
        ),
        fixture(
            "upgrade_wrong_operation",
            upgrade(c, |_, r, _, _| r.operation_id = id(0x7109)),
            "binding_mismatch",
        ),
        fixture(
            "upgrade_foreign_profile",
            upgrade(c, |q, _, _, _| q.profile = PhaseProfile::PhaseRefreshV1),
            "binding_mismatch",
        ),
        fixture(
            "upgrade_keeps_preparation_scope",
            upgrade(c, |_, r, _, _| r.permit.scope = PermitScope::Preparation),
            "scope_denied",
        ),
        fixture(
            "upgrade_outlives_grant",
            upgrade(c, |_, r, _, _| {
                r.permit.expires_at_ms = r.permit.hard_deadline_ms
            }),
            "deadline_mismatch",
        ),
        fixture(
            "upgrade_keeps_preparation_deadline",
            upgrade(c, |_, r, p, _| {
                r.permit.hard_deadline_ms = p.hard_deadline_ms
            }),
            "deadline_mismatch",
        ),
        fixture(
            "prepared_after_preparation_deadline",
            upgrade(c, |_, _, p, n| *n = p.hard_deadline_ms),
            "expired",
        ),
        fixture(
            "prepared_under_execution_permit",
            upgrade(c, |_, r, p, _| *p = r.permit.clone()),
            "scope_denied",
        ),
        fixture(
            "upgrade_replay",
            FixtureCase::PermitUpgradeReplay {
                original_request: c.upgrade_request.clone(),
                original_response: Box::new(c.upgrade_response.clone()),
                request: c.upgrade_request.clone(),
                response: Box::new(c.upgrade_response.clone()),
                preparation: c.preparation.clone(),
                preparation_permit: c.preparation_permit.clone(),
                now_ms: c.upgrade_now,
            },
            "accepted",
        ),
        fixture(
            "upgrade_changed_replay",
            FixtureCase::PermitUpgradeReplay {
                original_request: c.upgrade_request.clone(),
                original_response: Box::new(c.upgrade_response.clone()),
                request: c.upgrade_request.clone(),
                response: Box::new(PermitUpgradeResponse {
                    permit: DelegatedPermit {
                        credential: credential("execution_00001"),
                        ..c.upgrade_response.permit.clone()
                    },
                    ..c.upgrade_response.clone()
                }),
                preparation: c.preparation.clone(),
                preparation_permit: c.preparation_permit.clone(),
                now_ms: c.upgrade_now,
            },
            "replay_conflict",
        ),
    ]);
    let FixtureCase::Permit {
        permit: execution,
        context: granted,
        now_ms,
        ..
    } = base("valid_execution")
    else {
        panic!()
    };
    let FixtureCase::Permit {
        permit: preparation,
        context: preparing,
        now_ms: preparing_now,
        ..
    } = base("valid_preparation")
    else {
        panic!()
    };
    for (name, permit, context, now, operation, expected) in [
        (
            "execution_polls_mailbox",
            &execution,
            &granted,
            now_ms,
            RunnerOperation::MailboxPoll,
            "accepted",
        ),
        (
            "execution_acks_mailbox",
            &execution,
            &granted,
            now_ms,
            RunnerOperation::MailboxAck,
            "accepted",
        ),
        (
            "execution_cannot_submit_prepared",
            &execution,
            &granted,
            now_ms,
            RunnerOperation::Prepared,
            "scope_denied",
        ),
        (
            "valid_preparation_prepared",
            &preparation,
            &preparing,
            preparing_now,
            RunnerOperation::Prepared,
            "accepted",
        ),
        (
            "preparation_permit_cannot_poll_mailbox",
            &preparation,
            &preparing,
            preparing_now,
            RunnerOperation::MailboxPoll,
            "scope_denied",
        ),
    ] {
        out.push(fixture(
            name,
            FixtureCase::Permit {
                permit: permit.clone(),
                context: context.clone(),
                operation,
                now_ms: now,
            },
            expected,
        ));
    }
    let FixtureCase::Registration { registration } = base("valid_registration") else {
        panic!()
    };
    let enrolled = RegistrationResponse {
        schema_version: 2,
        operation_id: registration.registration.operation_id,
        node: registration.registration.identity.clone(),
        compatibility: CompatibilityReply::Compatible,
    };
    let enroll = |edit: fn(&mut RegistrationResponse)| {
        let mut response = enrolled.clone();
        edit(&mut response);
        FixtureCase::RegistrationEnrollment {
            registration: Box::new(registration.clone()),
            response,
        }
    };
    out.extend([
        fixture("registration_enrolled", enroll(|_| {}), "accepted"),
        fixture(
            "registration_declined",
            enroll(|r| {
                r.compatibility = CompatibilityReply::UnsupportedRequiredCapability {
                    missing: vec!["vm-isolation".into()],
                }
            }),
            "unsupported_required_capability",
        ),
        fixture(
            "registration_wrong_node",
            enroll(|r| r.node.boot_id = id(0x7110)),
            "binding_mismatch",
        ),
        fixture(
            "registration_wrong_operation",
            enroll(|r| r.operation_id = id(0x7111)),
            "binding_mismatch",
        ),
        fixture(
            "registration_response_v1",
            enroll(|r| r.schema_version = 1),
            "invalid_shape",
        ),
        fixture(
            "registration_replay",
            FixtureCase::RegistrationReplay {
                original_registration: Box::new(registration.clone()),
                original_response: enrolled.clone(),
                registration: Box::new(registration.clone()),
                response: enrolled.clone(),
            },
            "accepted",
        ),
        fixture(
            "registration_changed_replay",
            FixtureCase::RegistrationReplay {
                original_registration: Box::new(registration.clone()),
                original_response: enrolled.clone(),
                registration: Box::new(Registration {
                    backend: BackendAdvertisement {
                        max_concurrent_sandboxes: 5,
                        ..registration.backend.clone()
                    },
                    ..registration.clone()
                }),
                response: enrolled.clone(),
            },
            "replay_conflict",
        ),
    ]);
    out
}
#[test]
fn sandbox_handoff_vectors_derive_from_frozen_phase_payloads() {
    let published: Vec<Fixture> =
        serde_json::from_str(include_str!("../artifacts/sandbox-v1.fixtures.json")).unwrap();
    let generated = handoff_fixtures(&published);
    if let Ok(path) = std::env::var("AQ_SANDBOX_HANDOFF_OUT") {
        std::fs::write(path, serde_json::to_string_pretty(&generated).unwrap()).unwrap();
    }
    let wrong: Vec<_> = generated
        .iter()
        .filter(|f| sandbox::validate_fixture(f) != f.expected)
        .map(|f| (f.name.as_str(), sandbox::validate_fixture(f)))
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}");
    for fixture in &generated {
        let found = published.iter().find(|f| f.name == fixture.name);
        assert_eq!(
            found.map(|f| serde_json::to_value(f).unwrap()),
            Some(serde_json::to_value(fixture).unwrap()),
            "{} must be published unchanged",
            fixture.name
        );
    }
}
