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

use crate::machine::*;

const LINUX: &str = include_str!("../../../protocol/worker-machine/v1/fixtures/report-linux.json");
const WINDOWS: &str =
    include_str!("../../../protocol/worker-machine/v1/fixtures/report-windows-host.json");

#[test]
fn machine_fixtures_round_trip_and_validate() {
    for text in [LINUX, WINDOWS] {
        let report: MachineReport = serde_json::from_str(text).unwrap();
        assert_eq!(report.validate(), Ok(()));
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }
}
#[test]
fn machine_validate_rejects_each_rule() {
    let base: MachineReport = serde_json::from_str(LINUX).unwrap();
    let case = |edit: &dyn Fn(&mut MachineReport)| {
        let mut r = base.clone();
        edit(&mut r);
        r.validate()
    };
    assert_eq!(case(&|r| r.schema_version = 2), Err("schema_version"));
    assert_eq!(case(&|r| r.machine.key = Some(String::new())), Err("key"));
    assert_eq!(case(&|r| r.machine.key = Some("Upper".into())), Err("key"));
    assert_eq!(case(&|r| r.machine.key = Some("-lead".into())), Err("key"));
    assert_eq!(case(&|r| r.machine.key = Some("a".repeat(64))), Err("key"));
    assert_eq!(
        case(&|r| r.machine.label = Some(String::new())),
        Err("label")
    );
    assert_eq!(
        case(&|r| r.machine.label = Some("a\nb".into())),
        Err("label")
    );
    assert_eq!(
        case(&|r| r.machine.label = Some("a".repeat(65))),
        Err("label")
    );
    assert_eq!(case(&|r| r.logical_cpus = 0), Err("logical_cpus"));
    assert_eq!(case(&|r| r.logical_cpus = 4097), Err("logical_cpus"));
    assert_eq!(case(&|r| r.memory_total_bytes = 0), Err("memory"));
    assert_eq!(
        case(&|r| r.memory_used_bytes = r.memory_total_bytes + 1),
        Err("memory")
    );
    assert_eq!(case(&|r| r.cpu_busy_bps = 10_001), Err("cpu_busy_bps"));
    assert_eq!(case(&|r| r.disk_free_bytes = None), Err("disk"));
    assert_eq!(
        case(&|r| r.disk_free_bytes = Some(r.disk_total_bytes.unwrap() + 1)),
        Err("disk")
    );
}
#[test]
fn memory_used_bps_rounds_down() {
    let mut r: MachineReport = serde_json::from_str(LINUX).unwrap();
    r.memory_total_bytes = 3;
    r.memory_used_bytes = 1;
    assert_eq!(r.memory_used_bps(), 3333);
}
#[test]
fn worker_machine_schema_matches_protocol() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/worker-machine/v1/schema.json"
    );
    let want = serde_json::to_string_pretty(&schemars::schema_for!(MachineReport)).unwrap() + "\n";
    if std::env::var_os("AQ_WRITE_PROTOCOL").is_some() {
        std::fs::write(path, &want).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), want);
}
