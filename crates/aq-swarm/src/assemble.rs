//! Pure swarm assembly: machines, nodes, agents, states and waiting reasons
//! from facts the server has already read.
use crate::class::intelligence_class;
use crate::machine::{FRESH_MS, MachineFacts};
use crate::service::ServiceFact;
use crate::{TimeError, timestamp};
use aq_dashboard_contract::*;
use aq_selection::ArtifactRef;
use std::collections::BTreeMap;
use uuid::Uuid;

/// A measured-context fact for one runtime and qualification.
pub struct MeasuredWindow {
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}

/// Why a natively registered node's measurement does not allow placement.
pub fn measurement(
    facts: &[MeasuredWindow],
    registered: Option<(&ArtifactRef, &ArtifactRef)>,
    now_ms: u64,
) -> Option<HoldCode> {
    let (runtime, qualification) = registered?;
    let matching: Vec<&MeasuredWindow> = facts
        .iter()
        .filter(|f| f.runtime == *runtime && f.qualification == *qualification)
        .collect();
    if matching.is_empty() {
        Some(HoldCode::MeasurementUnavailable)
    } else if matching
        .iter()
        .any(|f| f.observed_at_ms <= now_ms && now_ms < f.expires_at_ms)
    {
        None
    } else {
        Some(HoldCode::MeasurementExpired)
    }
}

/// Per-node facts beyond capacity.
pub struct NodeExtra {
    pub id: Uuid,
    pub worker_version: String,
    pub sandbox_templates: Vec<String>,
    pub measurement: Option<HoldCode>,
}

/// Upper bound of running agents resolved per read.
pub const MAX_AGENTS: usize = 64;

impl NodeExtra {
    /// Version, sandbox templates and measurement state of each node.
    pub async fn load(
        conn: &mut sqlx::PgConnection,
        ids: &[Uuid],
        windows: &[MeasuredWindow],
        now_ms: u64,
    ) -> sqlx::Result<Vec<Self>> {
        use serde_json::Value;
        use sqlx::Row;
        let rows = sqlx::query("SELECT n.id, n.worker_version, b.registration->'backend'->'templates' AS templates, e.state->'native'->'request'->'runtime' AS runtime, e.state->'native'->'qualification' AS qualification FROM nodes n LEFT JOIN delegated_node_boots b ON b.node_id=n.id LEFT JOIN execution_nodes e ON e.node_id=n.id WHERE n.id = ANY($1)")
            .bind(ids)
            .fetch_all(conn)
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let get = |c: &str| {
                    r.get::<Option<Value>, _>(c)
                        .and_then(|v| serde_json::from_value::<ArtifactRef>(v).ok())
                };
                let registered = get("runtime").zip(get("qualification"));
                let templates = r.get::<Option<Value>, _>("templates");
                Self {
                    id: r.get("id"),
                    worker_version: r.get("worker_version"),
                    sandbox_templates: templates
                        .iter()
                        .filter_map(Value::as_array)
                        .flatten()
                        .filter_map(|t| t["digest"].as_str().map(|d| format!("sha256:{d}")))
                        .collect(),
                    measurement: measurement(
                        windows,
                        registered.as_ref().map(|(r, q)| (r, q)),
                        now_ms,
                    ),
                }
            })
            .collect())
    }
}

/// `(session, node)` of task-holding attempts, one more than `MAX_AGENTS`
/// so the caller can tell the read was cut.
pub async fn running(
    conn: &mut sqlx::PgConnection,
    ids: &[Uuid],
) -> sqlx::Result<Vec<(Uuid, Uuid)>> {
    sqlx::query_as("SELECT session_id, node_id FROM execution_attempts WHERE holds_task AND node_id = ANY($1) ORDER BY node_id, session_id LIMIT 65")
        .bind(ids)
        .fetch_all(conn)
        .await
}

pub struct SwarmInput {
    pub now_ms: u64,
    pub server: SwarmServer,
    pub capacity: Capacity,
    pub extras: Vec<NodeExtra>,
    pub machines: BTreeMap<Uuid, MachineFacts>,
    pub services: BTreeMap<Uuid, ServiceFact>,
    pub agents: Vec<(Uuid, SessionSummary)>,
}

