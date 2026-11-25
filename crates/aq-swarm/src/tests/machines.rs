use super::*;
#[test]
fn measurement_unavailable_expired_and_fresh() {
    let (r, q) = (aref("rt"), aref("q"));
    assert_eq!(measurement(&[], None, NOW), None);
    for (sample, expected) in [
        (
            window("other", 1, NOW + 1),
            Some(HoldCode::MeasurementUnavailable),
        ),
        (window("rt", 1, NOW), Some(HoldCode::MeasurementExpired)),
        (window("rt", 1, NOW + 1), None),
    ] {
        assert_eq!(measurement(&[sample], Some((&r, &q)), NOW), expected);
    }
}
#[test]
fn nodes_with_same_key_form_one_machine() {
    let mut i = input(vec![
        node((1, "a"), WorkerHealth::Online, (Some(3), 0), vec![]),
        node((2, "b"), WorkerHealth::Online, (Some(2), 0), vec![]),
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
    let mut boat = node((2, "cloud-1"), WorkerHealth::Online, (Some(1), 0), vec![]);
    boat.backend_kind = Some("boat".into());
    let s = assemble(input(vec![
        node((1, "local-1"), WorkerHealth::Online, (Some(1), 0), vec![]),
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
        (1, "a"),
        WorkerHealth::Offline,
        (Some(2), 1),
        vec![IneligibleReason::WorkerOffline],
    )]);
    i.machines.insert(uid(1), facts("desktop", 100, NOW));
    i.agents.push((
        uid(1),
        agent(uid(5), "claude-opus-5-5", AgentPhase::Running),
    ));
    let s = assemble(i).unwrap();
    let m = &s.machines[0];
    assert_eq!(
        (m.state, m.load.is_none(), m.holding.is_empty()),
        (MachineState::Offline, true, true)
    );
    assert_eq!(m.nodes[0].agents.len(), 1);
    assert_eq!(
        m.nodes[0].agents[0].intelligence_class,
        Some(IntelligenceClass::Deep)
    );
    assert!(m.nodes[0].holding.is_empty());
}
#[test]
fn zero_samples_gives_null_averages() {
    let mut i = input(vec![node(
        (1, "a"),
        WorkerHealth::Online,
        (Some(1), 0),
        vec![],
    )]);
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
        let mut i = input(vec![node(
            (1, "a"),
            WorkerHealth::Online,
            (Some(1), 0),
            vec![],
        )]);
        i.machines.insert(uid(1), facts("desktop", 2350, NOW - age));
        let load = assemble(i).unwrap().machines[0].load.clone().unwrap();
        assert_eq!(load.fresh, fresh);
        assert_eq!(load.cpu_busy_bps, 2350);
        assert_eq!(load.source, LoadSource::Linux);
    }
}
