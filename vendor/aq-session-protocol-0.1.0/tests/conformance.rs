#[path = "../examples/export.rs"]
mod exporter;
use aq_session_protocol::*;
use serde_json::Value;
use uuid::Uuid;
fn fixtures() -> Value {
    serde_json::from_str(include_str!("../artifacts/fixtures.json")).unwrap()
}
fn input() -> InputCommand {
    serde_json::from_value(fixtures()["input"].clone()).unwrap()
}
fn snapshot() -> Snapshot {
    serde_json::from_value(fixtures()["snapshot"].clone()).unwrap()
}
fn frame() -> OutputFrame {
    serde_json::from_value(fixtures()["frame"].clone()).unwrap()
}
#[test]
fn canonical_and_schema_drift() {
    let f = fixtures();
    for v in f["valid_documents"].as_array().unwrap() {
        let doc: ContractDocument = decode(&serde_json::to_vec(v).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(doc).unwrap(), *v);
    }
    let offer: ProfiledOffer =
        serde_json::from_value(f["valid_documents"][0]["body"].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(canonical_digest(&offer.manifest).unwrap()).unwrap(),
        f["manifest_digest"]
    );
    for bad in f["invalid_manifests"].as_array().unwrap() {
        assert!(decode::<LaunchManifest>(&serde_json::to_vec(bad).unwrap()).is_err());
    }
    let expected = format!(
        "{}\n",
        serde_json::to_string_pretty(&contract_schema()).unwrap()
    );
    assert_eq!(expected, include_str!("../artifacts/contract.schema.json"));
}
#[test]
fn literal_input_conflicts_and_unknown_never_replays() {
    let c = input();
    let mut state = InputConformance::new(c.lease.terminal.clone(), 1);
    assert_eq!(state.receive(&c, 1100).unwrap(), InputDisposition::Received);
    let mut changed = c.clone();
    changed.bytes.push(10);
    assert_eq!(state.receive(&changed, 1100), Err(ContractError::Conflict));
    state.begin_injection(&c, 1100).unwrap();
    state.finish(c.operation_id, false).unwrap();
    assert_eq!(
        state.receive(&c, 1100).unwrap(),
        InputDisposition::DeliveryUnknown
    );
    let mut later = c.clone();
    later.operation_id = Uuid::from_u128(2);
    assert_eq!(
        state.receive(&later, 1100),
        Err(ContractError::DeliveryUnknown)
    );
    assert!(state.resolve_unknown(&snapshot()).is_err());
    state
        .request_resolution_capture(snapshot().capture_request_id, 1)
        .unwrap();
    state.resolve_unknown(&snapshot()).unwrap();
    assert_eq!(
        state.receive(&later, 1100).unwrap(),
        InputDisposition::Received
    );
    assert_eq!(
        state.receive(&c, 1100).unwrap(),
        InputDisposition::DeliveryUnknown
    );
}
#[test]
fn authority_and_deadline_rechecked_before_effect() {
    let c = input();
    let mut state = InputConformance::new(c.lease.terminal.clone(), 2);
    assert_eq!(state.receive(&c, 1100), Err(ContractError::Stale));
    state.controller_generation = 1;
    assert_eq!(state.receive(&c, 2000), Err(ContractError::Expired));
    state.receive(&c, 1100).unwrap();
    assert_eq!(state.begin_injection(&c, 2000), Err(ContractError::Expired));
    state.controller_generation = 2;
    assert_eq!(state.begin_injection(&c, 1100), Err(ContractError::Stale));
    let mut stale = c.clone();
    stale.lease.terminal.epoch += 1;
    assert_eq!(state.receive(&stale, 1100), Err(ContractError::Conflict));
    stale.operation_id = Uuid::from_u128(3);
    assert_eq!(state.receive(&stale, 1100), Err(ContractError::Stale));
}
#[test]
fn bounded_input_backlog() {
    let mut c = input();
    c.bytes = vec![0; MAX_INPUT_BYTES];
    let mut state = InputConformance::new(c.lease.terminal.clone(), 1);
    for id in 1..=8 {
        c.operation_id = Uuid::from_u128(id);
        state.receive(&c, 1100).unwrap();
    }
    c.operation_id = Uuid::from_u128(9);
    assert!(state.receive(&c, 1100).is_err());
}
#[test]
fn snapshot_boundary_conflicts_gaps_and_backlog() {
    let s = snapshot();
    let mut r = ReplayConformance::from_snapshot(&s).unwrap();
    let f = frame();
    let mut covered = f.clone();
    covered.sequence = 2;
    assert!(!r.apply(&covered).unwrap());
    assert!(r.apply(&f).unwrap());
    assert!(!r.apply(&f).unwrap());
    let mut changed = f.clone();
    changed.bytes = vec![63];
    changed.digest = Digest::of(&changed.bytes);
    assert_eq!(r.apply(&changed), Err(ContractError::Conflict));
    let mut gap = f.clone();
    gap.sequence = 5;
    assert!(r.apply(&gap).is_err());
    let mut stale = f.clone();
    stale.terminal.epoch += 1;
    assert_eq!(r.apply(&stale), Err(ContractError::Stale));
    let mut full = s;
    full.state = vec![0; MAX_SNAPSHOT_BYTES];
    full.digest = Digest::of(&full.state);
    let mut replay = ReplayConformance::from_snapshot(&full).unwrap();
    let mut large = f.clone();
    large.bytes = vec![0; MAX_FRAME_BYTES];
    large.byte_length = MAX_FRAME_BYTES as u32;
    large.digest = Digest::of(&large.bytes);
    for sequence in 3..16 {
        large.sequence = sequence;
        replay.apply(&large).unwrap();
    }
    large.sequence = 16;
    assert!(replay.apply(&large).is_err());
}
#[test]
fn exact_capabilities_and_launch_binding() {
    let f = fixtures();
    let o: ProfiledOffer = serde_json::from_value(f["valid_documents"][0]["body"].clone()).unwrap();
    let mut a = ProfiledAcceptance {
        binding: o.binding.clone(),
        advertised_capabilities: vec![],
    };
    assert_eq!(a.validate_against(&o), Err(ContractError::Unsupported));
    a.advertised_capabilities = o.required_capabilities.clone();
    a.validate_against(&o).unwrap();
    a.advertised_capabilities[0].version = 2;
    assert_eq!(a.validate_against(&o), Err(ContractError::Unsupported));
    a.advertised_capabilities = o.required_capabilities.clone();
    let g = ProfiledGrant {
        binding: o.binding.clone(),
        grant_id: Uuid::from_u128(1),
        expires_at_ms: 2000,
    };
    g.validate_against(&a, 1100).unwrap();
    assert_eq!(g.validate_against(&a, 2000), Err(ContractError::Expired));
    let j = ProfiledJournalRecord {
        binding: o.binding.clone(),
        grant_id: g.grant_id,
    };
    j.validate_against(&g, &a).unwrap();
    a.binding.launch_manifest_digest = Digest::of(b"changed");
    assert_eq!(g.validate_against(&a, 1100), Err(ContractError::Conflict));
}
#[test]
fn central_identity_is_not_a_worker() {
    let id = SessionIdentity::Central {
        authority_id: Uuid::from_u128(2),
        session_id: Uuid::from_u128(3),
        generation: 1,
    };
    id.validate().unwrap();
    let v = serde_json::to_value(id).unwrap();
    assert!(v.get("node_id").is_none());
    let mut c = input();
    c.lease.terminal.session = serde_json::from_value(v).unwrap();
    let b = NodeCommandBatch {
        schema_version: 1,
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(1),
        batch_id: Uuid::from_u128(1),
        cursor: 0,
        commands: vec![TerminalCommand::Input(c)],
    };
    assert!(b.validate().is_err());
}
#[test]
fn unknown_mutation_fields_and_oversized_text_refuse() {
    let mut c = serde_json::to_value(input()).unwrap();
    c["tmux_command"] = Value::String("send-keys".into());
    assert!(serde_json::from_value::<InputCommand>(c).is_err());
    assert!(Text::new("x".repeat(513)).is_err());
}

