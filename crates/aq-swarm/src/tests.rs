use crate::capacity::*;
use crate::service::ControlFact;
use aq_dashboard_contract::*;

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

use crate::assemble::*;
use crate::class::intelligence_class;
use crate::service::ServiceFact;
use std::collections::BTreeMap;
use uuid::Uuid;

const NOW: u64 = 1_794_100_000_000;

fn uid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn aref(id: &str) -> aq_selection::ArtifactRef {
    serde_json::from_value(serde_json::json!({"id": id, "version": "1", "digest": "a".repeat(64)}))
        .unwrap()
}
fn window(runtime: &str, observed: u64, expires: u64) -> MeasuredWindow {
    MeasuredWindow {
        runtime: aref(runtime),
        qualification: aref("q"),
        observed_at_ms: observed,
        expires_at_ms: expires,
    }
}
/// One capacity node. `reasons` are the eligibility reasons capacity gave.
fn node(
    n: u128,
    name: &str,
    health: WorkerHealth,
    total: Option<u32>,
    occupied: u32,
    reasons: Vec<IneligibleReason>,
) -> NodeCapacity {
    NodeCapacity {
        node: EntityRef {
            kind: "worker".into(),
            id: uid(n),
            revision: None,
        },
        name: name.into(),
        health,
        last_seen_at: Some(crate::timestamp(NOW - 5_000 * n as u64).unwrap()),
        backend_kind: None,
        slots: NodeSlots {
            total,
            occupied,
            idle: total.map(|t| t.saturating_sub(occupied)),
        },
        eligibility: Eligibility {
            state: if reasons.is_empty() {
                EligibilityState::Eligible
            } else {
                EligibilityState::Ineligible
            },
            reasons,
        },
        capability_observed_at: None,
        capability_fresh_until: None,
        storage: None,
    }
}
fn facts(key: &str, cpu: u16, received_at_ms: u64) -> MachineFacts {
    let mut report: MachineReport = serde_json::from_str(LINUX).unwrap();
    report.machine.key = Some(key.into());
    report.machine.label = Some(key.into());
    report.cpu_busy_bps = cpu;
    MachineFacts {
        report,
        received_at_ms,
        cpu_avg_15m_bps: Some(cpu),
        memory_avg_15m_bps: Some(4000),
        samples_15m: 3,
    }
}
fn session(attempt: Option<Uuid>, model: &str) -> SessionSummary {
    serde_json::from_value(serde_json::json!({
        "id": uid(900), "label": "s", "task": null, "attempt_id": attempt,
        "project": {"id": uid(901), "label": "p"},
        "profile": {"provider": "anthropic", "model": model, "harness": "claude-code"},
        "authority": "a",
        "observation": {"state": "running", "observed_at": "2026-11-07T00:00:00.000Z",
                        "fresh_until": "2026-11-07T00:00:30.000Z"},
        "wait": null,
        "controller": {"state": "none", "label": null},
        "terminal": null,
        "session_identity": {"kind": "central", "authority_id": uid(902), "session_id": uid(900), "generation": "1"},
        "record_revision": null, "execution_authority": "e", "source_kind": "k",
        "lifecycle": {"started_at": "2026-11-07T00:00:00.000Z", "stopped_at": null}
    }))
    .expect("session summary shape")
}
fn input(nodes: Vec<NodeCapacity>) -> SwarmInput {
    let (slots, _) = crate::capacity::slot_summary(&nodes, 0, false, NOW).unwrap();
    SwarmInput {
        now_ms: NOW,
        server: SwarmServer {
            version: "0.1.0".into(),
            source_revision: "r".into(),
            running_agents: 0,
            owner_availability: OwnerAvailability {
                state: OwnerAvailabilityState::Unset,
                generation: "1".into(),
                since: None,
                note: None,
            },
        },
        capacity: Capacity {
            slots,
            nodes,
            providers: Vec::new(),
        },
        extras: Vec::new(),
        machines: BTreeMap::new(),
        services: BTreeMap::new(),
        controls: BTreeMap::new(),
        agents: Vec::new(),
    }
}
fn extra(n: u128, measurement: Option<HoldCode>) -> NodeExtra {
    NodeExtra {
        id: uid(n),
        worker_version: "0.5.1".into(),
        sandbox_templates: Vec::new(),
        measurement,
        machine_key: None,
    }
}
fn codes(h: &[HoldReason]) -> Vec<HoldCode> {
    h.iter().map(|r| r.code).collect()
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
fn measurement_unavailable_expired_and_fresh() {
    let (r, q) = (aref("rt"), aref("q"));
    assert_eq!(measurement(&[], None, NOW), None);
    assert_eq!(
        measurement(&[window("other", 1, NOW + 1)], Some((&r, &q)), NOW),
        Some(HoldCode::MeasurementUnavailable)
    );
    assert_eq!(
        measurement(&[window("rt", 1, NOW)], Some((&r, &q)), NOW),
        Some(HoldCode::MeasurementExpired)
    );
    assert_eq!(
        measurement(&[window("rt", 1, NOW + 1)], Some((&r, &q)), NOW),
        None
    );
}
#[test]
fn nodes_with_same_key_form_one_machine() {
    let mut i = input(vec![
        node(1, "a", WorkerHealth::Online, Some(3), 0, vec![]),
        node(2, "b", WorkerHealth::Online, Some(2), 0, vec![]),
    ]);
    i.machines.insert(uid(1), facts("desktop", 100, NOW));
    i.machines.insert(uid(2), facts("desktop", 100, NOW));
    let s = assemble(i).unwrap();
    assert_eq!(s.machines.len(), 1);
    assert_eq!(s.machines[0].nodes.len(), 2);
    assert_eq!(s.machines[0].agent_max, Some(5));
}
#[test]
fn unreported_node_is_its_own_machine() {
    let mut boat = node(2, "cloud-1", WorkerHealth::Online, Some(1), 0, vec![]);
    boat.backend_kind = Some("boat".into());
    let s = assemble(input(vec![
        node(1, "local-1", WorkerHealth::Online, Some(1), 0, vec![]),
        boat,
    ]))
    .unwrap();
    let local = s.machines.iter().find(|m| m.label == "local-1").unwrap();
    assert_eq!(local.id, format!("node:{}", uid(1)));
    assert_eq!(local.kind, MachineKind::Local);
    assert!(local.load.is_none());
    let cloud = s.machines.iter().find(|m| m.label == "cloud-1").unwrap();
    assert_eq!(cloud.kind, MachineKind::Cloud);
}
#[test]
fn offline_machine_omits_load_and_keeps_agents() {
    let mut i = input(vec![node(
        1,
        "a",
        WorkerHealth::Offline,
        Some(2),
        1,
        vec![IneligibleReason::WorkerOffline],
    )]);
    i.machines.insert(uid(1), facts("desktop", 100, NOW));
    i.agents
        .push((uid(1), session(Some(uid(5)), "claude-opus-5-5")));
    let s = assemble(i).unwrap();
    let m = &s.machines[0];
    assert_eq!(m.state, MachineState::Offline);
    assert!(m.load.is_none());
    assert!(m.holding.is_empty());
    assert_eq!(m.nodes[0].agents.len(), 1);
    assert_eq!(
        m.nodes[0].agents[0].intelligence_class,
        Some(IntelligenceClass::Deep)
    );
    assert!(m.nodes[0].holding.is_empty());
}
#[test]
fn agent_without_attempt_is_skipped() {
    let mut i = input(vec![node(1, "a", WorkerHealth::Online, Some(2), 0, vec![])]);
    i.agents.push((uid(1), session(None, "claude-opus-5-5")));
    assert!(assemble(i).unwrap().machines[0].nodes[0].agents.is_empty());
}
#[test]
fn holding_drops_concurrency_and_maps_measurement() {
    let mut i = input(vec![node(
        1,
        "a",
        WorkerHealth::Online,
        Some(1),
        1,
        vec![IneligibleReason::NodeConcurrencyCeilingReached],
    )]);
    i.extras.push(extra(1, Some(HoldCode::MeasurementExpired)));
    let m = &assemble(i).unwrap().machines[0];
    assert_eq!(m.state, MachineState::Holding);
    assert_eq!(codes(&m.holding), vec![HoldCode::MeasurementExpired]);
    assert_eq!(m.holding[0].message, "Context measurement expired");
}
#[test]
fn multi_node_holding_prefixes_node_name() {
    let mut i = input(vec![
        node(1, "local-1", WorkerHealth::Online, Some(1), 0, vec![]),
        node(
            2,
            "local-2",
            WorkerHealth::Online,
            Some(1),
            0,
            vec![IneligibleReason::CapabilityExpired],
        ),
    ]);
    i.machines.insert(uid(1), facts("desktop", 1, NOW));
    i.machines.insert(uid(2), facts("desktop", 1, NOW));
    let m = &assemble(i).unwrap().machines[0];
    assert!(m.holding[0].message.starts_with("local-2: "));
}
#[test]
fn stopped_when_every_online_node_stopped() {
    let mut i = input(vec![node(1, "a", WorkerHealth::Online, Some(1), 0, vec![])]);
    i.services.insert(
        uid(1),
        ServiceFact {
            state: ServiceState::Stopped,
            since_ms: NOW - 1000,
        },
    );
    let s = assemble(i).unwrap();
    assert_eq!(s.machines[0].state, MachineState::Stopped);
    assert!(codes(&s.machines[0].holding).contains(&HoldCode::WorkerStopped));
    assert_eq!(s.machines[0].nodes[0].service.state, ServiceState::Stopped);
}
#[test]
fn waiting_no_machine_online() {
    let s = assemble(input(vec![node(
        1,
        "a",
        WorkerHealth::Offline,
        Some(1),
        0,
        vec![IneligibleReason::WorkerOffline],
    )]))
    .unwrap();
    assert_eq!(codes(&s.waiting.reasons), vec![HoldCode::NoMachineOnline]);
    assert!(s.waiting.agents.is_empty());
}
#[test]
fn waiting_no_eligible_machine_names_machines() {
    let mut i = input(vec![node(1, "a", WorkerHealth::Online, Some(1), 0, vec![])]);
    i.machines.insert(uid(1), facts("desktop", 1, NOW));
    i.extras.push(extra(1, Some(HoldCode::MeasurementExpired)));
    let s = assemble(i).unwrap();
    assert_eq!(codes(&s.waiting.reasons), vec![HoldCode::NoEligibleMachine]);
    assert_eq!(
        s.waiting.reasons[0].message,
        "No machine can take work: desktop: Context measurement expired"
    );
    assert_eq!(
        s.waiting.reasons[0].remedy.as_deref(),
        Some("Measure this worker's runtime again")
    );
}
#[test]
fn waiting_no_free_slot() {
    let s = assemble(input(vec![node(
        1,
        "a",
        WorkerHealth::Online,
        Some(1),
        1,
        vec![IneligibleReason::NodeConcurrencyCeilingReached],
    )]))
    .unwrap();
    assert_eq!(codes(&s.waiting.reasons), vec![HoldCode::NoFreeSlot]);
}
#[test]
fn unknown_slot_total_gives_no_free_slot_reason() {
    let s = assemble(input(vec![node(
        1,
        "a",
        WorkerHealth::Online,
        None,
        2,
        vec![],
    )]))
    .unwrap();
    assert!(s.waiting.reasons.is_empty());
    assert_eq!(s.machines[0].agent_max, None);
}
#[test]
fn zero_samples_gives_null_averages() {
    let mut i = input(vec![node(1, "a", WorkerHealth::Online, Some(1), 0, vec![])]);
    let mut f = facts("desktop", 2350, NOW);
    f.cpu_avg_15m_bps = None;
    f.memory_avg_15m_bps = None;
    f.samples_15m = 0;
    i.machines.insert(uid(1), f);
    let load = assemble(i).unwrap().machines[0].load.clone().unwrap();
    assert_eq!(load.cpu_avg_15m_bps, None);
    assert_eq!(load.memory_avg_15m_bps, None);
    assert_eq!(load.samples_15m, 0);
    let json = serde_json::to_value(&load).unwrap();
    assert!(json["cpu_avg_15m_bps"].is_null());
}
#[test]
fn load_freshness_boundary() {
    for (age, fresh) in [(60_000, true), (60_001, false)] {
        let mut i = input(vec![node(1, "a", WorkerHealth::Online, Some(1), 0, vec![])]);
        i.machines.insert(uid(1), facts("desktop", 2350, NOW - age));
        let load = assemble(i).unwrap().machines[0].load.clone().unwrap();
        assert_eq!(load.fresh, fresh);
        assert_eq!(load.cpu_busy_bps, 2350);
        assert_eq!(load.source, LoadSource::Linux);
    }
}

fn keyed(n: u128, key: &str) -> NodeExtra {
    NodeExtra {
        machine_key: Some(key.into()),
        ..extra(n, None)
    }
}
fn control(desired: MachineDesired) -> ControlFact {
    ControlFact {
        desired,
        generation: 1,
        since_ms: NOW - 1000,
        pending_handoffs: 0,
    }
}
fn service_input(desired: MachineDesired, state: ServiceState) -> SwarmInput {
    let mut i = input(vec![
        node(1, "desk#1", WorkerHealth::Online, Some(2), 0, vec![]),
        node(2, "desk#2", WorkerHealth::Online, Some(2), 0, vec![]),
        node(3, "desk#3", WorkerHealth::Online, Some(2), 0, vec![]),
    ]);
    for n in 1..=3 {
        i.extras.push(keyed(n, "desk"));
        i.services.insert(
            uid(n),
            ServiceFact {
                state,
                since_ms: NOW - 1000,
            },
        );
    }
    i.machines.insert(uid(1), facts("desk", 100, NOW));
    i.controls.insert("desk".into(), control(desired));
    i
}
#[test]
fn service_machine_groups_by_server_key_and_carries_control() {
    let s = assemble(service_input(
        MachineDesired::Running,
        ServiceState::Running,
    ))
    .unwrap();
    assert_eq!(s.machines.len(), 1);
    let m = &s.machines[0];
    assert_eq!((m.id.as_str(), m.nodes.len()), ("desk", 3));
    let c = m.control.as_ref().unwrap();
    assert_eq!(
        (c.desired, c.generation.as_str(), c.pending_handoffs),
        (MachineDesired::Running, "1", 0)
    );
}
#[test]
fn reported_key_held_by_a_service_machine_is_not_merged() {
    let mut i = service_input(MachineDesired::Running, ServiceState::Running);
    i.capacity
        .nodes
        .push(node(9, "legacy", WorkerHealth::Online, Some(1), 0, vec![]));
    i.machines.insert(uid(9), facts("desk", 100, NOW));
    let s = assemble(i).unwrap();
    let legacy = s
        .machines
        .iter()
        .find(|m| m.id == format!("node:{}", uid(9)))
        .unwrap();
    assert!(legacy.control.is_none());
    assert_eq!(
        s.machines
            .iter()
            .find(|m| m.id == "desk")
            .unwrap()
            .nodes
            .len(),
        3
    );
}
#[test]
fn stopped_service_nodes_hold_worker_stopped_and_machine_is_stopped() {
    let s = assemble(service_input(
        MachineDesired::Stopped,
        ServiceState::Stopped,
    ))
    .unwrap();
    let m = &s.machines[0];
    assert_eq!(m.state, MachineState::Stopped);
    assert!(codes(&m.holding).contains(&HoldCode::WorkerStopped));
}
#[test]
fn draining_service_node_holds_worker_draining() {
    let s = assemble(service_input(
        MachineDesired::Draining,
        ServiceState::Draining,
    ))
    .unwrap();
    let m = &s.machines[0];
    assert_eq!(m.state, MachineState::Holding);
    assert!(codes(&m.nodes[0].holding).contains(&HoldCode::WorkerDraining));
}
#[test]
fn legacy_machine_has_no_control() {
    let s = assemble(input(vec![node(
        1,
        "a",
        WorkerHealth::Online,
        Some(1),
        0,
        vec![],
    )]))
    .unwrap();
    assert!(s.machines[0].control.is_none());
}
#[test]
fn reason_notes_a_cut_candidate_list() {
    assert_eq!(reason(None, 3, false), None);
    assert!(reason(None, 3, true).unwrap().contains("128 most recent"));
}
