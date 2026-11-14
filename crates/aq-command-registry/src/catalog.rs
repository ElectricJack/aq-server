//! The static catalog: reads, agent commands and the non-owner routes of
//! action kinds. Mutations that are `ActionKind`s are added by `Registry::new`.
use super::{Body, CommandSpec, Kind, Params, Route, TargetRule};
use aq_agent_grants::op;

const fn read(
    name: &'static str,
    grant: &'static str,
    target: TargetRule,
    summary: &'static str,
    owner: Option<Route>,
) -> CommandSpec {
    CommandSpec {
        name,
        kind: Kind::Read,
        grant: Some(grant),
        target,
        summary,
        owner,
        supervisor: None,
        agent: None,
    }
}
const fn agent(mut c: CommandSpec, route: Route) -> CommandSpec {
    c.agent = Some(route);
    c
}
const fn mutation(
    name: &'static str,
    grant: &'static str,
    target: TargetRule,
    summary: &'static str,
) -> CommandSpec {
    CommandSpec {
        name,
        kind: Kind::Mutation,
        grant: Some(grant),
        target,
        summary,
        owner: None,
        supervisor: None,
        agent: None,
    }
}
const fn get(path: &'static str) -> Option<Route> {
    Some(Route::get(path, &[]))
}

const AGENT_TASK: &str = r#"{"type":"object","additionalProperties":false,"required":["operation_id","title"],"properties":{"operation_id":{"type":"string","format":"uuid"},"title":{"type":"string","minLength":1,"maxLength":512},"body":{"type":"string","maxLength":32768},"acceptance_criteria":{"type":"string","maxLength":8192},"labels":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128}},"parent_id":{"anyOf":[{"type":"string","format":"uuid"},{"const":"self"},{"type":"null"}]},"priority":{"type":"integer","minimum":1,"maximum":300},"task_type":{"type":"string","minLength":1,"maxLength":128},"task_state":{"enum":["active","draft"]},"expected_project_revision":{"type":"string","pattern":"^[1-9][0-9]{0,18}$"},"profile_hint":{"type":"null","description":"An agent cannot choose the profile of a task it creates"},"intelligence_hint":{"type":"null","description":"An agent cannot choose the intelligence of a task it creates"}}}"#;
/// An agent's task read: an optional `task_id` (the attempt's own task when
/// omitted) and `project_id` (the attempt's project), plus `extra` fields.
macro_rules! task_params {
    ($extra:literal) => {
        concat!(
            r#"{"type":"object","additionalProperties":false,"properties":{"task_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's own task"},"project_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's project; another is refused"}"#,
            $extra,
            "}}"
        )
    };
}
/// The agent's scoped graph read of the task named by `task_id`.
const fn graph_task(kind: &'static str, defaults: &'static str, params: &'static str) -> Route {
    Route {
        method: "POST",
        path: "/v1/agent/graph/read",
        body: Body::GraphRead(Some(kind), defaults),
        params: Params::Inline(params),
        rename: &[("node_id", "task_id")],
    }
}
/// The agent's revisioned comment edit or delete: the comment and the
/// revision it expects (the route takes them from `params`, not a subject).
const COMMENT_EDIT: &str = r#"{"type":"object","additionalProperties":false,"required":["operation_id","comment_id","expected_revision","body"],"properties":{"operation_id":{"type":"string","format":"uuid"},"comment_id":{"type":"string","format":"uuid"},"expected_revision":{"type":"string","pattern":"^[1-9][0-9]{0,18}$"},"body":{"type":"string","minLength":1,"maxLength":16384}}}"#;
const COMMENT_DELETE: &str = r#"{"type":"object","additionalProperties":false,"required":["operation_id","comment_id","expected_revision"],"properties":{"operation_id":{"type":"string","format":"uuid"},"comment_id":{"type":"string","format":"uuid"},"expected_revision":{"type":"string","pattern":"^[1-9][0-9]{0,18}$"}}}"#;
const P: TargetRule = TargetRule::Project("project_id");
const T: TargetRule = TargetRule::Task("task_id");
const TASK: &[(&str, &str)] = &[("project", "project_id"), ("node", "task_id")];
const KNOW: &[(&str, &str)] = &[("project", "project_id"), ("node", "node_id")];
const PROJ: &[(&str, &str)] = &[("project", "project_id")];

