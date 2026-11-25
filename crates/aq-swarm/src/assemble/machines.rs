use super::*;
pub(super) fn machine(
    key: &str,
    group: Vec<Built<'_>>,
    control: Option<&ControlFact>,
    now_ms: u64,
) -> Result<SwarmMachine, TimeError> {
    let online: Vec<&Built> = group
        .iter()
        .filter(|b| b.node.health == WorkerHealth::Online)
        .collect();
    let (label, kind) = identify(key, &group);
    let state = machine_state(&online);
    let holding = machine_holds(&online, group.len() > 1);
    let newest = online
        .iter()
        .filter_map(|b| b.facts)
        .max_by_key(|f| f.received_at_ms);
    let load = newest.map(|f| machine_load(f, now_ms)).transpose()?;
    let hardware = newest.map(|f| MachineHardware {
        logical_cpus: f.report.logical_cpus,
        memory_total_bytes: f.report.memory_total_bytes,
        disk_total_bytes: f.report.disk_total_bytes,
        disk_free_bytes: f.report.disk_free_bytes,
    });
    let totals: Option<Vec<u32>> = group.iter().map(|b| b.node.slots.total).collect();
    Ok(SwarmMachine {
        id: key.to_owned(),
        label,
        kind,
        state,
        holding,
        last_seen_at: group
            .iter()
            .filter_map(|b| b.node.last_seen_at.clone())
            .max_by(|a, b| a.0.cmp(&b.0)),
        load,
        hardware,
        agent_max: totals.map(|t| t.iter().fold(0u32, |a, v| a.saturating_add(*v))),
        cost: None,
        control: control
            .map(|c| {
                Ok(MachineControl {
                    desired: c.desired,
                    generation: c.generation.to_string(),
                    since: Some(timestamp(c.since_ms)?),
                    pending_handoffs: c.pending_handoffs,
                })
            })
            .transpose()?,
        nodes: group.into_iter().map(|b| b.node).collect(),
    })
}
fn identify(key: &str, group: &[Built<'_>]) -> (String, MachineKind) {
    let first_facts = group.iter().find_map(|b| b.facts);
    let label = first_facts
        .and_then(|f| f.report.machine.label.clone())
        .unwrap_or_else(|| match key.strip_prefix("node:") {
            Some(_) => group[0].node.name.clone(),
            None => key.to_owned(),
        });
    let kind = first_facts.map_or_else(
        || {
            if group
                .iter()
                .any(|b| b.node.backend_kind.as_deref() == Some("boat"))
            {
                MachineKind::Cloud
            } else {
                MachineKind::Local
            }
        },
        |f| f.report.machine.kind,
    );
    (label, kind)
}
fn machine_state(online: &[&Built<'_>]) -> MachineState {
    if online.is_empty() {
        MachineState::Offline
    } else if online
        .iter()
        .all(|b| b.node.service.state == ServiceState::Stopped)
    {
        MachineState::Stopped
    } else if online.iter().any(|b| !b.node.holding.is_empty()) {
        MachineState::Holding
    } else {
        MachineState::Online
    }
}
fn machine_holds(online: &[&Built<'_>], multiple: bool) -> Vec<HoldReason> {
    let mut holding: Vec<HoldReason> = Vec::new();
    for b in online {
        for h in &b.node.holding {
            if holding.iter().any(|x| x.code == h.code) {
                continue;
            }
            let mut h = h.clone();
            if multiple {
                h.message = format!("{}: {}", b.node.name, h.message);
            }
            holding.push(h);
        }
    }
    holding
}
fn machine_load(f: &MachineFacts, now_ms: u64) -> Result<MachineLoad, TimeError> {
    Ok(MachineLoad {
        source: f.report.source,
        host_probe: f.report.host_probe,
        cpu_busy_bps: f.report.cpu_busy_bps,
        memory_used_bps: f.report.memory_used_bps(),
        cpu_avg_15m_bps: f.cpu_avg_15m_bps,
        memory_avg_15m_bps: f.memory_avg_15m_bps,
        samples_15m: f.samples_15m,
        observed_at: timestamp(f.received_at_ms)?,
        fresh: now_ms.saturating_sub(f.received_at_ms) <= FRESH_MS,
    })
}