#[test]
fn fixture_generation_drift() {
    assert_eq!(fixtures(), exporter::fixtures());
}
#[test]
fn nil_rebinding_and_aggregate_context_refuse() {
    let f = fixtures();
    let m: LaunchManifest =
        serde_json::from_value(f["valid_documents"][0]["body"]["manifest"].clone()).unwrap();
    let mut nil = m.clone();
    nil.bindings.project_id = Uuid::nil();
    assert!(nil.validate().is_err());
    let mut over = m.clone();
    over.context.startup_byte_budget = 6;
    assert!(over.validate().is_err());
    let mut o: ProfiledOffer =
        serde_json::from_value(f["valid_documents"][0]["body"].clone()).unwrap();
    o.binding.session = SessionIdentity::Central {
        authority_id: Uuid::from_u128(2),
        session_id: Uuid::from_u128(3),
        generation: 1,
    };
    assert!(o.validate().is_err());
}
#[test]
fn resize_is_fenced_ordered_and_idempotent() {
    let i = input();
    let mut c = ResizeCommand {
        operation_id: i.operation_id,
        lease: i.lease,
        sequence: 1,
        issued_at_ms: 1000,
        deadline_ms: 2000,
        columns: 80,
        rows: 24,
    };
    let mut r = ResizeConformance::new(c.lease.terminal.clone(), 1);
    assert!(r.apply(&c, 1100).unwrap());
    assert!(!r.apply(&c, 1100).unwrap());
    c.columns = 81;
    assert_eq!(r.apply(&c, 1100), Err(ContractError::Conflict));
    c.operation_id = Uuid::from_u128(3);
    c.sequence = 2;
    assert!(r.apply(&c, 1100).unwrap());
    c.sequence = 1;
    assert_eq!(r.apply(&c, 1100), Err(ContractError::Conflict));
    r.controller_generation = 2;
    assert_eq!(r.apply(&c, 1100), Err(ContractError::Stale));
}

