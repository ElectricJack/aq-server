//! Owner administration commands.
use super::*;

pub(super) const COMMANDS: &[CommandSpec] = &[
    owner_write(
        "project.set",
        "project.admin",
        "Create a project or replace its binding (repository, profiles, limits, publication)",
        post_route(
            "/api/projects/{id}",
            &[("id", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["project_id","operation_id","expected_revision","binding"],"properties":{"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":0,"description":"The project's current revision; 0 creates it"},"binding":{"type":"object","description":"The whole binding, as project.show returns it; its project_id must be this project"}}}"#,
        ),
    ),
    owner_write(
        "project.disable",
        "project.admin",
        "Disable a project so no new work starts in it",
        post_route(
            "/api/projects/{id}/disable",
            &[("id", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["project_id","operation_id","expected_revision"],"properties":{"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":1}}}"#,
        ),
    ),
    owner_write(
        "project.onboard",
        "project.admin",
        "Onboard a GitHub repository as a project (resumes when repeated with the same operation ID)",
        post_route(
            "/api/repositories/onboard",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation_id","project_id","repository_id","github","transport","publication","template"],"properties":{"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"project_id":{"type":"string","format":"uuid"},"repository_id":{"type":"string","format":"uuid"},"github":{"type":"object","additionalProperties":false,"required":["owner","name"],"properties":{"owner":{"type":"string","minLength":1,"maxLength":100},"name":{"type":"string","minLength":1,"maxLength":100}}},"transport":{"enum":["ssh","https"]},"credential_profile":{"type":"string","description":"A configured credential profile of the transport's kind"},"default_branch":{"type":"string"},"publication":{"type":"object","additionalProperties":false,"required":["mode"],"properties":{"mode":{"enum":["pull_request","fast_forward"]},"wait_for_checks":{"type":"object","additionalProperties":false,"required":["timeout_secs"],"properties":{"timeout_secs":{"type":"integer","minimum":0}}}}},"template":{"type":"object","description":"The project binding without its project, repository and publication fields"}}}"#,
        ),
    ),
    owner_write(
        "attempt.cancel",
        "attempt.control",
        "Ask a running attempt to stop",
        post_route(
            "/api/execution/attempts/{id}/cancel",
            &[("id", "attempt_id")],
            r#"{"type":"object","additionalProperties":false,"required":["attempt_id","operation_id","reason"],"properties":{"attempt_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"reason":{"type":"string","minLength":1,"maxLength":4096}}}"#,
        ),
    ),
    owner_write(
        "attempt.abandon",
        "attempt.control",
        "Abandon a stopped, unsettled attempt and release its hold on the task (cannot be undone)",
        post_route(
            "/api/execution/attempts/{id}/disposition",
            &[("id", "attempt_id")],
            r#"{"type":"object","additionalProperties":false,"required":["attempt_id","operation_id","reason"],"properties":{"attempt_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"reason":{"type":"string","minLength":1,"maxLength":4096}}}"#,
        ),
    ),
    owner_write(
        "supervisor.set",
        "supervisor.control",
        "Turn a supervisor on or off and choose the profile it runs on",
        post_route(
            "/api/supervisors/{id}/control",
            &[("id", "conversation_id")],
            r#"{"type":"object","additionalProperties":false,"required":["conversation_id","operation_id","expected_revision","enabled"],"properties":{"conversation_id":{"type":"string","format":"uuid","description":"The supervisor's conversation, as supervisor.list shows it"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":0},"enabled":{"type":"boolean"},"intelligence_profile":{"type":"object","additionalProperties":false,"required":["profile_id","candidate_id"],"description":"A level and one of its candidates","properties":{"profile_id":{"type":"string","description":"The level document's id"},"candidate_id":{"type":"string","description":"provider:model:reasoning, as the list shows it"}}}}}"#,
        ),
    ),
    owner_write(
        "notification.retry",
        "notification.admin",
        "Retry a notification delivery whose outcome is unknown, accepting a possible duplicate",
        post_route(
            "/api/notifications/retry",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation_id","delivery_id","expected_revision","accept_duplicate_risk"],"properties":{"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"delivery_id":{"type":"string","format":"uuid"},"expected_revision":{"type":"integer","minimum":0},"accept_duplicate_risk":{"const":true}}}"#,
        ),
    ),
    owner_write(
        "session.manual.start",
        "worker.admin",
        "Start a session by hand on a worker for a project",
        post_route(
            "/api/manual-agents/workers/{node}/projects/{project}",
            &[("node", "node_id"), ("project", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["node_id","project_id","operation_id"],"properties":{"node_id":{"type":"string","format":"uuid"},"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Becomes the session's ID; repeat it to retry safely"},"intelligence_profile":{"type":"object","additionalProperties":false,"required":["profile_id","candidate_id"],"description":"A level and one of its candidates","properties":{"profile_id":{"type":"string","description":"The level document's id"},"candidate_id":{"type":"string","description":"provider:model:reasoning, as the list shows it"}}}}}"#,
        ),
    ),
    owner_write(
        "session.manual.stop",
        "worker.admin",
        "Stop a session started by hand, keeping its workspace",
        post_route(
            "/api/manual-agents/{id}/stop",
            &[("id", "session_id")],
            r#"{"type":"object","additionalProperties":false,"required":["session_id","operation_id"],"properties":{"session_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"}}}"#,
        ),
    ),
    owner_write(
        "session.manual.slots.set",
        "worker.admin",
        "Set how many hand-started sessions a worker offers a project",
        post_route(
            "/api/manual-agents/workers/{node}/projects/{project}/slots",
            &[("node", "node_id"), ("project", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["node_id","project_id","operation_id","expected_revision","slots"],"properties":{"node_id":{"type":"string","format":"uuid"},"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":0},"slots":{"type":"integer","minimum":0,"maximum":8}}}"#,
        ),
    ),
    owner_write(
        "wait.suspend",
        "attempt.control",
        "Suspend the attempt behind a wait (repeating returns the same directive)",
        post_route(
            "/api/waits/{wait_id}/suspend",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["wait_id"],"properties":{"wait_id":{"type":"string","format":"uuid"}}}"#,
        ),
    ),
    owner_write(
        "sandbox.release",
        "attempt.control",
        "Release the protected disk of a suspended attempt",
        post_route(
            "/api/hibernation/{continuation_id}/disk-release",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["continuation_id"],"properties":{"continuation_id":{"type":"string","format":"uuid"}}}"#,
        ),
    ),
    owner_write(
        "operation.recover",
        "operation.recover",
        "Retry the dispatch of a task create whose outcome was uncertain",
        post_route(
            "/api/human/operations/{operation}/recover",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation"],"properties":{"operation":{"type":"string","format":"uuid","description":"The operation ID of the original create"}}}"#,
        ),
    ),
    owner_write(
        "worker.enroll",
        "worker.admin",
        "Create a one-time enrollment code for a new worker (each call makes a new code)",
        post_route(
            "/api/enrollments",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":[],"properties":{}}"#,
        ),
    ),
];
