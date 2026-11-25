use super::*;
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
        node((1, "desk#1"), WorkerHealth::Online, (Some(2), 0), vec![]),
        node((2, "desk#2"), WorkerHealth::Online, (Some(2), 0), vec![]),
        node((3, "desk#3"), WorkerHealth::Online, (Some(2), 0), vec![]),
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
    i.capacity.nodes.push(node(
        (9, "legacy"),
        WorkerHealth::Online,
        (Some(1), 0),
        vec![],
    ));
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
fn observed_service(desired: MachineDesired, state: ServiceState) -> SwarmMachine {
    assemble(service_input(desired, state))
        .unwrap()
        .machines
        .remove(0)
}
#[test]
fn stopped_service_nodes_hold_worker_stopped_and_machine_is_stopped() {
    let m = observed_service(MachineDesired::Stopped, ServiceState::Stopped);
    assert_eq!(m.state, MachineState::Stopped);
    assert!(codes(&m.holding).contains(&HoldCode::WorkerStopped));
}
#[test]
fn draining_service_node_holds_worker_draining() {
    let m = observed_service(MachineDesired::Draining, ServiceState::Draining);
    assert_eq!(m.state, MachineState::Holding);
    assert!(codes(&m.nodes[0].holding).contains(&HoldCode::WorkerDraining));
}
#[test]
fn legacy_machine_has_no_control() {
    let s = assemble(input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 0),
        vec![],
    )]))
    .unwrap();
    assert!(s.machines[0].control.is_none());
}
