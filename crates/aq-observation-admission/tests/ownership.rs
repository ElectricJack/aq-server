use aq_observation_admission::*;
use serde_json::json;
use uuid::Uuid;
use wire::Validate;
fn access() -> Access {
    Access {
        node_id: Uuid::new_v4(),
        boot_id: Uuid::new_v4(),
        accepted_through: 0,
        max_pending_distance: 100,
        new_boot_previously_owned: false,
    }
}
fn prepare(a: &Access) -> Request {
    Request {
        version: VERSION,
        operation_id: Uuid::new_v4(),
        node_id: a.node_id,
        operation: Operation::Prepare {
            boot_id: a.boot_id,
            store_id: Uuid::new_v4(),
        },
    }
}
fn request(a: &Access, operation: Operation) -> Request {
    Request {
        version: VERSION,
        operation_id: Uuid::new_v4(),
        node_id: a.node_id,
        operation,
    }
}
fn allocation(a: &Access) -> (Request, Transition) {
    let r = prepare(a);
    let t = apply(None, None, &r, a, Some(Uuid::new_v4())).unwrap();
    (r, t)
}
fn activate(a: &Access, o: &Owner) -> (Request, Transition) {
    let r = request(
        a,
        Operation::Activate {
            marker: o.marker.clone(),
        },
    );
    let t = apply(Some(o), None, &r, a, None).unwrap();
    (r, t)
}
#[test]
fn lost_prepare_reply_retains_one_owner_and_changes_conflict() {
    let a = access();
    let (r, t) = allocation(&a);
    assert!(t.reply.may_create(&r).unwrap());
    let replay = apply(
        Some(&t.reply.current_owner),
        Some(&t.reply.receipt),
        &r,
        &a,
        None,
    )
    .unwrap();
    assert!(!replay.changed);
    assert_eq!(t.reply, replay.reply);
    let mut changed = r.clone();
    if let Operation::Prepare { store_id, .. } = &mut changed.operation {
        *store_id = Uuid::new_v4();
    }
    assert!(
        apply(
            Some(&t.reply.current_owner),
            Some(&t.reply.receipt),
            &changed,
            &a,
            None
        )
        .is_err()
    );
    assert!(
        apply(
            Some(&t.reply.current_owner),
            None,
            &prepare(&a),
            &a,
            Some(Uuid::new_v4())
        )
        .is_err()
    );
}
#[test]
fn activated_zero_cursor_never_becomes_permission_to_create() {
    let a = access();
    let (p, t) = allocation(&a);
    let (r, active) = activate(&a, &t.reply.current_owner);
    let old = apply(
        Some(&active.reply.current_owner),
        Some(&t.reply.receipt),
        &p,
        &a,
        None,
    )
    .unwrap();
    assert!(!old.reply.may_create(&p).unwrap());
    assert_eq!(old.reply.accepted_through, 0);
    let replay = apply(
        Some(&active.reply.current_owner),
        Some(&active.reply.receipt),
        &r,
        &a,
        None,
    )
    .unwrap();
    assert!(!replay.changed);
    assert_eq!(replay.reply, active.reply);
    assert!(
        apply(
            Some(&active.reply.current_owner),
            None,
            &prepare(&a),
            &a,
            Some(Uuid::new_v4())
        )
        .is_err()
    );
    let new_activation = request(&a, r.operation.clone());
    assert!(
        apply(
            Some(&active.reply.current_owner),
            None,
            &new_activation,
            &a,
            None
        )
        .is_err()
    );
}
#[test]
fn late_prepare_reply_on_replaced_node_boot_cannot_create() {
    let mut a = access();
    let (r, t) = allocation(&a);
    a.boot_id = Uuid::new_v4();
    let replay = apply(
        Some(&t.reply.current_owner),
        Some(&t.reply.receipt),
        &r,
        &a,
        None,
    )
    .unwrap();
    assert!(!replay.reply.may_create(&r).unwrap());
    let activate = request(
        &a,
        Operation::Activate {
            marker: t.reply.current_owner.marker.clone(),
        },
    );
    assert!(apply(Some(&t.reply.current_owner), None, &activate, &a, None).is_err());
}
#[test]
fn pending_old_packets_allow_boot_transition_without_acknowledging_them() {
    let mut a = access();
    let (_, t) = allocation(&a);
    let (_, active) = activate(&a, &t.reply.current_owner);
    let old_boot = a.boot_id;
    a.boot_id = Uuid::new_v4();
    a.accepted_through = 10;
    let r = request(
        &a,
        Operation::AdvanceBoot {
            marker: active.reply.current_owner.marker.clone(),
            expected_boot_id: old_boot,
            expected_revision: active.reply.current_owner.revision,
            new_boot_id: a.boot_id,
            next_sequence: 42,
        },
    );
    let t = apply(Some(&active.reply.current_owner), None, &r, &a, None).unwrap();
    assert_eq!(t.reply.accepted_through, 10);
    assert_eq!(t.reply.current_owner.current_boot_id, a.boot_id);
    for next_sequence in [10, 112, i64::MAX as u64] {
        let mut bad = r.clone();
        if let Operation::AdvanceBoot {
            next_sequence: s, ..
        } = &mut bad.operation
        {
            *s = next_sequence;
        }
        assert!(apply(Some(&active.reply.current_owner), None, &bad, &a, None).is_err());
    }
}
#[test]
fn old_boot_receipt_cannot_roll_back_current_owner() {
    let mut a = access();
    let (_, t) = allocation(&a);
    let (_, active) = activate(&a, &t.reply.current_owner);
    let mut owner = active.reply.current_owner;
    let mut saved = None;
    for n in 0..2 {
        let expected = owner.current_boot_id;
        a.boot_id = Uuid::new_v4();
        let r = request(
            &a,
            Operation::AdvanceBoot {
                marker: owner.marker.clone(),
                expected_boot_id: expected,
                expected_revision: owner.revision,
                new_boot_id: a.boot_id,
                next_sequence: 1,
            },
        );
        let t = apply(Some(&owner), None, &r, &a, None).unwrap();
        owner = t.reply.current_owner.clone();
        if n == 0 {
            saved = Some((r, t.reply.receipt));
        }
    }
    let (r, receipt) = saved.unwrap();
    let replay = apply(Some(&owner), Some(&receipt), &r, &a, None).unwrap();
    assert!(!replay.changed);
    assert_eq!(replay.reply.current_owner, owner);
    assert_ne!(receipt.resulting_owner, owner);
}
#[test]
fn scope_markers_history_and_boot_quota_refuse() {
    let mut a = access();
    let (_, t) = allocation(&a);
    let (mut r, active) = activate(&a, &t.reply.current_owner);
    if let Operation::Activate { marker } = &mut r.operation {
        marker.store_id = Uuid::new_v4();
    }
    assert!(apply(Some(&t.reply.current_owner), None, &r, &a, None).is_err());
    a.node_id = Uuid::new_v4();
    assert!(
        apply(
            Some(&active.reply.current_owner),
            Some(&active.reply.receipt),
            &r,
            &a,
            None
        )
        .is_err()
    );
    let mut a = access();
    a.accepted_through = 1;
    assert!(apply(None, None, &prepare(&a), &a, Some(Uuid::new_v4())).is_err());
    let mut owner = active.reply.current_owner;
    owner.boot_transitions = MAX_BOOT_TRANSITIONS;
    owner.revision = 2 + u64::from(MAX_BOOT_TRANSITIONS);
    owner.current_boot_id = Uuid::new_v4();
    let a = Access {
        node_id: owner.marker.node_id,
        boot_id: Uuid::new_v4(),
        accepted_through: 0,
        max_pending_distance: 10,
        new_boot_previously_owned: false,
    };
    let r = request(
        &a,
        Operation::AdvanceBoot {
            marker: owner.marker.clone(),
            expected_boot_id: owner.current_boot_id,
            expected_revision: owner.revision,
            new_boot_id: a.boot_id,
            next_sequence: 1,
        },
    );
    assert!(apply(Some(&owner), None, &r, &a, None).is_err());
}
#[test]
fn strict_bounded_decoders_and_status_cannot_replace_create_receipt() {
    let a = access();
    let (r, t) = allocation(&a);
    let bytes = serde_json::to_vec(&r).unwrap();
    assert_eq!(decode_request(&bytes).unwrap(), r);
    assert_eq!(
        decode_reply(&serde_json::to_vec(&t.reply).unwrap(), &r).unwrap(),
        t.reply
    );
    let mut v = json!(r);
    v["extra"] = json!(true);
    assert!(decode_request(&serde_json::to_vec(&v).unwrap()).is_err());
    assert!(decode_request(&vec![b' '; MAX_BYTES + 1]).is_err());
    let duplicate = String::from_utf8(bytes)
        .unwrap()
        .replacen("{", "{\"version\":1,", 1);
    assert!(decode_request(duplicate.as_bytes()).is_err());
    let status = Status {
        version: VERSION,
        node_id: a.node_id,
        admitted_node_boot_id: a.boot_id,
        owner: Some(t.reply.current_owner),
        accepted_through: 0,
    };
    status.validate().unwrap();
    let bytes = serde_json::to_vec(&status).unwrap();
    assert_eq!(decode_status(&bytes).unwrap(), status);
    assert!(decode_reply(&bytes, &r).is_err());
}
#[test]
fn equal_revision_requires_identical_owner_and_boot_revision_is_exact() {
    let mut a = access();
    let (_, t) = allocation(&a);
    let (_, active) = activate(&a, &t.reply.current_owner);
    let owner = active.reply.current_owner;
    a.boot_id = Uuid::new_v4();
    let r = request(
        &a,
        Operation::AdvanceBoot {
            marker: owner.marker.clone(),
            expected_boot_id: owner.current_boot_id,
            expected_revision: owner.revision,
            new_boot_id: a.boot_id,
            next_sequence: 1,
        },
    );
    let t = apply(Some(&owner), None, &r, &a, None).unwrap();
    let mut forged = t.reply.clone();
    forged.current_owner.current_boot_id = Uuid::new_v4();
    assert!(forged.validate_against(&r).is_err());
    let mut bad = r.clone();
    if let Operation::AdvanceBoot {
        expected_revision, ..
    } = &mut bad.operation
    {
        *expected_revision += 1;
    }
    assert!(apply(Some(&owner), None, &bad, &a, None).is_err());
    let mut fabricated = t.reply.receipt.clone();
    fabricated.resulting_owner.revision += 1;
    fabricated.resulting_owner.boot_transitions += 1;
    assert!(fabricated.validate_against(&r).is_err());
    a.new_boot_previously_owned = true;
    assert!(apply(Some(&owner), None, &r, &a, None).is_err());
    let replay = apply(
        Some(&t.reply.current_owner),
        Some(&t.reply.receipt),
        &r,
        &a,
        None,
    )
    .unwrap();
    assert!(!replay.changed);
}
#[test]
fn hashes_match_public_canonical_json_and_invalid_receipts_cannot_be_digest_proofs() {
    let a = access();
    let (r, t) = allocation(&a);
    assert_eq!(r.digest().unwrap(), wire::canonical_digest(&r).unwrap());
    assert_eq!(
        t.reply.current_owner.marker.digest().unwrap(),
        wire::canonical_digest(&t.reply.current_owner.marker).unwrap()
    );
    let mut bad = t.reply.receipt.clone();
    bad.version = 42;
    assert!(bad.digest().is_err());
    bad = t.reply.receipt;
    bad.resulting_owner.revision = 999;
    assert!(bad.digest().is_err());
}
#[test]
fn published_schemas_examples_and_python_digest_vectors_match() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/transport.schema.json")).unwrap();
    assert_eq!(schemas(), schema);
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/transport.fixture.json")).unwrap();
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/python-digest-vectors.json")).unwrap();
    let pairs = fixture["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 4);
    for (i, pair) in pairs.iter().enumerate() {
        let r = decode_request(&serde_json::to_vec(&pair["request"]).unwrap()).unwrap();
        let reply = decode_reply(&serde_json::to_vec(&pair["reply"]).unwrap(), &r).unwrap();
        assert_eq!(
            json!(r.digest().unwrap()),
            vectors["vectors"][i]["request_sha256"]
        );
        assert_eq!(
            json!(reply.receipt.digest().unwrap()),
            vectors["vectors"][i]["receipt_sha256"]
        );
        assert_eq!(reply.may_create(&r).unwrap(), i == 0);
    }
    decode_status(&serde_json::to_vec(&fixture["status"]).unwrap()).unwrap();
}