/// The reason copy for a code (message, remedy).
pub fn hold(code: HoldCode) -> HoldReason {
    use HoldCode::*;
    let (message, remedy) = match code {
        WorkerOffline => (
            "Worker is offline",
            Some("Start the worker on this machine"),
        ),
        WorkerRevoked => (
            "Worker credential is revoked",
            Some("Enroll the machine again"),
        ),
        WorkerStopped => ("Worker is stopped", Some("Start the worker")),
        WorkerDraining => (
            "Worker is draining",
            Some("Wait for running agents to finish, or start the worker again"),
        ),
        CapabilityUnreported => (
            "Worker has not reported its capabilities",
            Some("Check that the worker runs the current version"),
        ),
        CapabilityExpired => (
            "Capability report expired",
            Some("Wait for the next report; restart the worker if none arrives"),
        ),
        MeasurementExpired => (
            "Context measurement expired",
            Some("Measure this worker's runtime again"),
        ),
        MeasurementUnavailable => (
            "No context measurement for this worker's runtime",
            Some("Measure this worker's runtime"),
        ),
        NodeSpendCeilingReached => (
            "Spend ceiling reached",
            Some("Raise the node's spend ceiling"),
        ),
        NodeSpendUnaccounted => (
            "Spend is not accounted yet",
            Some("Wait for usage accounting to catch up"),
        ),
        NodeDiskPressure => (
            "Disk is above 85% used",
            Some("Free disk space on this machine"),
        ),
        CpuAboveThreshold => ("CPU is above the placement threshold", None),
        MemoryAboveThreshold => ("Memory is above the placement threshold", None),
        AgentMaximumReached => ("Agent maximum reached", None),
        NoMachineOnline => (
            "No machine is online",
            Some("Start a worker on any machine"),
        ),
        NoEligibleMachine => ("No machine can take work", None),
        NoFreeSlot => (
            "Every slot on an eligible machine is busy",
            Some("Wait for a running agent to finish, or add a machine"),
        ),
    };
    HoldReason {
        code,
        message: message.into(),
        remedy: remedy.map(Into::into),
    }
}

fn mapped(reason: IneligibleReason) -> Option<HoldCode> {
    Some(match reason {
        IneligibleReason::WorkerOffline
        | IneligibleReason::WorkerRevoked
        | IneligibleReason::NodeConcurrencyCeilingReached => return None,
        IneligibleReason::CapabilityUnreported => HoldCode::CapabilityUnreported,
        IneligibleReason::CapabilityExpired => HoldCode::CapabilityExpired,
        IneligibleReason::NodeSpendCeilingReached => HoldCode::NodeSpendCeilingReached,
        IneligibleReason::NodeSpendUnaccounted => HoldCode::NodeSpendUnaccounted,
        IneligibleReason::NodeDiskPressure => HoldCode::NodeDiskPressure,
    })
}

struct Built<'a> {
    key: String,
    facts: Option<&'a MachineFacts>,
    node: SwarmNode,
}

