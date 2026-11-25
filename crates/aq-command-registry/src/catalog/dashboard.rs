//! Dashboard and operational reads.
use super::*;

pub(super) const COMMANDS: &[CommandSpec] = &[
    owner(
        "dashboard.bootstrap",
        "dashboard.read",
        "Dashboard bootstrap: principal, product and capabilities",
        "/api/v1/bootstrap",
    ),
    owner(
        "dashboard.overview",
        "dashboard.read",
        "The dashboard overview",
        "/api/v1/overview",
    ),
    owner(
        "dashboard.capacity",
        "dashboard.read",
        "Worker capacity",
        "/api/v1/capacity",
    ),
    owner(
        "dashboard.swarm",
        "dashboard.read",
        "Swarm: machines, worker nodes and running agents",
        "/api/v1/swarm",
    ),
    owner(
        "dashboard.provider_usage",
        "dashboard.read",
        "Provider usage and quota",
        "/api/v1/provider-usage",
    ),
    owner(
        "project.list",
        "dashboard.read",
        "Projects",
        "/api/v1/projects",
    ),
    owner(
        "session.list",
        "dashboard.read",
        "Sessions",
        "/api/v1/sessions",
    ),
    read(
        "session.show",
        "dashboard.read",
        TargetRule::Own,
        "One session",
    )
    .with_owner(Some(Route::get(
        "/api/v1/sessions/{id}",
        &[("id", "session_id")],
    ))),
    read(
        "session.operations",
        "dashboard.read",
        TargetRule::Own,
        "A session's operations",
    )
    .with_owner(Some(Route::get(
        "/api/v1/sessions/{id}/operations",
        &[("id", "session_id")],
    ))),
    read(
        "session.events",
        "dashboard.read",
        TargetRule::Own,
        "A session's observed events",
    )
    .with_owner(Some(Route::get(
        "/api/observations/sessions/{attempt}/events",
        &[("attempt", "attempt_id")],
    ))),
    read(
        "session.usage",
        "dashboard.read",
        TargetRule::Own,
        "A session's observed usage",
    )
    .with_owner(Some(Route::get(
        "/api/observations/sessions/{attempt}/usage",
        &[("attempt", "attempt_id")],
    ))),
    read(
        "terminal.show",
        "dashboard.read",
        TargetRule::Own,
        "A terminal",
    )
    .with_owner(Some(Route::get(
        "/api/v1/terminals/{terminal}",
        &[("terminal", "terminal_id")],
    ))),
    read(
        "terminal.output",
        "dashboard.read",
        TargetRule::Own,
        "A terminal's output",
    )
    .with_owner(Some(Route::get(
        "/api/v1/terminals/{terminal}/output",
        &[("terminal", "terminal_id")],
    ))),
    read(
        "terminal.tail",
        "dashboard.read",
        TargetRule::Own,
        "A terminal's tail",
    )
    .with_owner(Some(Route::get(
        "/api/v1/terminals/{terminal}/tail",
        &[("terminal", "terminal_id")],
    ))),
    read(
        "terminal.receipt",
        "dashboard.read",
        TargetRule::Own,
        "A terminal mutation receipt",
    )
    .with_owner(Some(Route::get(
        "/api/v1/terminals/{terminal}/receipts/{operation}",
        &[("terminal", "terminal_id"), ("operation", "operation_id")],
    ))),
    owner(
        "notification.list",
        "dashboard.read",
        "Notifications",
        "/api/notifications",
    ),
    read(
        "notification.show",
        "dashboard.read",
        TargetRule::Own,
        "One notification",
    )
    .with_owner(Some(Route::get(
        "/api/notifications/{message}",
        &[("message", "message_id")],
    ))),
    owner(
        "review.decision.list",
        "dashboard.read",
        "Review decisions",
        "/api/reviews/decisions",
    ),
    read(
        "review.decision.show",
        "dashboard.read",
        TargetRule::Own,
        "One review decision",
    )
    .with_owner(Some(Route::get(
        "/api/reviews/decisions/{id}",
        &[("id", "decision_id")],
    ))),
    owner(
        "playbook.inspection.runs",
        "dashboard.read",
        "Playbook runs",
        "/api/playbooks/inspection/runs",
    ),
    read(
        "playbook.inspection.run",
        "dashboard.read",
        TargetRule::Own,
        "One playbook run",
    )
    .with_owner(Some(Route::get(
        "/api/playbooks/inspection/runs/{id}",
        &[("id", "run_id")],
    ))),
    owner(
        "playbook.inspection.admissions",
        "dashboard.read",
        "Playbook admissions",
        "/api/playbooks/inspection/admissions",
    ),
    owner(
        "playbook.catalog.definitions",
        "dashboard.read",
        "Playbook catalog definitions",
        "/api/playbooks/inspection/catalog/definitions",
    ),
    owner(
        "playbook.catalog.activations",
        "dashboard.read",
        "Playbook catalog activations",
        "/api/playbooks/inspection/catalog/activations",
    ),
    owner(
        "playbook.loaded_definitions",
        "dashboard.read",
        "Loaded playbook definitions",
        "/api/playbooks/inspection/loaded-definitions",
    ),
    owner(
        "provider.accounts",
        "dashboard.read",
        "Provider accounts",
        "/api/human/provider-accounts",
    ),
    read(
        "project.show",
        "dashboard.read",
        P,
        "One project: its current binding, repository and execution settings",
    )
    .with_owner(Some(Route::get(
        "/api/projects/{id}",
        &[("id", "project_id")],
    ))),
    read(
        "project.revision.show",
        "dashboard.read",
        P,
        "A project's binding as it was at an earlier revision",
    )
    .with_owner(Some(Route::get(
        "/api/projects/{id}/revisions/{revision}",
        &[("id", "project_id")],
    ))),
    owner(
        "worker.list",
        "dashboard.read",
        "Worker nodes: version, last seen, status and storage",
        "/api/workers",
    ),
    read(
        "attempt.show",
        "dashboard.read",
        TargetRule::Own,
        "One execution attempt's state",
    )
    .with_owner(Some(Route::get(
        "/api/execution/attempts/{id}",
        &[("id", "attempt_id")],
    ))),
    owner(
        "server.status",
        "dashboard.read",
        "The server's version, health and update state",
        "/api/status",
    ),
    owner(
        "supervisor.list",
        "dashboard.read",
        "Supervisors, with the profiles a supervisor can run on",
        "/api/supervisors",
    ),
    owner(
        "session.manual.list",
        "dashboard.read",
        "Sessions the owner started by hand, with the workers, slots and projects to start one on",
        "/api/manual-agents",
    ),
    owner(
        "intelligence.list",
        "dashboard.read",
        "The intelligence profiles (levels): each document with its candidates",
        "/api/settings/intelligence-profiles",
    ),
    owner(
        "intelligence.registry.show",
        "dashboard.read",
        "The intelligence registry: credentials and recent observations",
        "/api/intelligence/registry",
    ),
];
