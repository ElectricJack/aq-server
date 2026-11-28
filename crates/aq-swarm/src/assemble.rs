//! Pure swarm assembly: machines, nodes, agents, states and waiting reasons
//! from facts the server has already read.
use crate::machine::{FRESH_MS, MachineFacts};
use crate::service::{ControlFact, ServiceFact};
use crate::{TimeError, timestamp};
use aq_dashboard_contract::*;
use aq_selection::ArtifactRef;
use std::collections::{BTreeMap, BTreeSet};
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
    /// The server-held machine key (`nodes.machine_key`) of a service node.
    pub machine_key: Option<String>,
}

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
        let key = if aq_machine_control::has_format(&mut *conn)
            .await
            .map_err(|_| sqlx::Error::Protocol("machine control".into()))?
        {
            "n.machine_key"
        } else {
            "NULL::text"
        };
        let rows = sqlx::query(&format!("SELECT n.id, n.worker_version, {key} AS machine_key, b.registration->'backend'->'templates' AS templates, e.state->'native'->'request'->'runtime' AS runtime, e.state->'native'->'qualification' AS qualification FROM nodes n LEFT JOIN delegated_node_boots b ON b.node_id=n.id LEFT JOIN execution_nodes e ON e.node_id=n.id WHERE n.id = ANY($1)"))
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
                    machine_key: r.get("machine_key"),
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

/// Slot-holding attempts listed per swarm poll.
pub const MAX_CANDIDATES: usize = 128;

/// The read's coverage reason: the capacity gap, and a note when more attempts
/// hold a slot than the read lists (the running count is then a lower bound).
pub fn reason(capacity: Option<String>, cut: bool) -> Option<String> {
    let counted = cut.then(|| {
        format!("Agents are listed and counted from the {MAX_CANDIDATES} most recent attempts")
    });
    let all = capacity
        .into_iter()
        .chain(counted)
        .collect::<Vec<String>>()
        .join("; ");
    Some(all).filter(|r| !r.is_empty())
}

pub struct SwarmInput {
    pub now_ms: u64,
    pub server: SwarmServer,
    pub capacity: Capacity,
    pub extras: Vec<NodeExtra>,
    pub machines: BTreeMap<Uuid, MachineFacts>,
    pub services: BTreeMap<Uuid, ServiceFact>,
    pub controls: BTreeMap<String, ControlFact>,
    /// `(node, row)` of every attempt holding a slot (`agents::load`).
    pub agents: Vec<(Uuid, SwarmAgent)>,
    pub history: Vec<aq_history_caps::Hold>,
    /// R6: current task holds per (project, reason code), `aq_machine_control::holds::counts`.
    pub task_holds: Vec<(Uuid, String, i64)>,
    /// Granted attempts on the page's nodes whose agent did not start within the launch
    /// deadline (`aq_machine_control::lapse::overdue_on`).
    pub launch_overdue: Vec<aq_machine_control::lapse::Overdue>,
}

mod holds;
mod machines;
mod nodes;
pub use holds::{history_reason, hold, launch_overdue_reasons, task_hold_reasons};

struct Built<'a> {
    key: String,
    facts: Option<&'a MachineFacts>,
    node: SwarmNode,
}

/// Server-held machine identities take precedence over worker report labels.
pub fn machine_key(
    node: Uuid,
    held_key: Option<&str>,
    reported_key: Option<&str>,
    held: &BTreeSet<&str>,
) -> String {
    held_key
        .or_else(|| reported_key.filter(|key| !key.is_empty() && !held.contains(key)))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("node:{node}"))
}

pub fn assemble(input: SwarmInput) -> Result<Swarm, TimeError> {
    let held = input
        .extras
        .iter()
        .filter_map(|e| e.machine_key.as_deref())
        .collect();
    let mut groups: BTreeMap<String, Vec<Built<'_>>> = BTreeMap::new();
    for n in input
        .capacity
        .nodes
        .iter()
        .filter(|n| n.health != WorkerHealth::Revoked)
    {
        let built = input.build_node(n, &held)?;
        groups.entry(built.key.clone()).or_default().push(built);
    }
    let mut out = Vec::with_capacity(groups.len());
    for (key, group) in groups {
        out.push(machines::machine(
            &key,
            group,
            input.controls.get(&key),
            input.now_ms,
        )?);
    }
    out.sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));

    let mut reasons = holds::waiting(&out);
    reasons.extend(
        input
            .history
            .iter()
            .filter(|h| !matches!(h.scope, aq_history_caps::Scope::Node(_)))
            .map(history_reason),
    );
    reasons.extend(launch_overdue_reasons(&input.launch_overdue));
    reasons.extend(task_hold_reasons(&input.task_holds));
    Ok(Swarm {
        server: input.server,
        machines: out,
        waiting: SwarmWaiting {
            reasons,
            agents: Vec::new(),
        },
    })
}
