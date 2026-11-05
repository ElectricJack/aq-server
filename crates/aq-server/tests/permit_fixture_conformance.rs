//! Shared owner vectors exercise the exact delegated contract consumed by the server.
use aq_admission_sandbox::sandbox::{self, Fixture, FixtureCase, SandboxDocument};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

const FIXTURES: &str =
    include_str!("../../../vendor/aq-admission-0.3.3/artifacts/sandbox-v1.fixtures.json");

#[test]
fn permit_fixture_conformance() {
    let fixtures: Vec<Fixture> = serde_json::from_str(FIXTURES).unwrap();
    for required in [
        "shared_build_cache_supported",
        "shared_build_cache_disabled",
        "ended_stopped_readopted_original_boot_replay",
        "ended_stopped_readopted_stale_permit",
        "ended_stopped_readopted_forged_permit",
        "ended_stopped_readopted_stale_context",
        "ended_stopped_readopted_changed_boot",
        "ended_stopped_readopted_rebound_boot",
        "ended_stopped_readopted_expired_replay",
    ] {
        assert!(
            fixtures.iter().any(|f| f.name == required),
            "missing {required}"
        );
    }
    let mut cases = BTreeMap::new();
    let mut outcomes = BTreeMap::new();
    for fixture in &fixtures {
        assert_eq!(
            sandbox::validate_fixture(fixture),
            fixture.expected,
            "owner vector {}",
            fixture.name
        );
        let kind = match fixture.case {
            FixtureCase::Permit { .. } => "permit",
            FixtureCase::StoppedReplay { .. } => "stopped_replay",
            FixtureCase::Readoption { .. } => "readoption",
            FixtureCase::ReadoptionReplay { .. } => "replay",
            FixtureCase::Compatibility { .. } => "compatibility",
            FixtureCase::Registration { .. } => "registration",
            FixtureCase::PhaseHandoff { .. } => "handoff",
            FixtureCase::RunnerRenewal { .. } => "renewal",
            FixtureCase::PreparationPermit { .. } => "preparation",
            FixtureCase::PermitIssuance { .. } => "issuance",
            FixtureCase::PermitIssuanceReplay { .. } => "issuance_replay",
            FixtureCase::PermitUpgrade { .. } => "upgrade",
            FixtureCase::PermitUpgradeReplay { .. } => "upgrade_replay",
            FixtureCase::RegistrationEnrollment { .. } => "enrollment",
            FixtureCase::RegistrationReplay { .. } => "registration_replay",
        };
        *cases.entry(kind).or_insert(0) += 1;
        *outcomes.entry(fixture.expected.as_str()).or_insert(0) += 1;
    }
    // Literal release inventory prevents an empty or partially copied corpus passing.
    assert_eq!(
        cases,
        BTreeMap::from([
            ("permit", 38),
            ("stopped_replay", 13),
            ("readoption", 5),
            ("replay", 4),
            ("compatibility", 5),
            ("registration", 3),
            ("handoff", 4),
            ("renewal", 18),
            ("preparation", 9),
            ("issuance", 11),
            ("issuance_replay", 3),
            ("upgrade", 12),
            ("upgrade_replay", 2),
            ("enrollment", 5),
            ("registration_replay", 2),
        ])
    );
    assert_eq!(
        outcomes,
        BTreeMap::from([
            ("accepted", 29),
            ("binding_mismatch", 30),
            ("deadline_mismatch", 12),
            ("expired", 13),
            ("invalid_shape", 5),
            ("replay_conflict", 11),
            ("scope_denied", 30),
            ("unsupported_required_capability", 4),
        ])
    );
    let published_schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../vendor/aq-admission-0.3.3/artifacts/sandbox-v1.schema.json"
    ))
    .unwrap();
    assert_eq!(sandbox::schema(), published_schema);
}