#[test]
fn unknown_required_capability_is_not_self_authorizing() {
    let unknown = Capability {
        id: Text::new("unknown.capability").unwrap(),
        version: 1,
    };
    assert_eq!(
        require_capabilities(
            std::slice::from_ref(&unknown),
            std::slice::from_ref(&unknown)
        ),
        Err(ContractError::Unsupported)
    );
    assert!(require_capabilities(&[], &[unknown]).is_ok());
}
#[test]
fn stale_capture_and_controller_cannot_resolve_unknown() {
    let c = input();
    let mut state = InputConformance::new(c.lease.terminal.clone(), 1);
    state.receive(&c, 1100).unwrap();
    state.begin_injection(&c, 1100).unwrap();
    state.finish(c.operation_id, false).unwrap();
    let mut s = snapshot();
    let fresh = Uuid::from_u128(100);
    assert_eq!(
        state.request_resolution_capture(fresh, 2),
        Err(ContractError::Stale)
    );
    state.request_resolution_capture(fresh, 1).unwrap();
    assert!(state.resolve_unknown(&s).is_err());
    s.capture_request_id = fresh;
    state.controller_generation = 2;
    assert!(state.resolve_unknown(&s).is_err());
    state.controller_generation = 1;
    state.resolve_unknown(&s).unwrap();
}
#[test]
fn maximum_snapshot_roundtrips_bounded_wire() {
    let mut s = snapshot();
    s.state = vec![255; MAX_SNAPSHOT_BYTES];
    s.digest = Digest::of(&s.state);
    let bytes = serde_json::to_vec(&s).unwrap();
    assert!(bytes.len() < MAX_BATCH_BYTES);
    assert_eq!(decode::<Snapshot>(&bytes).unwrap(), s);
}
#[test]
fn controller_lease_identity_is_immutable_within_generation() {
    let c = input();
    let mut state = InputConformance::new(c.lease.terminal.clone(), 1);
    state.receive(&c, 1100).unwrap();
    let mut later = c.clone();
    later.operation_id = Uuid::from_u128(2);
    later.lease.lease_id = Uuid::from_u128(3);
    assert_eq!(state.receive(&later, 1100), Err(ContractError::Stale));
    let mut changed = c;
    changed.lease.lease_id = Uuid::from_u128(3);
    assert_eq!(
        state.begin_injection(&changed, 1100),
        Err(ContractError::Stale)
    );
}

