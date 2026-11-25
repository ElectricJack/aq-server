use super::*;
#[test]
fn holding_drops_concurrency_and_maps_measurement() {
    let mut i = input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 1),
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
        node((1, "local-1"), WorkerHealth::Online, (Some(1), 0), vec![]),
        node(
            (2, "local-2"),
            WorkerHealth::Online,
            (Some(1), 0),
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
    let mut i = input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 0),
        vec![],
    )]);
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
        (1, "a"),
        WorkerHealth::Offline,
        (Some(1), 0),
        vec![IneligibleReason::WorkerOffline],
    )]))
    .unwrap();
    assert_eq!(codes(&s.waiting.reasons), vec![HoldCode::NoMachineOnline]);
    assert!(s.waiting.agents.is_empty());
}
#[test]
fn waiting_no_eligible_machine_names_machines() {
    let mut i = input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 0),
        vec![],
    )]);
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
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 1),
        vec![IneligibleReason::NodeConcurrencyCeilingReached],
    )]))
    .unwrap();
    assert_eq!(codes(&s.waiting.reasons), vec![HoldCode::NoFreeSlot]);
}
#[test]
fn unknown_slot_total_gives_no_free_slot_reason() {
    let s = assemble(input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (None, 2),
        vec![],
    )]))
    .unwrap();
    assert!(s.waiting.reasons.is_empty());
    assert_eq!(s.machines[0].agent_max, None);
}
#[test]
fn busy_node_is_not_held_for_a_lapsed_capability_report() {
    let lapsed = vec![IneligibleReason::CapabilityExpired];
    let mut i = input(vec![
        node((1, "busy"), WorkerHealth::Online, (None, 1), lapsed.clone()),
        node((2, "idle"), WorkerHealth::Online, (None, 0), lapsed),
    ]);
    i.machines.insert(uid(1), facts("desktop", 1, NOW));
    i.machines.insert(uid(2), facts("laptop", 1, NOW));
    let s = assemble(i).unwrap();
    let state = |label: &str| s.machines.iter().find(|m| m.label == label).unwrap();
    for (machine, expected, holds) in [
        ("desktop", MachineState::Online, vec![]),
        (
            "laptop",
            MachineState::Holding,
            vec![HoldCode::CapabilityExpired],
        ),
    ] {
        assert_eq!(
            (state(machine).state, codes(&state(machine).holding)),
            (expected, holds)
        );
    }
}

#[test]
fn history_holds_join_the_node_and_the_waiting_reasons() {
    use aq_history_caps::{Hold, Scope};
    let mut i = input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 0),
        vec![],
    )]);
    i.history = vec![
        Hold {
            kind: "presence_operations".into(),
            scope: Scope::Node(uid(1)),
            bytes: false,
            used: 4096,
            limit: 4096,
        },
        Hold {
            kind: "admission_artifacts".into(),
            scope: Scope::Project(uid(7)),
            bytes: false,
            used: 449,
            limit: 512,
        },
    ];
    let s = assemble(i).unwrap();
    let n = &s.machines[0].nodes[0];
    // The rate limits were removed (2026-11-13): a history hold is always a live cap.
    assert_eq!(
        n.holding
            .iter()
            .map(|h| (h.code, h.message.as_str()))
            .collect::<Vec<_>>(),
        [(
            HoldCode::HistoryCapReached,
            "Live inventory reports cap reached: 4096 of 4096"
        )]
    );
    assert!(matches!(s.machines[0].state, MachineState::Holding));
    assert!(
        s.waiting
            .reasons
            .iter()
            .any(|r| r.code == HoldCode::HistoryCapReached
                && r.message
                    == format!(
                        "Project {}: live admission artifacts cap reached: 449 of 512",
                        uid(7)
                    ))
    );
}

#[test]
fn task_holds_are_counted_per_project_in_the_waiting_reasons() {
    let mut i = input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 0),
        vec![],
    )]);
    assert!(
        !assemble(input(vec![node(
            (1, "a"),
            WorkerHealth::Online,
            (Some(1), 0),
            vec![]
        )]))
        .unwrap()
        .waiting
        .reasons
        .iter()
        .any(|r| r.code == HoldCode::TasksHeld),
        "no hold, no reason"
    );
    i.task_holds = vec![
        (uid(7), "handoff_successor_stuck".into(), 1),
        (
            uid(7),
            "replacement_policy_retry_budget_exhausted".into(),
            2,
        ),
        (uid(8), "lineage_retry_budget_exhausted".into(), 1),
    ];
    let s = assemble(i).unwrap();
    let held: Vec<&HoldReason> = (s.waiting.reasons.iter())
        .filter(|r| r.code == HoldCode::TasksHeld)
        .collect();
    assert_eq!(
        held.iter().map(|h| h.message.as_str()).collect::<Vec<_>>(),
        [
            format!(
                "Project {}: 3 tasks held (handoff_successor_stuck 1, replacement_policy_retry_budget_exhausted 2)",
                uid(7)
            ),
            format!(
                "Project {}: 1 task held (lineage_retry_budget_exhausted 1)",
                uid(8)
            )
        ]
    );
    assert!(held[0].remedy.is_some());
}