pub fn assemble(input: SwarmInput) -> Result<Swarm, TimeError> {
    let SwarmInput {
        now_ms,
        server,
        capacity,
        extras,
        machines,
        services,
        agents,
    } = input;
    let mut built = Vec::new();
    for n in capacity
        .nodes
        .iter()
        .filter(|n| n.health != WorkerHealth::Revoked)
    {
        let id = n.node.id;
        let facts = machines.get(&id);
        let service = services.get(&id);
        let extra = extras.iter().find(|e| e.id == id);
        let key = facts
            .and_then(|f| f.report.machine.key.clone())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| format!("node:{id}"));
        let mut codes: Vec<HoldCode> = Vec::new();
        if n.health == WorkerHealth::Online {
            codes.extend(n.eligibility.reasons.iter().copied().filter_map(mapped));
            codes.extend(extra.and_then(|e| e.measurement));
            match service.map(|s| s.state) {
                Some(ServiceState::Stopped) => codes.push(HoldCode::WorkerStopped),
                Some(ServiceState::Draining) => codes.push(HoldCode::WorkerDraining),
                _ => {}
            }
        }
        let agents = agents
            .iter()
            .filter(|(node, s)| *node == id && s.attempt_id.is_some())
            .map(|(_, s)| SwarmAgent {
                session_id: s.id,
                attempt_id: s.attempt_id.unwrap_or_default(),
                task: s.task.clone(),
                project: s.project.clone(),
                provider: s.profile.provider.clone(),
                model: s.profile.model.clone(),
                harness: s.profile.harness.clone(),
                intelligence_class: s.profile.model.as_deref().and_then(intelligence_class),
                terminal: s.terminal.clone(),
                started_at: s.lifecycle.as_ref().and_then(|l| l.started_at.clone()),
            })
            .collect();
        let node = SwarmNode {
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
            holding: codes.into_iter().map(hold).collect(),
            agents,
        };
        built.push(Built { key, facts, node });
    }

    let mut order: Vec<String> = Vec::new();
    for b in &built {
        if !order.contains(&b.key) {
            order.push(b.key.clone());
        }
    }
    let mut out = Vec::new();
    for key in order {
        let group: Vec<&Built> = built.iter().filter(|b| b.key == key).collect();
        let online: Vec<&Built> = group
            .iter()
            .copied()
            .filter(|b| b.node.health == WorkerHealth::Online)
            .collect();
        let first_facts = group.iter().find_map(|b| b.facts);
        let label = first_facts
            .and_then(|f| f.report.machine.label.clone())
            .unwrap_or_else(|| match key.strip_prefix("node:") {
                Some(_) => group[0].node.name.clone(),
                None => key.clone(),
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
        let state = if online.is_empty() {
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
        };
        let mut holding: Vec<HoldReason> = Vec::new();
        for b in &online {
            for h in &b.node.holding {
                if holding.iter().any(|x| x.code == h.code) {
                    continue;
                }
                let mut h = h.clone();
                if group.len() > 1 {
                    h.message = format!("{}: {}", b.node.name, h.message);
                }
                holding.push(h);
            }
        }
        let newest = online
            .iter()
            .filter_map(|b| b.facts)
            .max_by_key(|f| f.received_at_ms);
        let load = match (state, newest) {
            (MachineState::Offline, _) | (_, None) => None,
            (_, Some(f)) => Some(MachineLoad {
                source: f.report.source,
                host_probe: f.report.host_probe,
                cpu_busy_bps: f.report.cpu_busy_bps,
                memory_used_bps: f.report.memory_used_bps(),
                cpu_avg_15m_bps: f.cpu_avg_15m_bps,
                memory_avg_15m_bps: f.memory_avg_15m_bps,
                samples_15m: f.samples_15m,
                observed_at: timestamp(f.received_at_ms)?,
                fresh: now_ms.saturating_sub(f.received_at_ms) <= FRESH_MS,
            }),
        };
        let hardware = newest.map(|f| MachineHardware {
            logical_cpus: f.report.logical_cpus,
            memory_total_bytes: f.report.memory_total_bytes,
            disk_total_bytes: f.report.disk_total_bytes,
            disk_free_bytes: f.report.disk_free_bytes,
        });
        let totals: Option<Vec<u32>> = group.iter().map(|b| b.node.slots.total).collect();
        out.push(SwarmMachine {
            id: key.clone(),
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
            nodes: group.iter().map(|b| b.node.clone()).collect(),
        });
    }
    out.sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));

    let reasons = waiting(&out);
    Ok(Swarm {
        server,
        machines: out,
        waiting: SwarmWaiting {
            reasons,
            agents: Vec::new(),
        },
    })
}

fn waiting(machines: &[SwarmMachine]) -> Vec<HoldReason> {
    let live = |m: &&SwarmMachine| {
        matches!(
            m.state,
            MachineState::Online | MachineState::Holding | MachineState::Stopped
        )
    };
    if !machines.iter().any(|m| live(&m)) {
        return vec![hold(HoldCode::NoMachineOnline)];
    }
    let online_nodes = |m: &SwarmMachine| -> Vec<SwarmNode> {
        m.nodes
            .iter()
            .filter(|n| n.health == WorkerHealth::Online)
            .cloned()
            .collect()
    };
    let eligible: Vec<SwarmNode> = machines
        .iter()
        .flat_map(online_nodes)
        .filter(|n| n.holding.is_empty() && n.service.state != ServiceState::Stopped)
        .collect();
    if eligible.is_empty() {
        let firsts: Vec<(&str, &HoldReason)> = machines
            .iter()
            .filter(|m| m.nodes.iter().any(|n| n.health == WorkerHealth::Online))
            .filter_map(|m| m.holding.first().map(|h| (m.label.as_str(), h)))
            .collect();
        let detail: Vec<String> = firsts
            .iter()
            .map(|(label, h)| format!("{label}: {}", h.message))
            .collect();
        let mut reason = hold(HoldCode::NoEligibleMachine);
        reason.message = format!("No machine can take work: {}", detail.join("; "));
        reason.remedy = firsts.first().and_then(|(_, h)| h.remedy.clone());
        return vec![reason];
    }
    if eligible.iter().all(|n| n.slots.idle == Some(0)) {
        return vec![hold(HoldCode::NoFreeSlot)];
    }
    Vec::new()
}
