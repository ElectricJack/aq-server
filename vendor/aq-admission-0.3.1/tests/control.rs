mod control_common;
use aq_admission::{control::*, transport::*, *};
use control_common::{fixture, startup_common::common::*};
#[test]
fn immutable_six_receipts_have_fresh_nonce_clocks_without_retiming_history() {
    let (offer, accept, plan, prepare, initial, _, _) = fixture();
    plan.validate_for(&accept, id(110)).unwrap();
    initial
        .validate_for(&prepare, &plan.original, &offer, id(114))
        .unwrap();
    let old = serde_json::to_vec(&initial.original).unwrap();
    let mut retry = initial.clone();
    retry.delivery = DeliveryStamp {
        nonce: id(120),
        clock: DeliveryClock::new(1700, 1800).unwrap(),
    };
    retry
        .validate_for(&prepare, &plan.original, &offer, id(120))
        .unwrap();
    assert_eq!(serde_json::to_vec(&retry.original).unwrap(), old);
    assert!(
        retry
            .validate_for(&prepare, &plan.original, &offer, id(114))
            .is_err()
    );
    assert_eq!(
        retry
            .delivery
            .client_deadline_ns(100, 200, Some(50_000_000))
            .unwrap(),
        50_000_000
    );
    assert!(
        retry
            .delivery
            .client_deadline_ns(100, 200, Some(200))
            .is_err()
    );
    assert!(
        retry
            .delivery
            .client_deadline_ns(100, 100_000_101, None)
            .is_err()
    );
}
#[test]
fn renewal_outlives_delivery_lease_only_for_exact_same_grant_owner_and_hard_deadline() {
    let (_, _, _, _, initial, request, renewed) = fixture();
    renewed.validate_for(&request, &initial, id(116)).unwrap();
    assert!(renewed.delivery.clock.expires_at_ms > initial.original.materials[0].expires_at_ms);
    for case in 0..6 {
        let mut changed = renewed.clone();
        match case {
            0 => changed.grant_id = id(200),
            1 => changed.guardian_id = id(200),
            2 => changed.execution_deadline_ms += 1,
            3 => changed.versions.grant_revision = 0,
            4 => changed.journal_revision += 1,
            _ => changed.delivery.clock = DeliveryClock::new(2300, 2500).unwrap(),
        };
        assert!(changed.validate_for(&request, &initial, id(116)).is_err());
    }
    let mut newer = renewed.clone();
    newer.operation_id = id(130);
    newer.journal_revision += 1;
    newer.delivery.nonce = id(131);
    newer.delivery.clock = DeliveryClock::new(1750, 2201).unwrap();
    assert!(newer.validate_after(&renewed).is_err());
    newer.versions.record_revision += 1;
    newer.versions.authority_revision += 1;
    newer.versions.grant_revision += 1;
    newer.validate_after(&renewed).unwrap();
    assert!(renewed.validate_after(&newer).is_err());
}
#[test]
fn native_poll_refuses_scope_duplicate_cancellation_and_requested_limit_overflow() {
    let (offer, accept, _, _, _, _, _) = fixture();
    let request = Poll {
        schema_version: 1,
        operation_id: id(140),
        identity: accept.identity.clone(),
        inventory_sequence: 1,
        available: Resources {
            memory_bytes: 1,
            cpu_millis: 1,
            slots: 1,
            disk_bytes: 1,
        },
        limit: 1,
        after_offer: None,
    };
    let page = NativePollPage {
        schema_version: 1,
        operation_id: request.operation_id,
        identity: request.identity.clone(),
        server_time_ms: offer.offered.clock.server_time_ms,
        offers: vec![offer],
        cancellations: vec![],
        next_after: None,
    };
    page.validate_for(&request).unwrap();
    let mut changed = page.clone();
    changed.offers.push(changed.offers[0].clone());
    assert!(changed.validate_for(&request).is_err());
    changed = page.clone();
    changed.identity.boot_id = id(200);
    assert!(changed.validate_for(&request).is_err());
    changed = page;
    changed.cancellations.push(Cancellation {
        cancellation_id: id(141),
        binding: accept.acceptance.binding,
        authority_revision: 2,
        reason: CancellationReason::OwnerRequest,
    });
    assert!(changed.validate_for(&request).is_err());
}
#[test]
fn reconciliation_is_readonly_complete_scoped_and_cannot_hide_unknown_attempts() {
    let (_, accept, _, _, initial, _, _) = fixture();
    let request = Reconcile {
        schema_version: 1,
        operation_id: id(150),
        identity: accept.identity.clone(),
        after_attempt: None,
        limit: 1,
        attempts: vec![ReconcileAttempt {
            binding: initial.original.grant.binding.clone(),
            guardian_id: accept.guardian_id,
            launch_id: accept.launch_id,
            last_grant_id: Some(initial.original.grant.grant_id),
            journal: JournalEvidence::Recorded {
                revision: 3,
                digest: Digest::of(b"started"),
            },
        }],
    };
    let current = CurrentAuthority {
        binding: request.attempts[0].binding.clone(),
        versions: initial.versions.clone(),
        phase: CurrentPhase::Granted {
            owner: LaunchOwner {
                reservation_id: initial.original.reservation_id,
                guardian_id: initial.original.guardian_id,
                launch_id: initial.original.launch_id,
            },
            grant_id: initial.original.grant.grant_id,
            expires_at_ms: 1800,
            execution_deadline_ms: initial.execution_deadline_ms,
            started_operation_id: Some(id(151)),
        },
    };
    let reply = ReconcilePage {
        schema_version: 1,
        operation_id: request.operation_id,
        identity: request.identity.clone(),
        delivery_nonce: id(152),
        server_time_ms: 1700,
        attempts: vec![ReconcileEntry::Current {
            authority: Box::new(current),
        }],
        next_after: None,
    };
    reply.validate_for(&request, id(152)).unwrap();
    let value = serde_json::to_value(&reply).unwrap();
    assert!(value["attempts"][0]["authority"].get("delivery").is_none());
    let mut changed = reply.clone();
    changed.attempts.clear();
    assert!(changed.validate_for(&request, id(152)).is_err());
    changed = reply.clone();
    changed.delivery_nonce = id(153);
    assert!(changed.validate_for(&request, id(152)).is_err());
    changed = reply.clone();
    changed.attempts[0] = ReconcileEntry::Refused {
        attempt_id: id(200),
        code: FailureCode::UnknownOrNotOwned,
    };
    assert!(changed.validate_for(&request, id(152)).is_err());
    changed = reply.clone();
    changed.attempts[0] = ReconcileEntry::Refused {
        attempt_id: request.attempts[0].binding.attempt_id,
        code: FailureCode::UnknownOrNotOwned,
    };
    changed.validate_for(&request, id(152)).unwrap();
    let mut discover = request;
    discover.attempts.clear();
    assert!(changed.validate_for(&discover, id(152)).is_err());
    changed = reply;
    changed.server_time_ms = 1800;
    assert!(changed.validate_for(&discover, id(152)).is_err());
}
#[test]
fn typed_decode_rejects_nil_versions_scope_unknown_duplicates_and_oversized_bodies() {
    let (_, _, _, _, _, _, renewed) = fixture();
    let reply = ControlReply::Renewed(renewed);
    let raw = serde_json::to_vec(&reply).unwrap();
    decode_reply(&raw).unwrap();
    for case in 0..4 {
        let mut value = serde_json::to_value(&reply).unwrap();
        match case {
            0 => value["body"]["versions"]["record_revision"] = serde_json::json!(0),
            1 => value["body"]["delivery"]["nonce"] = serde_json::json!(id(0)),
            2 => value["body"]["principal_id"] = serde_json::json!(id(9)),
            _ => value["body"]["execution_deadline_ms"] = serde_json::json!(1),
        };
        assert!(decode_reply(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let duplicate = String::from_utf8(raw).unwrap().replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert!(decode_reply(duplicate.as_bytes()).is_err());
    assert!(decode_reply(&vec![b' '; MAX_REPLY_BYTES + 1]).is_err());
}
#[test]
fn mismatched_original_owner_material_and_historical_clocks_never_get_positive_delivery() {
    let (offer, accept, plan, prepare, initial, _, _) = fixture();
    let mut bad = plan.clone();
    bad.original.clock = DeliveryClock::new(1800, 1900).unwrap();
    assert!(bad.validate_for(&accept, id(110)).is_err());
    bad = plan;
    bad.original.reservation.reservation_id = id(200);
    assert!(bad.validate_for(&accept, id(110)).is_err());
    let mut bad = initial.clone();
    bad.original.materials[0].credential_profile = artifact("other-credential");
    assert!(
        bad.validate_for(&prepare, &fixture().2.original, &offer, id(114))
            .is_err()
    );
    bad = initial;
    bad.execution_deadline_ms = u64::MAX;
    assert!(
        bad.validate_for(&prepare, &fixture().2.original, &offer, id(114))
            .is_err()
    );
}
#[test]
fn published_control_fixtures_schema_and_independent_hashes_do_not_drift() {
    let document: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/control.fixture.json")).unwrap();
    assert_eq!(document, control_common::publication());
    assert_eq!(
        reply_schema(),
        serde_json::from_str::<serde_json::Value>(include_str!("../artifacts/control.schema.json"))
            .unwrap()
    );
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/control.python-vectors.json")).unwrap();
    let (offer, accept, preparation, prepare, initial, renew, _) = fixture();
    let poll: Poll = serde_json::from_value(document["requests"]["poll"].clone()).unwrap();
    let reconcile: Reconcile =
        serde_json::from_value(document["requests"]["reconcile"].clone()).unwrap();
    for (index, value) in document["replies"].as_array().unwrap().iter().enumerate() {
        let mut sorted = value.clone();
        sorted.sort_all_objects();
        let bytes = serde_json::to_vec(&sorted).unwrap();
        assert_eq!(
            Digest::of(&bytes).as_str(),
            vectors["reply_sha256"][index].as_str().unwrap()
        );
        let reply = decode_reply(&bytes).unwrap();
        match reply {
            ControlReply::Poll(v) => v.validate_for(&poll).unwrap(),
            ControlReply::Preparation(v) => v.validate_for(&accept, id(110)).unwrap(),
            ControlReply::InitialGrant(v) => v
                .validate_for(&prepare, &preparation.original, &offer, id(114))
                .unwrap(),
            ControlReply::Renewed(v) => v.validate_for(&renew, &initial, id(116)).unwrap(),
            ControlReply::Reconciled(v) => v.validate_for(&reconcile, id(152)).unwrap(),
            ControlReply::Failure(v) => v.validate().unwrap(),
        };
    }
    for (index, key) in [
        (1, "original_preparation_sha256"),
        (2, "original_initial_grant_sha256"),
    ] {
        let mut original = document["replies"][index]["body"]["original"].clone();
        original.sort_all_objects();
        assert_eq!(
            Digest::of(&serde_json::to_vec(&original).unwrap()).as_str(),
            vectors[key].as_str().unwrap()
        );
    }
}
#[test]
fn frozen_six_wire_publication_bytes_match_the_exact_archived_baseline() {
    let inventory: serde_json::Value = serde_json::from_str(include_str!(
        "../artifacts/control.compatibility-sha256.json"
    ))
    .unwrap();
    assert_eq!(
        inventory["baseline_archive_sha256"],
        "d74db09f5766a437b9d43b6d63710a56afc4db69432953ef06699ec7ab09e066"
    );
    for row in inventory["files"].as_array().unwrap() {
        // 0.2.0 moves ownership and removes scheduling from the production API.
        // Preserve every frozen wire artifact; predecessor source-byte guards
        // remain unchanged in vendor/aq-admission-0.1.11.
        if !row["path"].as_str().unwrap().starts_with("artifacts/") {
            continue;
        }
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(row["path"].as_str().unwrap()),
        )
        .unwrap();
        let retained = if row["prefix_only"] == true {
            &bytes[..row["bytes"].as_u64().unwrap() as usize]
        } else {
            &bytes
        };
        assert_eq!(
            Digest::of(retained).as_str(),
            row["sha256"].as_str().unwrap(),
            "{}",
            row["path"]
        );
    }
}
#[test]
fn material_receipt_time_is_server_watermark_not_unqualified_worker_wall_clock() {
    let (_, _, plan, prepare, initial, _, _) = fixture();
    let lease = &plan.original.materials[0];
    let mut receipt = prepare.materials[0].clone();
    receipt.received_at_ms = plan.delivery.clock.server_time_ms;
    receipt.validate_against(lease, 1700).unwrap();
    receipt.received_at_ms = 9_999_999;
    assert!(receipt.validate_against(lease, 1700).is_err());
    let mut reply = initial;
    reply.operation_id = id(200);
    assert!(
        reply
            .validate_for(&prepare, &plan.original, &fixture().0, id(114))
            .is_err()
    );
}

#[test]
fn initial_revision_cannot_change_expiry_and_new_revision_extends_only_live_prior_permit() {
    let (_, _, _, _, initial, request, renewed) = fixture();
    let mut unchanged = renewed.clone();
    unchanged.versions.grant_revision = initial.versions.grant_revision;
    assert!(unchanged.validate_for(&request, &initial, id(116)).is_err());
    unchanged.delivery.clock =
        DeliveryClock::new(1700, initial.original.grant.expires_at_ms).unwrap();
    unchanged.validate_for(&request, &initial, id(116)).unwrap();
    assert!(
        renewed
            .client_renewal_deadline_ns(100, 200, 100_000_000, 900_000_000, 1)
            .unwrap()
            > 100_000_000
    );
    assert_eq!(
        renewed
            .client_renewal_deadline_ns(100, 200, 100_000_000, 900_000_000, 2)
            .unwrap(),
        100_000_000
    );
    assert!(
        renewed
            .client_renewal_deadline_ns(100, 200, 200, 900_000_000, 1)
            .is_err()
    );
    assert!(
        renewed
            .client_renewal_deadline_ns(100, 200, 100_000_000, 900_000_000, 3)
            .is_err()
    );
}
