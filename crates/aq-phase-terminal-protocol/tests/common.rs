#![allow(dead_code)]
use aq_admission::{granted_closure as closure, preparation_v2 as phase, started, stopped};
use aq_phase_terminal_protocol::*;
use serde_json::{Value, json};
pub struct Case {
    pub proposal: phase::Proposal,
    pub acceptance: phase::AcceptRequest,
    pub accepted: phase::AcceptedReceipt,
    pub prepare: phase::PrepareRequest,
    pub execution: phase::ExecutionReceipt,
    pub prepared: closure::JournalCheckpoint,
    pub started: StartedRequest,
    pub stopped: StoppedRequest,
    pub closures: Vec<ClosureRequest>,
}
impl Case {
    pub fn context(&self) -> Context<'_> {
        Context {
            execution: &self.execution,
            prepare: &self.prepare,
            accepted: &self.accepted,
            acceptance: &self.acceptance,
            proposal: &self.proposal,
            runtime: &self.started.original.runtime,
            runtime_qualification: &self.started.original.qualification,
            prepared: &self.prepared,
        }
    }
}
fn set(v: &mut Value, key: &str, val: &impl serde::Serialize) {
    v[key] = serde_json::to_value(val).unwrap();
}
pub fn case() -> Case {
    let p: Value = serde_json::from_str(include_str!("data/preparation-v2.fixture.json")).unwrap();
    let proposal = serde_json::from_value(p["proposal"].clone()).unwrap();
    let acceptance = serde_json::from_value(p["accept"].clone()).unwrap();
    let accepted: phase::AcceptedReceipt = serde_json::from_value(p["accepted"].clone()).unwrap();
    let prepare: phase::PrepareRequest = serde_json::from_value(p["prepare"].clone()).unwrap();
    let execution: phase::ExecutionReceipt =
        serde_json::from_value(p["execution"].clone()).unwrap();
    let prepared = closure::JournalCheckpoint {
        revision: prepare.original.journal_revision,
        digest: prepare.original.journal_digest.clone(),
    };
    let mut s: started::Request =
        serde_json::from_str(include_str!("data/started.fixture.json")).unwrap();
    s.binding = execution.grant.binding.clone();
    s.grant_id = execution.grant.grant_id;
    s.guardian_id = accepted.lease.guardian_id;
    s.launch_id = accepted.lease.launch_id;
    s.reservation_id = accepted.lease.hold.reservation.reservation_id;
    s.journal_revision = prepared.revision + 1;
    s.observation.guardian.intent.launch_id = s.launch_id;
    s.observation.guardian.intent.session = s.binding.session.clone();
    s.observation.guardian.intent.manifest_digest = s.binding.launch_manifest_digest.clone();
    s.observation_digest = s.observation.digest().unwrap();
    let c = Context {
        execution: &execution,
        prepare: &prepare,
        accepted: &accepted,
        acceptance: &acceptance,
        proposal: &proposal,
        runtime: &s.runtime,
        runtime_qualification: &s.qualification,
        prepared: &prepared,
    };
    let origin = Origin::for_context(&c).unwrap();
    let negotiation = Negotiation::current().unwrap();
    let started = StartedRequest {
        schema_version: 1,
        negotiation: negotiation.clone(),
        origin: origin.clone(),
        original: s,
    };
    let mut t: Value = serde_json::from_str(include_str!("data/stopped.fixture.json")).unwrap();
    set(&mut t, "runtime", &started.original.runtime);
    set(&mut t, "qualification", &started.original.qualification);
    set(&mut t, "reservation_id", &started.original.reservation_id);
    set(&mut t["evidence"], "binding", &started.original.binding);
    set(
        &mut t["evidence"],
        "guardian_id",
        &started.original.guardian_id,
    );
    set(&mut t["evidence"], "launch_id", &started.original.launch_id);
    set(
        &mut t["evidence"]["disposition"],
        "grant_id",
        &started.original.grant_id,
    );
    t["evidence"]["journal"]["revision"] = json!(started.original.journal_revision + 1);
    set(
        &mut t["proof"]["observation"],
        "guardian",
        &started.original.observation.guardian,
    );
    set(
        &mut t["proof"]["observation"],
        "pane",
        &started.original.observation.pane,
    );
    let proof: stopped::HostStoppedProof = serde_json::from_value(t["proof"].clone()).unwrap();
    t["evidence"]["disposition"]["stop_receipt_digest"] = json!(proof.digest().unwrap());
    let stopped = StoppedRequest {
        schema_version: 1,
        negotiation: negotiation.clone(),
        origin: origin.clone(),
        original: serde_json::from_value(t).unwrap(),
    };
    let old: Value =
        serde_json::from_str(include_str!("data/granted-closure.fixture.json")).unwrap();
    let mut closures = Vec::new();
    for value in old["valid_requests"].as_array().unwrap() {
        let mut q: closure::Request = serde_json::from_value(value.clone()).unwrap();
        q.binding = started.original.binding.clone();
        q.grant_id = started.original.grant_id;
        q.guardian_id = started.original.guardian_id;
        q.launch_id = started.original.launch_id;
        q.reservation_id = started.original.reservation_id;
        q.journal.revision = prepared.revision + 3;
        match &mut q.evidence {
            closure::Evidence::Suppressed {
                record,
                record_digest,
            } => {
                record.binding = q.binding.clone();
                record.grant_id = q.grant_id;
                record.guardian_id = q.guardian_id;
                record.launch_id = q.launch_id;
                record.reservation_id = q.reservation_id;
                record.previous = prepared.clone();
                record.terminal_revision = q.journal.revision;
                *record_digest = record.digest().unwrap();
            }
            closure::Evidence::HostStopped {
                driver_intent,
                proof,
                proof_digest,
            } => {
                driver_intent.binding = q.binding.clone();
                driver_intent.grant_id = q.grant_id;
                driver_intent.guardian_id = q.guardian_id;
                driver_intent.reservation_id = q.reservation_id;
                driver_intent.host.launch_id = q.launch_id;
                driver_intent.host.session = q.binding.session.clone();
                driver_intent.host.manifest_digest = q.binding.launch_manifest_digest.clone();
                driver_intent.journal.revision = prepared.revision + 1;
                proof.observation.guardian.intent = driver_intent.host.clone();
                *proof_digest = proof.digest().unwrap();
            }
        }
        closures.push(ClosureRequest {
            schema_version: 1,
            negotiation: negotiation.clone(),
            origin: origin.clone(),
            original: q,
        });
    }
    Case {
        proposal,
        acceptance,
        accepted,
        prepare,
        execution,
        prepared,
        started,
        stopped,
        closures,
    }
}
pub fn fixtures() -> Value {
    let c = case();
    let q = &c.started;
    let started = StartedReceipt {
        schema_version: 1,
        negotiation: q.negotiation.clone(),
        origin: q.origin.clone(),
        request_digest: q.digest().unwrap(),
        original: started::Receipt {
            schema_version: 1,
            operation_id: q.original.operation_id,
            binding: q.original.binding.clone(),
            grant_id: q.original.grant_id,
            request_digest: q.original.digest().unwrap(),
            recorded_at_ms: 999999,
            authority_revision_at_record: 10,
        },
    };
    let q = &c.stopped;
    let stopped=StoppedReceipt{schema_version:1,negotiation:q.negotiation.clone(),origin:q.origin.clone(),request_digest:q.digest().unwrap(),original:serde_json::from_value(json!({"schema_version":1,"operation_id":q.original.evidence.stop_operation_id,"binding":q.original.evidence.binding,"grant_id":q.original.grant_id().unwrap(),"request_digest":q.original.digest().unwrap(),"recorded_at_ms":999999,"authority_revision_at_record":11})).unwrap()};
    let closures: Vec<_> = c
        .closures
        .iter()
        .map(|q| ClosureReceipt {
            schema_version: 1,
            negotiation: q.negotiation.clone(),
            origin: q.origin.clone(),
            request_digest: q.digest().unwrap(),
            original: closure::Receipt {
                schema_version: 1,
                operation_id: q.original.operation_id,
                binding: q.original.binding.clone(),
                grant_id: q.original.grant_id,
                request_digest: q.original.digest().unwrap(),
                recorded_at_ms: 999999,
                authority_revision_at_record: 11,
                disposition: closure::Disposition::LaunchClosedAwaitingSettlement,
            },
        })
        .collect();
    let history = HistoryRequest {
        schema_version: 1,
        negotiation: q.negotiation.clone(),
        operation_id: q.original.evidence.stop_operation_id,
        binding: c.execution.grant.binding.clone(),
        accepted_receipt_digest: c.accepted.digest().unwrap(),
        prepare_operation_id: c.prepare.original.operation_id,
    };
    let known = HistoryReceipt {
        schema_version: 1,
        negotiation: history.negotiation.clone(),
        request_digest: history.digest().unwrap(),
        observed_at_ms: 999999,
        identity: Some(HistoricalGrantIdentity::for_context(&c.context()).unwrap()),
    };
    let absent = HistoryReceipt {
        identity: None,
        ..known.clone()
    };
    json!({"case":"synthetic structural correlations only; no owner/proof/live authority","started_request":c.started,"stopped_request":c.stopped,"closure_requests":c.closures,"started_receipt":started,"stopped_receipt":stopped,"closure_receipts":closures,"history_request":history,"history_known":known,"history_absent":absent,"historical_readback":HistoricalReadback::Started(started)})
}
pub fn invalid_fixtures() -> Value {
    let f = fixtures();
    let mut docs = Vec::new();
    for (name, path, value) in [
        ("unknown_mutation_field", "/secret", json!("refuse")),
        (
            "unknown_feature",
            "/negotiation/features/0",
            json!("future_feature"),
        ),
        (
            "changed_origin",
            "/origin/execution_receipt_digest",
            json!(aq_admission::Digest::of(b"changed")),
        ),
        ("wrong_version", "/schema_version", json!(2)),
        (
            "wrong_runtime",
            "/original/runtime/digest",
            json!(aq_admission::Digest::of(b"wrong runtime")),
        ),
    ] {
        let mut q = f["started_request"].clone();
        if path == "/secret" {
            q["secret"] = value;
        } else {
            *q.pointer_mut(path).unwrap() = value;
        }
        docs.push(json!({"case":name,"family":"started_request","document":q,"requires_relational_validation":matches!(name,"changed_origin"|"wrong_runtime")}));
    }
    let mut t = f["stopped_request"].clone();
    t["original"]["proof"]["observation"]["kernel_wait_echild"] = json!(false);
    docs.push(json!({"case":"false_echild","family":"stopped_request","document":t,"requires_relational_validation":true}));
    let mut r = f["started_receipt"].clone();
    r["request_digest"] = json!(aq_admission::Digest::of(b"wrong wrapper"));
    docs.push(json!({"case":"wrong_outer_receipt_digest","family":"started_receipt","document":r,"requires_relational_validation":true}));
    let mut h = f["history_absent"].clone();
    h.as_object_mut().unwrap().remove("identity");
    docs.push(json!({"case":"omitted_nullable_identity","family":"history_receipt","document":h,"requires_relational_validation":false}));
    json!({"profile":"all documents must refuse; schema cannot prove relational cases","invalid":docs})
}
