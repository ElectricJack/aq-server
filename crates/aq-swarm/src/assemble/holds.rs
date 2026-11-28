use super::*;
/// The reason copy for a code (message, remedy).
pub fn hold(code: HoldCode) -> HoldReason {
    let (message, remedy) = hold_text(code);
    HoldReason {
        code,
        message: message.into(),
        remedy: remedy.map(Into::into),
    }
}
fn hold_text(code: HoldCode) -> (&'static str, Option<&'static str>) {
    use HoldCode::*;
    match code {
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
        SandboxAdmissionDeferred => (
            "Worker cannot admit sandboxes",
            Some("Free disk space or fix the sandbox daemon; the worker resumes on its own"),
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
        // Unused: the rate limits were removed on 2026-11-13 and nothing produces this
        // code. It stays only because the pinned dashboard-read-1 schema lists it.
        HistoryRateLimited => ("Rate limit reached", None),
        HistoryCapReached => (
            "Live history cap reached",
            Some("AQ rechecks usage when workers poll"),
        ),
        TasksHeld => (
            "Tasks are held",
            Some("Open a held task to see how it can resume"),
        ),
        LaunchOverdue => (
            "Launch overdue",
            Some("AQ ends the attempt as not started and offers its task to another node"),
        ),
    }
}

/// One reason per granted attempt whose agent did not start within its launch deadline:
/// `Launch overdue on <node>: task <title> has not started its agent`.
pub fn launch_overdue_reasons(overdue: &[aq_machine_control::lapse::Overdue]) -> Vec<HoldReason> {
    (overdue.iter())
        .map(|o| {
            let mut reason = hold(HoldCode::LaunchOverdue);
            let node: String = o.node_name.chars().take(128).collect();
            let task: String = crate::agents::title_or_id(o.task_id)
                .chars()
                .take(256)
                .collect();
            reason.message = format!(
                "Launch overdue on {node}: task {task} has not started its agent (attempt {})",
                o.attempt_id
            );
            reason
        })
        .collect()
}

/// R6: one reason per project with held tasks, counted by hold reason code.
pub fn task_hold_reasons(counts: &[(Uuid, String, i64)]) -> Vec<HoldReason> {
    let mut by_project: BTreeMap<Uuid, Vec<(&str, i64)>> = BTreeMap::new();
    for (project, code, n) in counts {
        by_project.entry(*project).or_default().push((code, *n));
    }
    (by_project.into_iter())
        .map(|(project, codes)| {
            let total: i64 = codes.iter().map(|(_, n)| n).sum();
            let each: Vec<String> = codes.iter().map(|(c, n)| format!("{c} {n}")).collect();
            let mut reason = hold(HoldCode::TasksHeld);
            let tasks = if total == 1 { "task" } else { "tasks" };
            reason.message = format!(
                "Project {project}: {total} {tasks} held ({})",
                each.join(", ")
            );
            reason
        })
        .collect()
}

/// A spec 0008 history hold with its own counted message.
pub fn history_reason(h: &aq_history_caps::Hold) -> HoldReason {
    HoldReason {
        code: HoldCode::HistoryCapReached,
        message: h.message(),
        remedy: Some(h.remedy().into()),
    }
}

pub(super) fn mapped(reason: IneligibleReason) -> Option<HoldCode> {
    Some(match reason {
        IneligibleReason::WorkerOffline
        | IneligibleReason::WorkerRevoked
        | IneligibleReason::NodeConcurrencyCeilingReached => return None,
        IneligibleReason::CapabilityUnreported => HoldCode::CapabilityUnreported,
        IneligibleReason::CapabilityExpired => HoldCode::CapabilityExpired,
        IneligibleReason::NodeSpendCeilingReached => HoldCode::NodeSpendCeilingReached,
        IneligibleReason::NodeSpendUnaccounted => HoldCode::NodeSpendUnaccounted,
        IneligibleReason::NodeDiskPressure => HoldCode::NodeDiskPressure,
        IneligibleReason::SandboxAdmissionDeferred => HoldCode::SandboxAdmissionDeferred,
    })
}

pub(super) fn waiting(machines: &[SwarmMachine]) -> Vec<HoldReason> {
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
