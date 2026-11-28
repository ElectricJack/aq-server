use crate::capacity::*;
use crate::machine::*;
use crate::service::ControlFact;
use aq_dashboard_contract::*;

const LINUX: &str = include_str!("../../../protocol/worker-machine/v1/fixtures/report-linux.json");
const WINDOWS: &str =
    include_str!("../../../protocol/worker-machine/v1/fixtures/report-windows-host.json");
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
    (n, name): (u128, &str),
    health: WorkerHealth,
    (total, occupied): (Option<u32>, u32),
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
        sandbox_admission: None,
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
fn agent(attempt: Uuid, model: &str, phase: AgentPhase) -> SwarmAgent {
    serde_json::from_value(serde_json::json!({
        "session_id": uid(900), "attempt_id": attempt,
        "task": {"type": "task", "id": uid(903), "title": "Fix the flaky test"},
        "project": {"id": uid(901), "label": "p"},
        "provider": "anthropic", "model": model, "harness": "claude-code",
        "intelligence_class": crate::class::intelligence_class(model),
        "terminal": null, "started_at": "2026-11-07T00:00:00.000Z",
        "phase": phase, "held_since": "2026-11-06T23:59:50.000Z",
        "ran_with": null, "last_output_at": "2026-11-07T00:00:03.000Z"
    }))
    .expect("swarm agent shape")
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
        history: Vec::new(),
        task_holds: Vec::new(),
        launch_overdue: Vec::new(),
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

mod agents;
mod capacity;
mod holds;
mod machines;
mod reports;
mod services;
