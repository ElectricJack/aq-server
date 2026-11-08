//! Swarm read model (spec 0007 phase 2), served at `GET /api/v1/swarm`.
use crate::{EntityRef, NodeSlots, ProjectRef, Timestamp, TitledRef, WorkerHealth};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The whole swarm: the server, its machines and swarm-wide waiting reasons.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Swarm {
    pub server: SwarmServer,
    pub machines: Vec<SwarmMachine>,
    pub waiting: SwarmWaiting,
}
/// The server root of the tree.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmServer {
    pub version: String,
    pub source_revision: String,
    pub running_agents: u32,
    pub owner_availability: OwnerAvailability,
}
/// The owner's sticky availability switch (spec 0007 Q2). `unset` keeps the
/// manual observations of before; `off` stops new offers.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OwnerAvailabilityState {
    Unset,
    On,
    Off,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnerAvailability {
    pub state: OwnerAvailabilityState,
    /// Decimal compare-and-swap generation for swarm.availability.set.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub generation: String,
    pub since: Option<Timestamp>,
    /// Why the state differs from the last switch, e.g. `unset` because the
    /// owner who set it is no longer an owner. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 200))]
    pub note: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Local,
    Cloud,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    Online,
    Holding,
    Offline,
    Stopped,
    Booting,
}
/// A group of worker nodes that report the same machine key.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmMachine {
    /// Machine key, or `node:<uuid>` for a node without one.
    pub id: String,
    pub label: String,
    pub kind: MachineKind,
    pub state: MachineState,
    pub holding: Vec<HoldReason>,
    /// Newest node heartbeat.
    pub last_seen_at: Option<Timestamp>,
    /// None when the machine is offline or has not reported.
    pub load: Option<MachineLoad>,
    pub hardware: Option<MachineHardware>,
    pub agent_max: Option<u32>,
    /// Cost of a cloud machine; always None until cost reporting exists.
    pub cost: Option<MachineCost>,
    /// Present only for a service machine (a server-held key); None for legacy and `node:` machines.
    pub control: Option<MachineControl>,
    pub nodes: Vec<SwarmNode>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineDesired {
    Running,
    Draining,
    Stopped,
}
/// The owner's control of a service machine (spec 0007 phase 1).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MachineControl {
    pub desired: MachineDesired,
    /// Decimal compare-and-swap generation for worker.start/stop/drain.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub generation: String,
    pub since: Option<Timestamp>,
    pub pending_handoffs: u32,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadSource {
    Linux,
    WindowsHost,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostProbe {
    NotApplicable,
    Fresh,
    Stale,
    Missing,
}
/// Live machine load in basis points (0-10000).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MachineLoad {
    pub source: LoadSource,
    pub host_probe: HostProbe,
    pub cpu_busy_bps: u16,
    pub memory_used_bps: u16,
    /// Mean of the samples in the last 15 minutes; null when there are none.
    pub cpu_avg_15m_bps: Option<u16>,
    pub memory_avg_15m_bps: Option<u16>,
    pub samples_15m: u32,
    pub observed_at: Timestamp,
    pub fresh: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MachineHardware {
    pub logical_cpus: u32,
    pub memory_total_bytes: u64,
    pub disk_total_bytes: Option<u64>,
    pub disk_free_bytes: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MachineCost {
    pub currency: String,
    pub micros_per_hour: u64,
    pub started_at: Timestamp,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Running,
    Stopped,
    Draining,
    Unreported,
}
/// The worker service state of a node.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerService {
    pub state: ServiceState,
    pub since: Option<Timestamp>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReportedState {
    Ready,
    Unavailable,
    Unreported,
}
/// One worker node of a machine.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmNode {
    /// Entity type `worker`.
    pub node: EntityRef,
    pub name: String,
    pub health: WorkerHealth,
    pub service: WorkerService,
    /// `docker-local`, `boat` and so on.
    pub backend_kind: Option<String>,
    pub worker_version: String,
    /// `sha256:<64 hex>` digests.
    pub sandbox_templates: Vec<String>,
    /// Always `unreported`: no node reports it yet.
    pub agent_channel: ReportedState,
    pub last_seen_at: Option<Timestamp>,
    pub slots: NodeSlots,
    pub holding: Vec<HoldReason>,
    pub agents: Vec<SwarmAgent>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntelligenceClass {
    Fast,
    Standard,
    Deep,
}
/// A running agent on a node.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmAgent {
    pub session_id: Uuid,
    pub attempt_id: Uuid,
    pub task: Option<TitledRef>,
    pub project: ProjectRef,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub harness: Option<String>,
    pub intelligence_class: Option<IntelligenceClass>,
    pub terminal: Option<EntityRef>,
    pub started_at: Option<Timestamp>,
}
/// Swarm-wide reasons why new work cannot start now.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmWaiting {
    pub reasons: Vec<HoldReason>,
    /// Empty until placement produces per-agent waits.
    pub agents: Vec<WaitingAgent>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitingAgent {
    pub task: TitledRef,
    pub project: ProjectRef,
    pub provider: Option<String>,
    pub intelligence_class: Option<IntelligenceClass>,
    pub reasons: Vec<HoldReason>,
    pub since: Option<Timestamp>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HoldCode {
    WorkerOffline,
    WorkerRevoked,
    WorkerStopped,
    WorkerDraining,
    CapabilityUnreported,
    CapabilityExpired,
    MeasurementExpired,
    MeasurementUnavailable,
    NodeSpendCeilingReached,
    NodeSpendUnaccounted,
    NodeDiskPressure,
    CpuAboveThreshold,
    MemoryAboveThreshold,
    AgentMaximumReached,
    NoMachineOnline,
    NoEligibleMachine,
    NoFreeSlot,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HoldReason {
    pub code: HoldCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swarm_enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(MachineState::Holding).unwrap(),
            "holding"
        );
        assert_eq!(
            serde_json::to_value(LoadSource::WindowsHost).unwrap(),
            "windows_host"
        );
        assert_eq!(
            serde_json::to_value(HostProbe::NotApplicable).unwrap(),
            "not_applicable"
        );
        assert_eq!(
            serde_json::to_value(ServiceState::Unreported).unwrap(),
            "unreported"
        );
        assert_eq!(
            serde_json::to_value(HoldCode::NoFreeSlot).unwrap(),
            "no_free_slot"
        );
        assert_eq!(
            serde_json::to_value(IntelligenceClass::Deep).unwrap(),
            "deep"
        );
    }
    #[test]
    fn hold_reason_omits_absent_remedy_and_refuses_unknown_fields() {
        let r = HoldReason {
            code: HoldCode::WorkerOffline,
            message: "Worker is offline".into(),
            remedy: None,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"code":"worker_offline","message":"Worker is offline"})
        );
        assert!(
            serde_json::from_value::<HoldReason>(
                serde_json::json!({"code":"worker_offline","message":"m","extra":1})
            )
            .is_err()
        );
    }
    #[test]
    fn schema_lists_the_swarm_response() {
        assert!(crate::schema().to_string().contains("SwarmMachine"));
    }
}
