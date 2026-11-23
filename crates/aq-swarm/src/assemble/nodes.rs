use super::holds::mapped;
use super::*;
impl SwarmInput {
    pub(super) fn build_node<'a>(
        &'a self,
        n: &NodeCapacity,
        held: &BTreeSet<&str>,
    ) -> Result<Built<'a>, TimeError> {
        let id = n.node.id;
        let facts = self.machines.get(&id);
        let service = self.services.get(&id);
        let extra = self.extras.iter().find(|e| e.id == id);
        let key = extra
            .and_then(|e| e.machine_key.clone())
            .or_else(|| {
                facts
                    .and_then(|f| f.report.machine.key.clone())
                    .filter(|k| !k.is_empty() && !held.contains(k.as_str()))
            })
            .unwrap_or_else(|| format!("node:{id}"));
        let agents = self
            .agents
            .iter()
            .filter(|(node, _)| *node == id)
            .map(|(_, agent)| agent.clone())
            .collect();
        let node = SwarmNode {
            capability_observed_at: n.capability_observed_at.clone(),
            inventory_rejection: None,
            node: n.node.clone(),
            name: n.name.clone(),
            health: n.health,
            service: WorkerService {
                state: service.map_or(ServiceState::Unreported, |s| s.state),
                since: service.map(|s| timestamp(s.since_ms)).transpose()?,
            },
            backend_kind: n.backend_kind.clone(),
            worker_version: extra.map(|e| e.worker_version.clone()).unwrap_or_default(),
            sandbox_templates: extra
                .map(|e| e.sandbox_templates.clone())
                .unwrap_or_default(),
            agent_channel: ReportedState::Unreported,
            last_seen_at: n.last_seen_at.clone(),
            slots: n.slots.clone(),
            holding: node_holds(n, extra, service, &self.history),
            agents,
        };
        Ok(Built { key, facts, node })
    }
}
fn node_holds(
    n: &NodeCapacity,
    extra: Option<&NodeExtra>,
    service: Option<&ServiceFact>,
    history: &[aq_history_caps::Hold],
) -> Vec<HoldReason> {
    let mut codes: Vec<HoldCode> = Vec::new();
    if n.health != WorkerHealth::Online {
        return Vec::new();
    }
    // Live acceptance D7: a service slot reports its capabilities once
    // per attempt, so the report lapses while its agent runs. A busy
    // node is not held for that; it reports again before its next poll.
    let busy = n.slots.occupied > 0;
    let lapsed = |r: &IneligibleReason| busy && *r == IneligibleReason::CapabilityExpired;
    let reasons = n.eligibility.reasons.iter().filter(|r| !lapsed(r));
    codes.extend(reasons.copied().filter_map(mapped));
    codes.extend(extra.and_then(|e| e.measurement));
    match service.map(|s| s.state) {
        Some(ServiceState::Stopped) => codes.push(HoldCode::WorkerStopped),
        Some(ServiceState::Draining) => codes.push(HoldCode::WorkerDraining),
        _ => {}
    }
    let node_history = history
        .iter()
        .filter(|h| h.scope == aq_history_caps::Scope::Node(n.node.id))
        .map(history_reason);
    codes.into_iter().map(hold).chain(node_history).collect()
}