#[test]
fn resize_ids_do_not_rebind_after_intervening_command() {
    let i = input();
    let first = ResizeCommand {
        operation_id: i.operation_id,
        lease: i.lease,
        sequence: 1,
        issued_at_ms: 1000,
        deadline_ms: 2000,
        columns: 80,
        rows: 24,
    };
    let mut r = ResizeConformance::new(first.lease.terminal.clone(), 1);
    r.apply(&first, 1100).unwrap();
    let mut next = first.clone();
    next.operation_id = Uuid::from_u128(2);
    next.sequence = 2;
    r.apply(&next, 1100).unwrap();
    let mut changed = first;
    changed.sequence = 3;
    assert_eq!(r.apply(&changed, 1100), Err(ContractError::Conflict));
    next.sequence = 3;
    next.operation_id = Uuid::from_u128(3);
    next.lease.lease_id = Uuid::from_u128(3);
    assert_eq!(r.apply(&next, 1100), Err(ContractError::Stale));
}
#[test]
fn nil_poll_refuses_and_terminal_capability_mandatory() {
    let p = NodeTerminalPoll {
        schema_version: 1,
        node_id: Uuid::nil(),
        boot_id: Uuid::nil(),
        cursor: 0,
        hold_ms: 0,
        active_subscriptions: 0,
    };
    assert!(p.validate().is_err());
    let mut o: ProfiledOffer =
        serde_json::from_value(fixtures()["valid_documents"][0]["body"].clone()).unwrap();
    o.required_capabilities
        .retain(|c| c.id.as_str() != TERMINAL_CAPABILITY);
    assert!(o.validate().is_err());
}
#[test]
fn outbound_batches_deduplicate_and_conflict() {
    let f = fixtures();
    let mut batch: NodeOutputBatch = serde_json::from_value(
        f["valid_documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == "output")
            .unwrap()["body"]
            .clone(),
    )
    .unwrap();
    let mut b = BatchConformance::default();
    assert!(b.output(&batch).unwrap());
    assert!(!b.output(&batch).unwrap());
    batch.cursor += 1;
    assert_eq!(b.output(&batch), Err(ContractError::Conflict));
}
#[test]
fn published_artifact_mirrors_match_when_in_repository() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/session-v1");
    if root.exists() {
        assert_eq!(
            std::fs::read(root.join("fixtures.json")).unwrap(),
            include_bytes!("../artifacts/fixtures.json")
        );
        assert_eq!(
            std::fs::read(root.join("contract.schema.json")).unwrap(),
            include_bytes!("../artifacts/contract.schema.json")
        );
    }
}
#[test]
fn manifest_declares_exactly_one_confinement_mode() {
    let f = fixtures();
    let container: LaunchManifest =
        decode(&serde_json::to_vec(&f["container_manifest"]).unwrap()).unwrap();
    assert!(
        container
            .harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == CONTAINER_CONFINEMENT_CAPABILITY)
    );
    assert!(
        !container
            .harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == NATIVE_CONFINEMENT_CAPABILITY)
    );
    let cap = Capability {
        id: Text::new(CONTAINER_CONFINEMENT_CAPABILITY).unwrap(),
        version: 1,
    };
    assert!(require_capabilities(std::slice::from_ref(&cap), std::slice::from_ref(&cap)).is_ok());
    let invalid = f["invalid_manifests"].as_array().unwrap();
    for name in ["both_confinements", "no_confinement"] {
        let bad = invalid
            .iter()
            .find(|m| m["selection_explanation"]["id"] == name)
            .unwrap_or_else(|| panic!("missing invalid fixture {name}"));
        assert!(decode::<LaunchManifest>(&serde_json::to_vec(bad).unwrap()).is_err());
    }
}