#[test]
fn permit_fixture_conformance_shared_build_cache_opt_in() {
    use sandbox::{BackendCapability, CompatibilityReply};

    let fixtures: Vec<Fixture> = serde_json::from_str(FIXTURES).unwrap();
    let fixture = fixtures
        .iter()
        .find(|f| f.name == "valid_registration")
        .unwrap();
    let FixtureCase::Registration { mut registration } = fixture.case.clone() else {
        panic!("valid_registration must exercise the registration envelope");
    };
    let required = vec!["shared-build-cache".to_owned()];
    let supported = |registration: &sandbox::Registration| -> Vec<String> {
        registration.validate().unwrap();
        serde_json::from_value(serde_json::to_value(&registration.backend.capabilities).unwrap())
            .unwrap()
    };
    // Existing nodes do not acquire cache support by upgrading the contract.
    assert!(
        !registration
            .backend
            .capabilities
            .contains(&BackendCapability::SharedBuildCache)
    );
    assert_eq!(
        sandbox::negotiate(&required, &supported(&registration)).unwrap(),
        CompatibilityReply::UnsupportedRequiredCapability {
            missing: required.clone()
        }
    );
    registration
        .backend
        .capabilities
        .push(BackendCapability::SharedBuildCache);
    let bytes = serde_json::to_vec(&SandboxDocument::Register(registration)).unwrap();
    let SandboxDocument::Register(registration) = sandbox::decode_document(&bytes).unwrap() else {
        panic!("registration must round-trip");
    };
    assert_eq!(
        sandbox::negotiate(&required, &supported(&registration)).unwrap(),
        CompatibilityReply::Compatible
    );
}

#[test]
fn permit_fixture_conformance_source_pin() {
    #[derive(Deserialize)]
    struct Pin {
        source_revision: String,
        archive: String,
        archive_sha256: String,
        directory: String,
        files: BTreeMap<String, String>,
    }
    let pin: Pin =
        serde_json::from_str(include_str!("../../../vendor/admission-contract.json")).unwrap();
    assert_eq!(pin.source_revision.len(), 40);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let archive = fs::read(root.join(pin.archive)).unwrap();
    assert_eq!(format!("{:x}", Sha256::digest(archive)), pin.archive_sha256);
    assert_eq!(pin.files.len(), 120);
    for (path, expected) in pin.files {
        let actual = fs::read(root.join(&pin.directory).join(&path)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(actual)), expected, "{path}");
    }
}

#[test]
fn permit_fixture_conformance_frozen_transport() {
    let legacy: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../aq-admission/artifacts/transport.fixtures.json"
    ))
    .unwrap();
    assert!(!legacy.is_empty());
    for value in legacy {
        let bytes = serde_json::to_vec(&value).unwrap();
        let frozen = aq_admission::transport::decode_transport(&bytes).unwrap();
        let next = aq_admission_sandbox::transport::decode_transport(&bytes).unwrap();
        assert_eq!(serde_json::to_value(frozen).unwrap(), value);
        assert_eq!(serde_json::to_value(next).unwrap(), value);
    }
    let fixtures: Vec<Fixture> = serde_json::from_str(FIXTURES).unwrap();
    let registration = fixtures
        .iter()
        .find(|f| f.name == "valid_registration")
        .unwrap();
    let FixtureCase::Registration { registration } = &registration.case else {
        panic!("valid_registration must exercise the registration envelope");
    };
    let bytes = serde_json::to_vec(&SandboxDocument::Register(registration.clone())).unwrap();
    sandbox::decode_document(&bytes).unwrap();
    // Frozen endpoints explicitly reject the new envelope during negotiation.
    assert!(aq_admission::transport::decode_transport(&bytes).is_err());
}

