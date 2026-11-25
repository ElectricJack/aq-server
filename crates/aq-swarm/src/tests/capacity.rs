use super::*;
fn cap_node(health: WorkerHealth, total: Option<u32>) -> NodeCapacity {
    node_capacity(
        NodeInput {
            id: uuid::Uuid::nil(),
            name: "n",
            health,
            last_seen_ms: None,
            backend_kind: None,
            total,
            occupied: 0,
            ceiling: None,
            snapshot: None,
            storage: None,
        },
        1_000,
    )
    .unwrap()
}

#[test]
fn timestamp_formats_millis_utc() {
    let expected = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(1_794_000_000_123)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert_eq!(crate::timestamp(1_794_000_000_123).unwrap().0, expected);
    assert!(expected.ends_with(".123Z"));
}
#[test]
fn node_capacity_offline_unreported_lists_both_reasons() {
    let n = cap_node(WorkerHealth::Offline, Some(2));
    assert_eq!(
        n.eligibility.reasons,
        vec![
            IneligibleReason::WorkerOffline,
            IneligibleReason::CapabilityUnreported
        ]
    );
    assert_eq!(n.slots.idle, Some(2));
}
#[test]
fn slot_summary_unknown_when_an_online_node_has_no_total() {
    let nodes = [cap_node(WorkerHealth::Online, None)];
    let (s, reason) = slot_summary(&nodes, 0, false, 1_000).unwrap();
    assert!(matches!(
        (s.total.value, s.total.accuracy, s.idle.accuracy),
        (None, Accuracy::Unknown, Accuracy::Unknown)
    ));
    assert_eq!(
        reason.as_deref(),
        Some("Some online workers advertise no slot total")
    );
}
#[test]
fn provider_unknown_without_fresh_report() {
    let accounts = vec![AccountInput {
        provider: "p".into(),
        account_reference: "a".into(),
        enabled: true,
        profiles: Vec::new(),
    }];
    let out = provider_availability(accounts, &[], 1_000).unwrap();
    let [account] = out.as_slice() else {
        panic!("expected just the configured account");
    };
    assert_eq!(
        (account.state, account.reason, account.available_nodes),
        (
            AvailabilityState::Unknown,
            Some(AvailabilityReason::NoFreshNodeReport),
            0
        )
    );
}

#[test]
fn class_table_maps_model_families() {
    use IntelligenceClass::*;
    for (model, class) in [
        ("claude-sonnet-5-5", Standard),
        ("claude-opus-5-5", Deep),
        ("claude-haiku-4-5", Fast),
        ("gpt-5.1-codex-mini", Fast),
        ("gpt-5-mini", Fast),
        ("o4-mini", Fast),
        ("gemini-2.5-flash", Fast),
        ("gemini-2.5-pro", Standard),
        ("gpt-5-codex", Standard),
    ] {
        assert_eq!(intelligence_class(model), class, "{model}");
    }
}
#[test]
fn reason_notes_a_cut_candidate_list() {
    assert_eq!(reason(None, false), None);
    assert!(reason(None, true).unwrap().contains("128 most recent"));
}
