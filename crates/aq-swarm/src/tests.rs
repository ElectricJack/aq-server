use crate::capacity::*;
use aq_dashboard_contract::*;

fn node(health: WorkerHealth, total: Option<u32>) -> NodeCapacity {
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
    let n = node(WorkerHealth::Offline, Some(2));
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
    let nodes = [node(WorkerHealth::Online, None)];
    let (s, reason) = slot_summary(&nodes, 0, false, 1_000).unwrap();
    assert_eq!(s.total.value, None);
    assert!(matches!(s.total.accuracy, Accuracy::Unknown));
    assert!(matches!(s.idle.accuracy, Accuracy::Unknown));
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
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].state, AvailabilityState::Unknown);
    assert_eq!(out[0].reason, Some(AvailabilityReason::NoFreshNodeReport));
    assert_eq!(out[0].available_nodes, 0);
}
