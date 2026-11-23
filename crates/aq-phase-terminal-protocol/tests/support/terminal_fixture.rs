use aq_admission::{granted_closure as closure, started, stopped};
use serde_json::{Value, json};

fn set(v: &mut Value, key: &str, val: &impl serde::Serialize) {
    v[key] = serde_json::to_value(val).unwrap();
}
pub(super) fn stopped(started: &started::Request) -> stopped::Request {
    let mut t: Value = serde_json::from_str(include_str!("../data/stopped.fixture.json")).unwrap();
    set(&mut t, "runtime", &started.runtime);
    set(&mut t, "qualification", &started.qualification);
    set(&mut t, "reservation_id", &started.reservation_id);
    set(&mut t["evidence"], "binding", &started.binding);
    set(&mut t["evidence"], "guardian_id", &started.guardian_id);
    set(&mut t["evidence"], "launch_id", &started.launch_id);
    set(
        &mut t["evidence"]["disposition"],
        "grant_id",
        &started.grant_id,
    );
    t["evidence"]["journal"]["revision"] = json!(started.journal_revision + 1);
    set(
        &mut t["proof"]["observation"],
        "guardian",
        &started.observation.guardian,
    );
    set(
        &mut t["proof"]["observation"],
        "pane",
        &started.observation.pane,
    );
    let proof: stopped::HostStoppedProof = serde_json::from_value(t["proof"].clone()).unwrap();
    t["evidence"]["disposition"]["stop_receipt_digest"] = json!(proof.digest().unwrap());
    serde_json::from_value(t).unwrap()
}

pub(super) fn closures(
    started: &started::Request,
    prepared: &closure::JournalCheckpoint,
) -> Vec<closure::Request> {
    let old: Value =
        serde_json::from_str(include_str!("../data/granted-closure.fixture.json")).unwrap();
    let mut closures = Vec::new();
    for value in old["valid_requests"].as_array().unwrap() {
        let mut q: closure::Request = serde_json::from_value(value.clone()).unwrap();
        q.binding = started.binding.clone();
        q.grant_id = started.grant_id;
        q.guardian_id = started.guardian_id;
        q.launch_id = started.launch_id;
        q.reservation_id = started.reservation_id;
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
        closures.push(q);
    }
    closures
}

pub(super) fn bind_observation(s: &mut started::Request) {
    s.observation.guardian.intent.launch_id = s.launch_id;
    s.observation.guardian.intent.session = s.binding.session.clone();
    s.observation.guardian.intent.manifest_digest = s.binding.launch_manifest_digest.clone();
    s.observation_digest = s.observation.digest().unwrap();
}