pub fn catalog() -> Vec<CommandSpec> {
    let task = |name, path, summary| {
        read(
            name,
            op::TASK_READ,
            T,
            summary,
            Some(Route::get(path, TASK)),
        )
    };
    let owner = |name, grant, summary, path| read(name, grant, TargetRule::Own, summary, get(path));
    let mut whoami = read(
        "whoami",
        "",
        TargetRule::Own,
        "The calling principal: attempt, task, project, profile, grants and credential expiry",
        Some(Route::local()),
    );
    whoami.grant = None;
    whoami.supervisor = Some(Route::local());
    whoami.agent = Some(Route::local());
    let mut operation = read(
        "operation.show",
        "",
        TargetRule::Own,
        "One operation by ID, with its current disposition (never resends it)",
        Some(Route::get(
            "/api/v1/operations/{id}",
            &[("id", "operation_id")],
        )),
    );
    operation.grant = None;
    operation.supervisor = Some(Route::get(
        "/api/v1/operations/{id}",
        &[("id", "operation_id")],
    ));
    operation.agent = Some(Route::get("/v1/agent/operations/{operation_id}", &[]));
    let mut task_create = mutation(
        "task.create",
        op::TASK_CREATE_SUBTASK,
        TargetRule::ParentTask {
            field: "parent_id",
            unparented: op::TASK_CREATE_PROJECT,
        },
        "Create a task (a subtask when parent_id is given)",
    );
    task_create.supervisor = Some(Route::action());
    // An agent's subtask: parent_id "self" or omitted is its own task; the
    // project revision is the current one unless given.
    task_create.agent = Some(Route::post(
        "/v1/agent/tasks",
        Body::Versioned,
        Params::Inline(AGENT_TASK),
    ));
    let mut graph_create = mutation(
        "task.graph.create",
        op::TASK_CREATE_SUBTASK,
        TargetRule::ParentTask {
            field: "parent_id",
            unparented: op::TASK_CREATE_PROJECT,
        },
        "Create several related tasks and their edges in one operation",
    );
    graph_create.supervisor = Some(Route::action());
    let mut commands = vec![
        whoami,
        operation,
        task_create,
        graph_create,
        read(
            "task.list",
            op::TASK_READ,
            P,
            "Tasks in a project",
            Some(Route::get("/api/human/projects/{project}/tasks", PROJ)),
        ),
        agent(
            task(
                "task.show",
                "/api/human/projects/{project}/tasks/{node}",
                "One task",
            ),
            graph_task("get", "{}", task_params!("")),
        ),
        CommandSpec {
            agent: Some(graph_task(
                "relationships",
                r#"{"direction":"outgoing","edge_types":["parent-child"],"limit":64,"cursor":null}"#,
                task_params!(r#","limit":{"type":"integer","minimum":1,"maximum":64}"#),
            )),
            ..read(
                "task.children",
                op::TASK_READ,
                T,
                "A task's direct subtasks (its outgoing parent-child edges)",
                None,
            )
        },
        task(
            "task.graph.show",
            "/api/human/projects/{project}/tasks/{node}/graph",
            "A task's dependency graph",
        ),
        agent(
            task(
                "task.history",
                "/api/human/projects/{project}/tasks/{node}/history",
                "A task's history",
            ),
            graph_task(
                "history",
                r#"{"limit":20,"cursor":null}"#,
                task_params!(
                    r#","limit":{"type":"integer","minimum":1,"maximum":64},"cursor":{"anyOf":[{"type":"string","minLength":1,"maxLength":2048},{"type":"null"}]}"#
                ),
            ),
        ),
        task(
            "task.summary",
            "/api/human/projects/{project}/tasks/{node}/summary",
            "A task's work summary",
        ),
        task(
            "task.work",
            "/api/human/projects/{project}/tasks/{node}/work",
            "A task's attempts and work",
        ),
        read(
            "task.overview",
            op::TASK_READ,
            TargetRule::Own,
            "Tasks across projects",
            get("/api/human/overview/tasks"),
        ),
        agent(
            read(
                "comment.list",
                op::COMMENT_READ,
                T,
                "A task's comment thread, newest first",
                Some(Route::get(
                    "/api/human/projects/{project}/tasks/{node}/comments",
                    TASK,
                )),
            ),
            Route::post(
                "/v1/agent/comments/list",
                Body::Versioned,
                Params::Inline(task_params!(
                    r#","cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100}"#
                )),
            ),
        ),
        agent(
            mutation(
                "task.comment.add",
                op::COMMENT_WRITE,
                TargetRule::OwnTask,
                "Comment on a task (an agent comments on its own task)",
            ),
            Route::post(
                "/v1/agent/comments",
                Body::Versioned,
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["operation_id","body"],"properties":{"operation_id":{"type":"string","format":"uuid"},"body":{"type":"string","minLength":1,"maxLength":16384}}}"#,
                ),
            ),
        ),
        agent(
            mutation(
                "task.comment.edit",
                op::COMMENT_WRITE,
                TargetRule::Comment("comment_id"),
                "Edit a comment at its current revision (an agent edits only its own)",
            ),
            Route::post(
                "/v1/agent/comments/edit",
                Body::Versioned,
                Params::Inline(COMMENT_EDIT),
            ),
        ),
        agent(
            mutation(
                "task.comment.delete",
                op::COMMENT_WRITE,
                TargetRule::Comment("comment_id"),
                "Delete a comment at its current revision (an agent deletes only its own)",
            ),
            Route::post(
                "/v1/agent/comments/delete",
                Body::Versioned,
                Params::Inline(COMMENT_DELETE),
            ),
        ),
        read(
            "knowledge.list",
            op::KNOWLEDGE_READ,
            P,
            "Knowledge records in a project",
            Some(Route::get("/api/human/projects/{project}/knowledge", PROJ)),
        ),
        agent(
            read(
                "knowledge.show",
                op::KNOWLEDGE_READ,
                P,
                "One knowledge record",
                Some(Route::get(
                    "/api/human/projects/{project}/knowledge/{node}",
                    KNOW,
                )),
            ),
            Route::post(
                "/v1/agent/graph/read",
                Body::GraphRead(Some("get"), "{}"),
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["node_id"],"properties":{"node_id":{"type":"string","format":"uuid"}}}"#,
                ),
            ),
        ),
        agent(
            read(
                "knowledge.search",
                op::KNOWLEDGE_READ,
                P,
                "Search tasks and knowledge",
                get("/api/v1/search"),
            ),
            Route::post(
                "/v1/agent/graph/read",
                Body::GraphRead(
                    Some("search"),
                    r#"{"kind_filter":null,"limit":20,"cursor":null}"#,
                ),
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string","minLength":1,"maxLength":1024},"kind_filter":{"anyOf":[{"enum":["task","knowledge"]},{"type":"null"}]},"limit":{"type":"integer","minimum":1,"maximum":64},"cursor":{"anyOf":[{"type":"string"},{"type":"null"}]}}}"#,
                ),
            ),
        ),
        CommandSpec {
            agent: Some(Route::post(
                "/v1/agent/graph/read",
                Body::GraphRead(None, "{}"),
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["query"],"properties":{"query":{"$ref":"protocol/agent-access/v1/schema.json#/$defs/ReadQuery"}}}"#,
                ),
            )),
            ..read(
                "graph.read",
                op::KNOWLEDGE_READ,
                P,
                "A scoped graph read (get, revision, history, search, context, relationships)",
                None,
            )
        },
        CommandSpec {
            agent: Some(Route::post(
                "/v1/agent/graph/commands",
                Body::GraphCommand,
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["commands"],"properties":{"operation_id":{"type":"string","format":"uuid"},"reason":{"type":"string","minLength":1,"maxLength":4096},"commands":{"type":"array","minItems":1,"maxItems":16,"items":{"$ref":"protocol/agent-access/v1/schema.json#/$defs/GraphCommand"}}}}"#,
                ),
            )),
            ..mutation(
                "graph.command",
                op::KNOWLEDGE_AUTHOR,
                P,
                "Durable graph intent: findings, corrections, relationships, proposals",
            )
        },
        agent(
            mutation(
                "question.ask",
                op::QUESTION_ASK,
                TargetRule::Own,
                "Ask the owner a question; wait on it with question.show",
            ),
            Route::post(
                "/v1/agent/questions",
                Body::Versioned,
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["operation_id","question"],"properties":{"operation_id":{"type":"string","format":"uuid"},"question":{"type":"string","minLength":1,"maxLength":4096}}}"#,
                ),
            ),
        ),
        agent(
            read(
                "question.show",
                op::QUESTION_READ,
                TargetRule::Own,
                "One question, with the answer once recorded",
                Some(Route::get(
                    "/api/questions/decisions/{id}",
                    &[("id", "decision_id")],
                )),
            ),
            Route::post(
                "/v1/agent/questions/show",
                Body::Versioned,
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["decision_id"],"properties":{"decision_id":{"type":"string","format":"uuid"}}}"#,
                ),
            ),
        ),
        owner(
            "question.list",
            op::QUESTION_READ,
            "Open and answered agent questions",
            "/api/questions/decisions",
        ),
        CommandSpec {
            agent: Some(Route::post(
                "/v1/agent/waits/register",
                Body::Params,
                Params::Ref("crates/aq-lifecycle-api/artifacts/request.schema.json"),
            )),
            ..mutation(
                "wait.register",
                op::WAIT_REGISTER,
                TargetRule::Own,
                "Register an H4 wait for this attempt (a sealed lifecycle request)",
            )
        },
        CommandSpec {
            agent: Some(Route::post(
                "/v1/agent/waits/inspect",
                Body::Params,
                Params::Ref("crates/aq-lifecycle-api/artifacts/request.schema.json"),
            )),
            ..read(
                "wait.inspect",
                op::WAIT_READ,
                TargetRule::Own,
                "Inspect this attempt's wait (a sealed lifecycle request)",
                None,
            )
        },
        CommandSpec {
            agent: Some(Route::post(
                "/v1/agent/waits/ack",
                Body::Params,
                Params::Ref("crates/aq-lifecycle-api/artifacts/request.schema.json"),
            )),
            ..mutation(
                "wait.ack",
                op::WAIT_ACK,
                TargetRule::Own,
                "Acknowledge a continuation (a sealed lifecycle request)",
            )
        },
        agent(
            owner(
                "wait.list",
                op::WAIT_READ,
                "Registered waits (an agent's own attempt's)",
                "/api/waits",
            ),
            Route::post(
                "/v1/agent/waits/list",
                Body::Versioned,
                Params::Inline(r#"{"type":"object","additionalProperties":false}"#),
            ),
        ),
        agent(
            read(
                "wait.show",
                op::WAIT_READ,
                TargetRule::Own,
                "One registered wait (an agent's own attempt's)",
                Some(Route::get("/api/waits/{wait_id}", &[])),
            ),
            Route::post(
                "/v1/agent/waits/show",
                Body::Versioned,
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["wait_id"],"properties":{"wait_id":{"type":"string","format":"uuid"}}}"#,
                ),
            ),
        ),
        CommandSpec {
            owner: Some(Route::post(
                "/api/messages/send",
                Body::Params,
                Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageSend"),
            )),
            agent: Some(Route::post(
                "/v1/agent/messages/send",
                Body::Params,
                Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageSend"),
            )),
            ..mutation(
                "message.send",
                op::MESSAGE_SEND,
                TargetRule::Mailbox("recipient"),
                "Send mail to a task, session or supervisor mailbox",
            )
        },
        CommandSpec {
            owner: Some(Route::post(
                "/api/messages/inbox",
                Body::Params,
                Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageInbox"),
            )),
            agent: Some(Route::post(
                "/v1/agent/messages/inbox",
                Body::Params,
                Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageInbox"),
            )),
            ..read(
                "message.inbox",
                op::MESSAGE_READ,
                TargetRule::Mailbox("recipient"),
                "A mailbox page",
                None,
            )
        },
        CommandSpec {
            agent: Some(Route::post(
                "/v1/agent/messages/body",
                Body::Params,
                Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageBodyRequest"),
            )),
            ..read(
                "message.body",
                op::MESSAGE_READ,
                TargetRule::Mailbox("recipient"),
                "One message body, in bounded chunks",
                None,
            )
        },
        CommandSpec {
            agent: Some(Route::post(
                "/v1/agent/messages/ack",
                Body::Params,
                Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageAck"),
            )),
            ..mutation(
                "message.ack",
                op::MESSAGE_ACK,
                TargetRule::Mailbox("recipient"),
                "Acknowledge a message as received or read",
            )
        },
        owner(
            "conversation.global",
            op::MESSAGE_READ,
            "The supervisor conversation",
            "/api/human/conversations/global",
        ),
        read(
            "conversation.messages",
            op::MESSAGE_READ,
            TargetRule::Own,
            "A conversation's messages",
            Some(Route::get(
                "/api/human/conversations/{conversation}/messages",
                &[("conversation", "conversation_id")],
            )),
        ),
        read(
            "conversation.read_marker",
            op::MESSAGE_READ,
            TargetRule::Own,
            "A conversation's read marker",
            Some(Route::get(
                "/api/human/conversations/{conversation}/read-marker",
                &[("conversation", "conversation_id")],
            )),
        ),
        read(
            "conversation.operation",
            op::MESSAGE_READ,
            TargetRule::Own,
            "A conversation send operation",
            Some(Route::get(
                "/api/human/conversations/{conversation}/operations/{operation}",
                &[
                    ("conversation", "conversation_id"),
                    ("operation", "operation_id"),
                ],
            )),
        ),
        owner(
            "result.list",
            op::RESULT_READ,
            "Results",
            "/api/human/results",
        ),
        read(
            "result.show",
            op::RESULT_READ,
            TargetRule::Own,
            "One result revision",
            Some(Route::get(
                "/api/human/results/{id}/revisions/{revision}",
                &[("id", "result_id")],
            )),
        ),
        read(
            "result.media.list",
            op::RESULT_READ,
            TargetRule::Own,
            "Media attached to a result revision",
            Some(Route::get(
                "/api/human/results/{report}/revisions/{revision}/media",
                &[("report", "result_id")],
            )),
        ),
        owner(
            "result.feed",
            op::RESULT_READ,
            "The results feed",
            "/api/v1/results",
        ),
        read(
            "result.detail",
            op::RESULT_READ,
            TargetRule::Own,
            "One result in the feed",
            Some(Route::get(
                "/api/v1/results/{id}/revisions/{revision}",
                &[("id", "result_id")],
            )),
        ),
        read(
            "result.comments",
            op::RESULT_READ,
            TargetRule::Own,
            "A result's comment threads",
            Some(Route::get(
                "/api/v1/results/{id}/revisions/{revision}/comments",
                &[("id", "result_id")],
            )),
        ),
        read(
            "feedback.task.show",
            op::RESULT_READ,
            TargetRule::Own,
            "A feedback task draft",
            Some(Route::get(
                "/api/v1/feedback-tasks/{id}",
                &[("id", "draft_id")],
            )),
        ),
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
            Some(Route::get("/api/v1/sessions/{id}", &[("id", "session_id")])),
        ),
        read(
            "session.operations",
            "dashboard.read",
            TargetRule::Own,
            "A session's operations",
            Some(Route::get(
                "/api/v1/sessions/{id}/operations",
                &[("id", "session_id")],
            )),
        ),
        read(
            "session.events",
            "dashboard.read",
            TargetRule::Own,
            "A session's observed events",
            Some(Route::get(
                "/api/observations/sessions/{attempt}/events",
                &[("attempt", "attempt_id")],
            )),
        ),
        read(
            "session.usage",
            "dashboard.read",
            TargetRule::Own,
            "A session's observed usage",
            Some(Route::get(
                "/api/observations/sessions/{attempt}/usage",
                &[("attempt", "attempt_id")],
            )),
        ),
        read(
            "terminal.show",
            "dashboard.read",
            TargetRule::Own,
            "A terminal",
            Some(Route::get(
                "/api/v1/terminals/{terminal}",
                &[("terminal", "terminal_id")],
            )),
        ),
        read(
            "terminal.output",
            "dashboard.read",
            TargetRule::Own,
            "A terminal's output",
            Some(Route::get(
                "/api/v1/terminals/{terminal}/output",
                &[("terminal", "terminal_id")],
            )),
        ),
        read(
            "terminal.tail",
            "dashboard.read",
            TargetRule::Own,
            "A terminal's tail",
            Some(Route::get(
                "/api/v1/terminals/{terminal}/tail",
                &[("terminal", "terminal_id")],
            )),
        ),
        read(
            "terminal.receipt",
            "dashboard.read",
            TargetRule::Own,
            "A terminal mutation receipt",
            Some(Route::get(
                "/api/v1/terminals/{terminal}/receipts/{operation}",
                &[("terminal", "terminal_id"), ("operation", "operation_id")],
            )),
        ),
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
            Some(Route::get(
                "/api/notifications/{message}",
                &[("message", "message_id")],
            )),
        ),
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
            Some(Route::get(
                "/api/reviews/decisions/{id}",
                &[("id", "decision_id")],
            )),
        ),
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
            Some(Route::get(
                "/api/playbooks/inspection/runs/{id}",
                &[("id", "run_id")],
            )),
        ),
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
            Some(Route::get("/api/projects/{id}", &[("id", "project_id")])),
        ),
        read(
            "project.revision.show",
            "dashboard.read",
            P,
            "A project's binding as it was at an earlier revision",
            Some(Route::get(
                "/api/projects/{id}/revisions/{revision}",
                &[("id", "project_id")],
            )),
        ),
        owner(
            "conversation.project",
            op::MESSAGE_READ,
            "A project's supervisor conversation",
            "/api/human/conversations/projects/{project}",
        ),
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
            Some(Route::get(
                "/api/execution/attempts/{id}",
                &[("id", "attempt_id")],
            )),
        ),
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
            "Intelligence settings: profiles, the projects using each, and model choices",
            "/api/settings/intelligence",
        ),
        owner(
            "intelligence.registry.show",
            "dashboard.read",
            "The intelligence registry: credentials and recent observations",
            "/api/intelligence/registry",
        ),
        owner(
            "profile.catalog.active",
            "dashboard.read",
            "The active profile catalog's digest and revision",
            "/api/profiles/catalogs",
        ),
        owner(
            "profile.catalog.show",
            "dashboard.read",
            "One profile catalog by digest",
            "/api/profiles/catalogs/{digest}",
        ),
    ];
    for command in &mut commands {
        let path = match command.name {
            "task.list" => "/v1/supervisor/read/api/human/projects/{project}/tasks",
            "task.show" => "/v1/supervisor/read/api/human/projects/{project}/tasks/{node}",
            "task.graph.show" => {
                "/v1/supervisor/read/api/human/projects/{project}/tasks/{node}/graph"
            }
            "task.work" => "/v1/supervisor/read/api/human/projects/{project}/tasks/{node}/work",
            "task.history" => {
                "/v1/supervisor/read/api/human/projects/{project}/tasks/{node}/history"
            }
            "task.summary" => {
                "/v1/supervisor/read/api/human/projects/{project}/tasks/{node}/summary"
            }
            "comment.list" => {
                "/v1/supervisor/read/api/human/projects/{project}/tasks/{node}/comments"
            }
            "project.list" => "/v1/supervisor/projects",
            "knowledge.list" => "/v1/supervisor/read/api/human/projects/{project}/knowledge",
            "knowledge.show" => "/v1/supervisor/read/api/human/projects/{project}/knowledge/{node}",
            _ => continue,
        };
        if let Some(owner) = command.owner {
            command.supervisor = Some(Route { path, ..owner });
        }
    }
    commands
}
