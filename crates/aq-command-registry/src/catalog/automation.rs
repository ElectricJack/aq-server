//! Playbook, internal work and routing commands.
use super::*;

pub(super) const COMMANDS: &[CommandSpec] = &[
    // Playbook sources and authoring (spec 0013 §4.4), internal work
    // (spec 0012) and routing (spec 0011). The source and run routes
    // take no operation ID; each summary says what repeating one does.
    owner(
        "playbook.source.list",
        "dashboard.read",
        "Playbook sources, each with the playbook in effect and its latest authoring run",
        "/api/playbooks/sources",
    ),
    read(
        "playbook.source.show",
        "dashboard.read",
        TargetRule::Own,
        "One playbook source, with the playbook in effect and its latest authoring run",
    )
    .with_owner(Some(Route::get(
        "/api/playbooks/sources/{id}",
        &[("id", "source_id")],
    ))),
    owner_write(
        "playbook.source.register",
        "playbook.author",
        "Register a playbook source from a Markdown document and start authoring it (refused if the source exists)",
        post_route(
            "/api/playbooks/sources",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["id","binding","purpose","markdown","commands","events"],"properties":{"id":{"type":"string","minLength":1,"maxLength":128,"description":"The new source's ID; no '/' or control characters"},"binding":{"type":"string","minLength":1,"maxLength":128,"description":"The binding its playbook is activated under"},"purpose":{"type":"string","minLength":1,"maxLength":1024,"description":"What the playbook is for"},"markdown":{"type":"string","description":"The document the playbook is authored from"},"commands":{"type":"array","minItems":1,"uniqueItems":true,"items":{"type":"string"},"description":"Host catalog commands the playbook may call; its abilities follow from these"},"events":{"type":"array","uniqueItems":true,"items":{"type":"string"},"description":"Host catalog events the playbook may react to"}}}"#,
        ),
    ),
    with_method(
        owner_write(
            "playbook.source.edit",
            "playbook.author",
            "Replace a registered source's document at an expected revision and start authoring it",
            post_route(
                "/api/playbooks/sources/{id}/markdown",
                &[("id", "source_id")],
                r#"{"type":"object","additionalProperties":false,"required":["source_id","markdown","expected_revision"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"},"markdown":{"type":"string"},"expected_revision":{"type":"integer","minimum":0,"description":"The source's revision as last read"}}}"#,
            ),
        ),
        "PUT",
    ),
    owner_write(
        "playbook.source.author",
        "playbook.author",
        "Author a source again from its saved document (does nothing if nothing the agent sees has changed)",
        post_route(
            "/api/playbooks/sources/{id}/runs",
            &[("id", "source_id")],
            r#"{"type":"object","additionalProperties":false,"required":["source_id"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"}}}"#,
        ),
    ),
    owner_write(
        "playbook.source.rollback",
        "playbook.author",
        "Put a source's previous playbook back in effect and supersede a run in flight (each call goes back one more)",
        post_route(
            "/api/playbooks/sources/{id}/rollback",
            &[("id", "source_id")],
            r#"{"type":"object","additionalProperties":false,"required":["source_id"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"}}}"#,
        ),
    ),
    with_method(
        owner_write(
            "playbook.source.remove",
            "playbook.author",
            "Delete a registered source: its playbook stops and its grant is revoked; run history is kept",
            post_route(
                "/api/playbooks/sources/{id}",
                &[("id", "source_id")],
                r#"{"type":"object","additionalProperties":false,"required":["source_id"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"}}}"#,
            ),
        ),
        "DELETE",
    ),
    // The playbook engine's owner controls (playbooks/api.rs), mounted
    // only when the server runs a playbook driver. None takes an
    // operation ID; each summary says what repeating one does.
    owner(
        "playbook.status",
        "dashboard.read",
        "The playbook engine: its driver's state, the commands it knows and the events it holds",
        "/api/playbooks/status",
    ),
    owner_write(
        "playbook.event.retry",
        "playbook.run",
        "Deliver a held event again (refused unless the event is held)",
        post_route(
            "/api/playbooks/events/{id}/retry",
            &[("id", "event_id")],
            r#"{"type":"object","additionalProperties":false,"required":["event_id"],"properties":{"event_id":{"type":"string","format":"uuid","description":"The held event's ID, from playbook.status"}}}"#,
        ),
    ),
    owner_write(
        "playbook.event.hold",
        "playbook.run",
        "Hold an event that is waiting for delivery (refused if it is held or delivered)",
        post_route(
            "/api/playbooks/events/{id}/hold",
            &[("id", "event_id")],
            r#"{"type":"object","additionalProperties":false,"required":["event_id"],"properties":{"event_id":{"type":"string","format":"uuid","description":"The waiting event's ID"}}}"#,
        ),
    ),
    read(
        "playbook.admission.show",
        "dashboard.read",
        TargetRule::Own,
        "One playbook admission: a request to start a run, and what became of it",
    )
    .with_owner(Some(Route::get(
        "/api/playbooks/admissions/{id}",
        &[("id", "request_id")],
    ))),
    owner_write(
        "playbook.admission.recover",
        "playbook.run",
        "Look up an admission's run in the engine and record it (never starts a run; safe to repeat)",
        post_route(
            "/api/playbooks/admissions/{id}/recover",
            &[("id", "request_id")],
            r#"{"type":"object","additionalProperties":false,"required":["request_id"],"properties":{"request_id":{"type":"string","format":"uuid","description":"The admission's request ID"}}}"#,
        ),
    ),
    owner_write(
        "playbook.admission.retry",
        "playbook.run",
        "Admit a retained admission again; one whose outcome was unknown may run its work twice (repeating after it is admitted returns the same run)",
        post_route(
            "/api/playbooks/admissions/{id}/retry",
            &[("id", "request_id")],
            r#"{"type":"object","additionalProperties":false,"required":["request_id"],"properties":{"request_id":{"type":"string","format":"uuid","description":"The admission's request ID"}}}"#,
        ),
    ),
    owner_write(
        "playbook.driver.recover",
        "playbook.run",
        "Have the engine recover its runs now (safe to repeat)",
        post_route(
            "/api/playbooks/recover",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":[],"properties":{}}"#,
        ),
    ),
    owner_write(
        "playbook.driver.stop",
        "playbook.run",
        "Stop the playbook driver; queued messages are kept (safe to repeat, and allowed during maintenance)",
        post_route(
            "/api/playbooks/stop",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":[],"properties":{}}"#,
        ),
    ),
    owner(
        "internal.settings.show",
        "dashboard.read",
        "Which Intelligence profile internal work runs on, by default and per purpose",
        "/api/settings/internal-work",
    ),
    owner_write(
        "internal.settings.set",
        "intelligence.admin",
        "Choose the Intelligence profile internal work runs on, by default and per purpose",
        post_route(
            "/api/settings/internal-work",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation_id","expected_revision","overrides"],"properties":{"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"string","pattern":"^[0-9]{1,18}$","description":"The settings revision as last read"},"default_profile_id":{"type":["string","null"],"maxLength":128},"overrides":{"type":"object","description":"Purpose to profile ID","propertyNames":{"enum":["routing_classification","playbook_compilation","playbook_call","decision_revision","result_processing"]},"additionalProperties":{"type":"string","minLength":1,"maxLength":128}}}}"#,
        ),
    ),
    read(
        "internal.settings.operation.show",
        "dashboard.read",
        TargetRule::Own,
        "The recorded result of one internal.settings.set",
    )
    .with_owner(Some(Route::get(
        "/api/settings/internal-work/operations/{id}",
        &[("id", "operation_id")],
    ))),
    owner(
        "internal.run.list",
        "dashboard.read",
        "Internal work runs, newest first, by what they affect, purpose or status",
        "/api/internal-work/runs",
    ),
    read(
        "internal.run.show",
        "dashboard.read",
        TargetRule::Own,
        "One internal work run, with what its agents did",
    )
    .with_owner(Some(Route::get(
        "/api/internal-work/runs/{id}",
        &[("id", "run_id")],
    ))),
    owner_write(
        "internal.run.retry",
        "intelligence.admin",
        "Run a failed internal work run again as a new run (each call starts another)",
        post_route(
            "/api/internal-work/runs/{id}/retry",
            &[("id", "run_id")],
            r#"{"type":"object","additionalProperties":false,"required":["run_id"],"properties":{"run_id":{"type":"string","format":"uuid","description":"The internal work run's ID"}}}"#,
        ),
    ),
    owner_write(
        "internal.run.escalate",
        "intelligence.admin",
        "Hand a failed internal work run to the owner with a diagnosis",
        post_route(
            "/api/internal-work/runs/{id}/escalate",
            &[("id", "run_id")],
            r#"{"type":"object","additionalProperties":false,"required":["run_id","diagnosis"],"properties":{"run_id":{"type":"string","format":"uuid","description":"The internal work run's ID"},"diagnosis":{"type":"string","minLength":1,"maxLength":4000,"description":"What went wrong, at most 4000 bytes"}}}"#,
        ),
    ),
    owner_write(
        "internal.run.resolve",
        "intelligence.admin",
        "Mark an escalated internal work run, and the runs it retried, resolved",
        post_route(
            "/api/internal-work/runs/{id}/resolve",
            &[("id", "run_id")],
            r#"{"type":"object","additionalProperties":false,"required":["run_id"],"properties":{"run_id":{"type":"string","format":"uuid","description":"The internal work run's ID"}}}"#,
        ),
    ),
    owner(
        "routing.show",
        "dashboard.read",
        "Whether automatic routing is on, and the routing playbook source",
        "/api/settings/routing",
    ),
    owner(
        "routing.document.show",
        "dashboard.read",
        "The routing document: the text the routing playbook is authored from",
        "/api/settings/routing-document",
    ),
    owner_write(
        "routing.set",
        "intelligence.admin",
        "Turn automatic routing on or off (on only while a routing playbook is active)",
        post_route(
            "/api/settings/routing",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["enabled"],"properties":{"enabled":{"type":"boolean"}}}"#,
        ),
    ),
    read(
        "task.route.history",
        "dashboard.read",
        TargetRule::Own,
        "Every route a task has had, newest first, with reasons, and why it is unrouted if it is",
    )
    .with_owner(Some(Route::get(
        "/api/tasks/{id}/routes",
        &[("id", "task_id")],
    ))),
    read(
        "project.onboard.show",
        "dashboard.read",
        TargetRule::Own,
        "The state of one repository onboarding",
    )
    .with_owner(Some(Route::get(
        "/api/repositories/onboard/{operation_id}",
        &[],
    ))),
    read(
        "attempt.disposition.show",
        "dashboard.read",
        TargetRule::Own,
        "The record of an attempt's abandonment",
    )
    .with_owner(Some(Route::get(
        "/api/execution/attempts/{id}/disposition",
        &[("id", "attempt_id")],
    ))),
    read(
        "supervisor.operation.show",
        "dashboard.read",
        TargetRule::Own,
        "The receipt of one supervisor.set",
    )
    .with_owner(Some(Route::get(
        "/api/supervisors/{id}/operations/{operation}",
        &[("id", "conversation_id"), ("operation", "operation_id")],
    ))),
];
