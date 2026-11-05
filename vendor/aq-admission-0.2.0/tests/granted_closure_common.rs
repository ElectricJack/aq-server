#![allow(dead_code)]
use aq_admission::{granted_closure::*, *};
use serde_json::{Value, json};
use uuid::Uuid;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
pub fn context() -> GrantedContext {
    let control: Value =
        serde_json::from_str(include_str!("../artifacts/control.fixture.json")).unwrap();
    let initial = &control["replies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "initial_grant")
        .unwrap()["body"];
    let started: Value =
        serde_json::from_str(include_str!("../artifacts/started.fixture.json")).unwrap();
    GrantedContext {
        grant: serde_json::from_value(initial["original"].clone()).unwrap(),
        runtime: serde_json::from_value(started["runtime"].clone()).unwrap(),
        qualification: serde_json::from_value(started["qualification"].clone()).unwrap(),
        prepared: JournalCheckpoint {
            revision: 10,
            digest: Digest::of(b"synthetic original Prepared journal"),
        },
    }
}
pub fn requests() -> Vec<Request> {
    let c = context();
    let g = &c.grant;
    let record = SuppressionRecord {
        schema_version: 1,
        operation_id: id(901),
        suppression_id: id(902),
        binding: g.grant.binding.clone(),
        grant_id: g.grant.grant_id,
        guardian_id: g.guardian_id,
        launch_id: g.launch_id,
        reservation_id: g.reservation_id,
        original_kernel_boot_id: id(903),
        recording_kernel_boot_id: id(904),
        previous: c.prepared.clone(),
        terminal_revision: 11,
    };
    let suppressed = Request {
        schema_version: 1,
        operation_id: record.operation_id,
        binding: record.binding.clone(),
        grant_id: record.grant_id,
        guardian_id: record.guardian_id,
        launch_id: record.launch_id,
        reservation_id: record.reservation_id,
        runtime: c.runtime.clone(),
        qualification: c.qualification.clone(),
        journal: JournalCheckpoint {
            revision: 11,
            digest: Digest::of(b"synthetic terminal suppression snapshot"),
        },
        evidence: Evidence::Suppressed {
            record_digest: record.digest().unwrap(),
            record,
        },
    };
    let stopped: Value =
        serde_json::from_str(include_str!("../artifacts/stopped.fixture.json")).unwrap();
    let mut observation: StoppedObservation =
        serde_json::from_value(stopped["proof"]["observation"].clone()).unwrap();
    observation.guardian.intent.launch_id = g.launch_id;
    observation.guardian.intent.session = g.grant.binding.session.clone();
    observation.guardian.intent.manifest_digest = g.grant.binding.launch_manifest_digest.clone();
    let mut full = suppressed.clone();
    full.operation_id = id(905);
    full.journal = JournalCheckpoint {
        revision: 12,
        digest: Digest::of(b"synthetic stopped closure snapshot"),
    };
    let driver_intent = DriverIntent {
        binding: full.binding.clone(),
        grant_id: full.grant_id,
        guardian_id: full.guardian_id,
        reservation_id: full.reservation_id,
        host: observation.guardian.intent.clone(),
        journal: JournalCheckpoint {
            revision: 11,
            digest: Digest::of(b"synthetic pre-hook driver launch intent"),
        },
    };
    let proof = HostStoppedProof { observation };
    full.evidence = Evidence::HostStopped {
        driver_intent: Box::new(driver_intent),
        proof_digest: proof.digest().unwrap(),
        proof,
    };
    let mut server_only = full.clone();
    server_only.operation_id = id(906);
    if let Evidence::HostStopped {
        proof,
        proof_digest,
        ..
    } = &mut server_only.evidence
    {
        proof.observation.pane = None;
        *proof_digest = proof.digest().unwrap();
    }
    let mut no_children = server_only.clone();
    no_children.operation_id = id(907);
    if let Evidence::HostStopped {
        proof,
        proof_digest,
        ..
    } = &mut no_children.evidence
    {
        proof.observation.root_status = None;
        proof.observation.reaped_count = 0;
        *proof_digest = proof.digest().unwrap();
    }
    vec![suppressed, full, server_only, no_children]
}
pub fn fixtures() -> Value {
    let requests = requests();
    let c = context();
    for r in &requests {
        r.validate_against(&c).unwrap();
    }
    let receipts = requests
        .iter()
        .map(|r| Receipt {
            schema_version: 1,
            operation_id: r.operation_id,
            binding: r.binding.clone(),
            grant_id: r.grant_id,
            request_digest: r.digest().unwrap(),
            recorded_at_ms: 9000,
            authority_revision_at_record: 7,
            disposition: Disposition::LaunchClosedAwaitingSettlement,
        })
        .collect::<Vec<_>>();
    let mut invalid = vec![];
    for (name, pointer, value) in [
        ("unknown_input", "/secret", json!("forbidden")),
        ("nil_operation", "/operation_id", json!(Uuid::nil())),
        (
            "unknown_disposition",
            "/evidence/kind",
            json!("never_granted"),
        ),
        ("zero_terminal_revision", "/journal/revision", json!(0)),
        (
            "changed_suppression_boot",
            "/evidence/record/binding/session/boot_id",
            json!(id(999)),
        ),
        ("overflow_revision", "/journal/revision", json!(u64::MAX)),
        (
            "changed_record_digest",
            "/evidence/record_digest",
            json!(Digest::of(b"changed")),
        ),
    ] {
        let mut body = serde_json::to_value(&requests[0]).unwrap();
        if pointer == "/secret" {
            body["secret"] = value;
        } else {
            *body.pointer_mut(pointer).unwrap() = value;
        }
        invalid.push(json!({"case":name,"body":body}));
    }
    for (name, pointer, value) in [
        (
            "false_echild",
            "/evidence/proof/observation/kernel_wait_echild",
            json!(false),
        ),
        (
            "running_is_not_stopped",
            "/evidence/proof/observation/phase",
            json!("running"),
        ),
        (
            "changed_physical_intent",
            "/evidence/proof/observation/guardian/intent/configuration_digest",
            json!(Digest::of(b"different config")),
        ),
        (
            "zero_guardian_birth",
            "/evidence/proof/observation/guardian/process/start_ticks",
            json!(0),
        ),
        (
            "bad_pid_namespace",
            "/evidence/proof/observation/guardian/pid_namespace",
            json!("pid:[not-a-number]"),
        ),
        (
            "root_status_without_reap",
            "/evidence/proof/observation/reaped_count",
            json!(0),
        ),
    ] {
        let mut body = serde_json::to_value(&requests[1]).unwrap();
        *body.pointer_mut(pointer).unwrap() = value;
        invalid.push(json!({"case":name,"body":body}));
    }
    json!({"profile":"pure synthetic contracts; no opaque journal/kernel attestation","valid_requests":requests,"valid_receipts":receipts,"invalid_shape":invalid})
}
