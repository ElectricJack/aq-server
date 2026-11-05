mod common;
use aq_admission::{Digest, transport::*};
use common::*;

fn stopped() -> WorkerStopEvidence {
    WorkerStopEvidence {
        schema_version: 1,
        binding: binding(),
        guardian_id: id(70),
        launch_id: id(71),
        stop_operation_id: id(72),
        journal: JournalEvidence::Recorded {
            revision: 2,
            digest: Digest::of(b"journal-2"),
        },
        disposition: StopDisposition::Stopped {
            grant_id: id(73),
            stop_receipt_digest: Digest::of(b"stop"),
        },
        reason: StopReason::ExecutionExpiry,
    }
}
#[test]
fn roundtrip_budget_is_charged_and_replay_cannot_restart_ttl() {
    let clock = DeliveryClock::new(1000, 2000).unwrap();
    assert_eq!(clock.client_deadline_ns(10, 20).unwrap(), 1_000_000_010);
    assert!(clock.client_deadline_ns(10, 1_000_000_010).is_err());
    assert!(clock.client_deadline_ns(20, 10).is_err());
    assert!(clock.client_deadline_ns(u64::MAX - 1, u64::MAX).is_err());
    let replay = DeliveryClock::new(1900, 2000).unwrap();
    assert_eq!(replay.valid_for_ms, 100);
    assert!(DeliveryClock::new(2000, 2000).is_err());
    assert!(DeliveryClock::new(1, 120002).is_err());
}
#[test]
fn stop_journal_requires_durable_closed_reservation_and_unknown_never_releases() {
    let mut evidence = stopped();
    evidence.disposition = StopDisposition::Unknown { grant_id: None };
    evidence.journal = JournalEvidence::Unavailable {
        reason: JournalUnavailable::Missing,
    };
    evidence.validate().unwrap();
    assert!(!evidence.is_release_candidate());
    evidence.disposition = StopDisposition::NotStarted {
        reservation: NoLaunchReservation {
            reservation_id: id(75),
            reservation_operation_id: id(76),
            reservation_journal_digest: Digest::of(b"closed-slot"),
        },
        grant_id: None,
    };
    assert!(evidence.validate().is_err());
    evidence.journal = stopped().journal;
    assert!(evidence.is_release_candidate());
    evidence.guardian_id = id(0);
    assert!(!evidence.is_release_candidate());
}
#[test]
fn exact_stop_replay_is_immutable_and_journal_order_is_launch_scoped() {
    let mut ledger = StopJournalLedger::default();
    let evidence = stopped();
    assert_eq!(ledger.admit(&evidence).unwrap(), JournalAdmission::Recorded);
    assert_eq!(ledger.admit(&evidence).unwrap(), JournalAdmission::Replay);
    let mut changed = evidence.clone();
    changed.journal = JournalEvidence::Recorded {
        revision: 3,
        digest: Digest::of(b"journal-3"),
    };
    assert!(ledger.admit(&changed).is_err()); // Higher revision cannot rewrite original stop op.
    changed.stop_operation_id = id(80);
    ledger.admit(&changed).unwrap();
    let mut older = evidence.clone();
    older.stop_operation_id = id(81);
    assert!(ledger.admit(&older).is_err());
    older.launch_id = id(82);
    ledger.admit(&older).unwrap(); // Independent launch has independent journal ordering.
}

#[test]
fn published_transport_replays_and_mutations_reject_duplicate_unknown_and_scope() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/transport.schema.json")).unwrap();
    assert_eq!(transport_schema(), schema);
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../artifacts/transport.fixtures.json")).unwrap();
    for value in fixtures {
        let document = decode_transport(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(document).unwrap(), value);
        let mut unknown = value.clone();
        unknown["body"]["chosen_principal"] = serde_json::json!(id(90));
        assert!(decode_transport(&serde_json::to_vec(&unknown).unwrap()).is_err());
    }
    let value = serde_json::to_value(TransportDocument::StopEvidence(stopped())).unwrap();
    let raw = serde_json::to_string(&value).unwrap();
    let duplicate = raw.replace(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
    );
    assert_ne!(raw, duplicate);
    assert!(decode_transport(duplicate.as_bytes()).is_err());
    let mut foreign = stopped();
    foreign.binding.attempt_id = id(999);
    assert!(foreign.validate().is_err());
}
