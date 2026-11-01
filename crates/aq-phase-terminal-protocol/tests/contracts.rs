mod common;
use aq_admission::{Digest, SessionIdentity, granted_closure::Evidence};
use aq_phase_terminal_protocol::*;
use serde_json::{Value, json};
fn fixtures() -> Value {
    common::fixtures()
}
#[test]
fn actual_phase_history_structural_linkage_and_historical_receipts() {
    let c = common::case();
    c.context().validate().unwrap();
    c.started.validate_for(&c.context()).unwrap();
    c.stopped.validate_for(&c.context(), &c.started).unwrap();
    for q in &c.closures {
        q.validate_for(&c.context()).unwrap();
    }
    let f = fixtures();
    let sr: StartedReceipt = serde_json::from_value(f["started_receipt"].clone()).unwrap();
    sr.validate_for(&c.started).unwrap();
    sr.digest_for(&c.started).unwrap();
    let tr: StoppedReceipt = serde_json::from_value(f["stopped_receipt"].clone()).unwrap();
    tr.validate_for(&c.stopped).unwrap();
    tr.digest_for(&c.stopped).unwrap();
    for (q, r) in c
        .closures
        .iter()
        .zip(f["closure_receipts"].as_array().unwrap())
    {
        let r: ClosureReceipt = serde_json::from_value(r.clone()).unwrap();
        r.validate_for(q).unwrap();
        r.digest_for(q).unwrap();
    }
    // No now parameter: expired historical receipt replay is not live permission.
    assert!(sr.original.recorded_at_ms > c.execution.execution_deadline_ms);
}
#[test]
fn origin_boot_generation_runtime_and_prepared_must_match_actual_history() {
    let c = common::case();
    for field in ["execution_receipt_digest", "accepted_receipt_digest"] {
        let mut q = c.started.clone();
        let mut v = serde_json::to_value(&q).unwrap();
        v["origin"][field] = json!(Digest::of(b"changed"));
        q = serde_json::from_value(v).unwrap();
        assert!(q.validate_for(&c.context()).is_err());
    }
    let mut q = c.started.clone();
    if let SessionIdentity::Remote { boot_id, .. } = &mut q.original.binding.session {
        *boot_id = uuid::Uuid::from_u128(999);
    }
    assert!(q.validate_for(&c.context()).is_err());
    let mut q = c.started.clone();
    q.original.binding.assignment_generation += 1;
    assert!(q.validate_for(&c.context()).is_err());
    let mut q = c.started.clone();
    q.original.runtime.digest = Digest::of(b"other runtime");
    assert!(q.validate_for(&c.context()).is_err());
    let mut q = c.started.clone();
    q.original.journal_revision = c.prepared.revision;
    assert!(q.validate_for(&c.context()).is_err());
    let mut p = c.prepared.clone();
    p.revision = 0;
    let mut cx = c.context();
    cx.prepared = &p;
    assert!(cx.validate().is_err());
}
#[test]
fn stopped_rejects_false_echild_guardian_pane_and_changed_origin() {
    let c = common::case();
    let mut q = c.stopped.clone();
    q.original.proof.observation.kernel_wait_echild = false;
    assert!(q.validate_for(&c.context(), &c.started).is_err());
    let mut q = c.stopped.clone();
    q.original.proof.observation.guardian.nonce = uuid::Uuid::from_u128(999);
    assert!(q.validate_for(&c.context(), &c.started).is_err());
    let mut q = c.stopped.clone();
    q.original.proof.observation.pane.pane.start_ticks += 1;
    assert!(q.validate_for(&c.context(), &c.started).is_err());
    let mut q = c.stopped.clone();
    q.origin.execution_receipt_digest = Digest::of(b"other");
    assert!(q.validate_for(&c.context(), &c.started).is_err());
}
#[test]
fn closure_direct_phase_checkpoint_both_suppression_and_real_stop_shapes() {
    let c = common::case();
    assert!(
        c.closures
            .iter()
            .any(|q| matches!(q.original.evidence, Evidence::Suppressed { .. }))
    );
    assert!(
        c.closures
            .iter()
            .any(|q| matches!(q.original.evidence, Evidence::HostStopped { .. }))
    );
    for original in &c.closures {
        let mut q = original.clone();
        q.original.grant_id = uuid::Uuid::from_u128(999);
        assert!(q.validate_for(&c.context()).is_err());
    }
    let mut q = c.closures[0].clone();
    if let Evidence::Suppressed {
        record,
        record_digest,
    } = &mut q.original.evidence
    {
        record.previous.digest = Digest::of(b"wrong checkpoint");
        *record_digest = record.digest().unwrap();
    }
    assert!(q.validate_for(&c.context()).is_err());
}
#[test]
fn negotiation_unknown_duplicate_reordered_or_missing_features_refuse() {
    let c = common::case();
    let mut q = c.started.clone();
    q.negotiation.features.pop();
    assert!(q.validate().is_err());
    let mut q = c.started.clone();
    q.negotiation.features[1] = q.negotiation.features[0];
    assert!(q.validate().is_err());
    let mut q = c.started.clone();
    q.negotiation.features.reverse();
    assert!(q.validate().is_err());
    let mut v = serde_json::to_value(&c.started).unwrap();
    v["negotiation"]["features"][0] = json!("future_feature");
    assert!(decode_started_request(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut q = c.started.clone();
    q.negotiation.contract.digest = Digest::of(b"other contract");
    assert!(q.validate().is_err());
}
#[test]
fn strict_wire_inclusive_bound_unknown_keys_floats_and_duplicate_fields() {
    let c = common::case();
    let mut b = serde_json::to_vec(&c.started).unwrap();
    b.resize(MAX_BYTES, b' ');
    decode_started_request(&b).unwrap();
    b.push(b' ');
    assert!(decode_started_request(&b).is_err());
    let mut v = serde_json::to_value(&c.started).unwrap();
    v["secret"] = json!("must not be accepted");
    assert!(decode_started_request(&serde_json::to_vec(&v).unwrap()).is_err());
    v.as_object_mut().unwrap().remove("secret");
    v["schema_version"] = json!(1.0);
    assert!(decode_started_request(&serde_json::to_vec(&v).unwrap()).is_err());
    let raw = serde_json::to_string(&c.started).unwrap();
    let duplicate = raw.replacen("{", "{\"schema_version\":1,", 1);
    assert!(decode_started_request(duplicate.as_bytes()).is_err());
    let mut q = c.started.clone();
    q.negotiation.protocol = "x".repeat(MAX_BYTES);
    assert!(q.validate().is_err());
}
#[test]
fn immutable_outer_identity_and_domain_separation() {
    let c = common::case();
    let f = fixtures();
    let r: StartedReceipt = serde_json::from_value(f["started_receipt"].clone()).unwrap();
    let mut q = c.started.clone();
    q.origin.execution_receipt_digest = Digest::of(b"changed");
    assert!(r.validate_for(&q).is_err());
    assert_ne!(q.digest().unwrap(), c.started.digest().unwrap());
    let mut q = c.started.clone();
    q.original.journal_digest = Digest::of(b"same op changed payload");
    assert_ne!(q.digest().unwrap(), c.started.digest().unwrap());
    assert!(r.validate_for(&q).is_err());
    assert_ne!(
        c.started.digest().unwrap(),
        c.started.original.digest().unwrap()
    );
    assert_ne!(
        r.digest_for(&c.started).unwrap(),
        c.started.digest().unwrap()
    );
    let mut v = serde_json::to_value(&c.started).unwrap();
    v.sort_all_objects();
    assert_eq!(
        decode_started_request(&serde_json::to_vec(&v).unwrap())
            .unwrap()
            .digest()
            .unwrap(),
        c.started.digest().unwrap()
    );
}
#[test]
fn lost_first_execution_reply_lookup_needs_only_known_keys_no_live_clock() {
    let c = common::case();
    let f = fixtures();
    let q: HistoryRequest = serde_json::from_value(f["history_request"].clone()).unwrap();
    q.validate().unwrap();
    assert_eq!(q.accepted_receipt_digest, c.accepted.digest().unwrap());
    assert_eq!(q.prepare_operation_id, c.prepare.original.operation_id);
    for key in ["history_known", "history_absent"] {
        let r: HistoryReceipt = serde_json::from_value(f[key].clone()).unwrap();
        r.validate_for(&q).unwrap();
        r.digest_for(&q).unwrap();
        let wire = serde_json::to_vec(&r).unwrap();
        decode_history_receipt(&wire, &q).unwrap();
        assert!(!String::from_utf8(wire).unwrap().contains("remaining_ms"));
    }
    let r: HistoryReceipt = serde_json::from_value(f["history_known"].clone()).unwrap();
    let mut changed = q.clone();
    changed.prepare_operation_id = uuid::Uuid::from_u128(999);
    assert!(r.validate_for(&changed).is_err());
    assert_eq!(
        r.identity.unwrap().prepare_request_digest,
        c.prepare.digest().unwrap()
    );
    let mut v = f["history_absent"].clone();
    v.as_object_mut().unwrap().remove("identity");
    assert!(serde_json::from_value::<HistoryReceipt>(v).is_err());
}
#[test]
fn historical_readback_checks_exact_variant_and_request() {
    let c = common::case();
    let f = fixtures();
    let r: HistoricalReadback = serde_json::from_value(f["historical_readback"].clone()).unwrap();
    let q = TerminalRequest::Started(c.started);
    r.validate_for(&q).unwrap();
    r.digest_for(&q).unwrap();
    assert!(
        r.validate_for(&TerminalRequest::Stopped(c.stopped))
            .is_err()
    );
    decode_historical_readback(&serde_json::to_vec(&r).unwrap(), &q).unwrap();
}
#[test]
fn deterministic_published_schema_manifest_and_fixtures() {
    let actual = serde_json::to_string_pretty(&fixtures()).unwrap() + "\n";
    assert_eq!(actual, include_str!("../artifacts/fixtures.json"));
    let actual = serde_json::to_string_pretty(&schema()).unwrap() + "\n";
    assert_eq!(actual, include_str!("../artifacts/schema.json"));
    let actual = serde_json::to_string_pretty(&contract_manifest().unwrap()).unwrap() + "\n";
    assert_eq!(actual, include_str!("../artifacts/manifest.json"));
}
#[test]
fn published_invalid_documents_are_refused() {
    let c = common::case();
    let f = common::invalid_fixtures();
    for item in f["invalid"].as_array().unwrap() {
        let bytes = serde_json::to_vec(&item["document"]).unwrap();
        let refused = match item["family"].as_str().unwrap() {
            "started_request" => decode_started_request(&bytes)
                .and_then(|q| q.validate_for(&c.context()))
                .is_err(),
            "stopped_request" => decode_stopped_request(&bytes)
                .and_then(|q| q.validate_for(&c.context(), &c.started))
                .is_err(),
            "started_receipt" => decode_started_receipt(&bytes, &c.started).is_err(),
            "history_receipt" => {
                let q: HistoryRequest =
                    serde_json::from_value(fixtures()["history_request"].clone()).unwrap();
                decode_history_receipt(&bytes, &q).is_err()
            }
            _ => panic!("unknown fixture family"),
        };
        assert!(refused, "{}", item["case"]);
    }
    assert_eq!(
        serde_json::to_string_pretty(&f).unwrap() + "\n",
        include_str!("../artifacts/invalid.json")
    );
}
#[test]
fn context_cannot_replace_server_prepared_with_worker_local_commit_or_digest() {
    let c = common::case();
    let mut prepared = c.prepared.clone();
    prepared.revision += 1;
    let mut cx = c.context();
    cx.prepared = &prepared;
    assert!(cx.validate().is_err());
    let mut other = c.prepared.clone();
    other.digest = Digest::of(b"changed");
    let mut cx = c.context();
    cx.prepared = &other;
    assert!(cx.validate().is_err());
}