#[test]
fn published_runner_receipts_match_native_owners() {
    use aq_server::delegated_permits::receipts::Envelope;
    use sandbox::{DelegatedPermit, PermitContext, RunnerRenewalResponse};
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/implementation/B2-evidence/runner-receipts");
    let manifest: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(directory.join("sha256.json")).unwrap()).unwrap();
    assert_eq!(manifest.len(), 8);
    for (name, expected) in manifest {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(fs::read(directory.join(name)).unwrap())
            ),
            expected
        );
    }
    let read = |name: &str| -> serde_json::Value {
        serde_json::from_slice(&fs::read(directory.join(format!("{name}.json"))).unwrap()).unwrap()
    };
    let renewal = read("renewal");
    let SandboxDocument::RunnerRenewalRequest(request) =
        serde_json::from_value(renewal["request"].clone()).unwrap()
    else {
        panic!()
    };
    let SandboxDocument::RunnerRenewalResponse(response) =
        serde_json::from_value(renewal["response"].clone()).unwrap()
    else {
        panic!()
    };
    let context: PermitContext = serde_json::from_value(renewal["context"].clone()).unwrap();
    response
        .validate_for(
            &request,
            &context,
            response.delivery.as_ref().unwrap().nonce,
            renewal["server_now_ms"].as_u64().unwrap(),
        )
        .unwrap();
    for profile in ["PhaseV2", "PhaseRefreshV1"] {
        let source = read(&format!("source-{profile}"));
        let source_envelope: Envelope = serde_json::from_value(source["receipts"].clone()).unwrap();
        let source_receipt = source_envelope.source.unwrap();
        let native_source: aq_worker_protocol::TransferStatus =
            serde_json::from_value(source["status"].clone()).unwrap();
        let native_source = native_source.receipt.unwrap();
        assert_eq!(source_receipt.receipt_id, native_source.receipt_id);
        assert_eq!(
            source_receipt.artifact_digest.as_str(),
            native_source.manifest_digest
        );
        let result = read(&format!("result-{profile}"));
        let report: aq_admission::transport::ResultReport =
            serde_json::from_value(result["request"].clone()).unwrap();
        let native: aq_admission::result_record::Receipt =
            serde_json::from_value(result["response"].clone()).unwrap();
        native.validate_for(&report).unwrap();
        let envelope: Envelope = serde_json::from_value(result["receipts"].clone()).unwrap();
        let central = envelope.result.unwrap();
        let finished = envelope.finished.unwrap();
        assert_eq!(source_receipt.attempt, central.attempt);
        assert_eq!(central.attempt, finished.attempt);
        assert_eq!(central.attempt.launch_id, report.stop.launch_id);
        assert_eq!(central.attempt.attempt_id, report.binding.attempt_id);
        assert_eq!(
            central.attempt.manifest_digest,
            report.binding.launch_manifest_digest
        );
        assert_eq!(central.receipt_id, native.report_id);
        assert_eq!(finished.receipt_id, native.report_id);
        assert_eq!(central.artifact_digest, native.report_digest);
        assert!(
            report
                .source_receipt_ids
                .contains(&source_receipt.receipt_id)
        );
    }
    let adoption = read("readoption");
    let request: sandbox::ReadoptionRequest =
        serde_json::from_value(adoption["request"].clone()).unwrap();
    let response: sandbox::ReadoptionResponse =
        serde_json::from_value(adoption["response"].clone()).unwrap();
    request.validate().unwrap();
    response.validate().unwrap();
    assert_eq!(request.operation_id, response.operation_id);
    let awaiting: RunnerRenewalResponse =
        serde_json::from_value(adoption["awaiting"].clone()).unwrap();
    assert!(matches!(
        awaiting.reply,
        sandbox::RenewalReply::AwaitingNodeReadoption { .. }
    ));
    assert!(awaiting.delivery.is_none());
    let replay = read("readoption-stopped-replay-accepted");
    let permit: DelegatedPermit = serde_json::from_value(replay["permit"].clone()).unwrap();
    let context: PermitContext = serde_json::from_value(replay["context"].clone()).unwrap();
    let stopped: aq_admission_sandbox::stopped::Request =
        serde_json::from_value(replay["request"]["original"].clone()).unwrap();
    let aq_admission_sandbox::SessionIdentity::Remote { boot_id, .. } =
        &stopped.evidence.binding.session
    else {
        panic!()
    };
    assert_ne!(*boot_id, permit.binding.node.boot_id);
    let receipt: aq_admission_sandbox::stopped::Receipt =
        serde_json::from_value(replay["replay_response"]["original"].clone()).unwrap();
    receipt.validate_for(&stopped).unwrap();
    assert_eq!(
        permit.validate_stopped_replay(
            &context,
            &stopped,
            &stopped,
            replay["server_now_ms"].as_u64().unwrap()
        ),
        Ok(())
    );
    assert_eq!(replay["http_status"], 200);
    assert_eq!(replay["original_response"], replay["replay_response"]);
}
