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
/// An owner mutation served by its own REST route (not an action kind):
/// the params fill the path and the rest is the JSON body. The route keeps
/// its own idempotency, keyed by the `operation_id` in its body.
const fn owner_post(
    name: &'static str,
    grant: &'static str,
    summary: &'static str,
    path: &'static str,
    rename: &'static [(&'static str, &'static str)],
    params: &'static str,
) -> CommandSpec {
    let mut c = mutation(name, grant, TargetRule::Own, summary);
    c.owner = Some(Route {
        method: "POST",
        path,
        body: Body::Params,
        params: Params::Inline(params),
        rename,
    });
    c
}
/// [`owner_post`] for a route that is a `PUT` or a `DELETE`.
const fn with_method(mut c: CommandSpec, method: &'static str) -> CommandSpec {
    if let Some(mut route) = c.owner {
        route.method = method;
        c.owner = Some(route);
    }
    c
}
const fn get(path: &'static str) -> Option<Route> {
    Some(Route::get(path, &[]))
}

const AGENT_TASK: &str = r#"{"type":"object","additionalProperties":false,"required":["operation_id","title"],"properties":{"operation_id":{"type":"string","format":"uuid"},"title":{"type":"string","minLength":1,"maxLength":512},"body":{"type":"string","maxLength":32768},"acceptance_criteria":{"type":"string","maxLength":8192},"labels":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128}},"parent_id":{"anyOf":[{"type":"string","format":"uuid"},{"const":"self"},{"type":"null"}]},"priority":{"type":"integer","minimum":1,"maximum":300},"task_type":{"type":"string","minLength":1,"maxLength":128},"task_state":{"enum":["active","draft"]},"expected_project_revision":{"type":"string","pattern":"^[1-9][0-9]{0,18}$"},"role":{"type":"string","minLength":1,"maxLength":128,"description":"The role the task expects; it must exist. Absent: the project's default role"},"intelligence_hint":{"type":"null","description":"An agent cannot choose the intelligence of a task it creates"}}}"#;
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
        // A reviewing agent's project-scoped read of retained results: it needs
        // `result.read@project:self` (not the owner's session) and answers from
        // the same validated owner reads as `result.list` and `result.show`,
        // beside where the retained work (the exact diff) is kept.
        agent(
            read(
                "result.review.list",
                op::RESULT_READ,
                P,
                "Retained results of the attempt's project; a report that cannot be read is listed as unavailable and the page says it is incomplete",
                None,
            ),
            Route::post(
                "/v1/agent/results/list",
                Body::Versioned,
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"properties":{"project_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's project; another is refused"},"after":{"type":"string","format":"uuid","description":"The last report of the previous page"},"limit":{"type":"integer","minimum":1,"maximum":25}}}"#,
                ),
            ),
        ),
        agent(
            read(
                "result.review.show",
                op::RESULT_READ,
                P,
                "One retained result revision of the attempt's project, with where its work is retained (ref, base, head, provenance digest) and how to read the exact diff",
                None,
            ),
            Route::post(
                "/v1/agent/results/show",
                Body::Versioned,
                Params::Inline(
                    r#"{"type":"object","additionalProperties":false,"required":["report_id","revision"],"properties":{"project_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's project; another is refused"},"report_id":{"type":"string","format":"uuid"},"revision":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"The exact report digest"}}}"#,
                ),
            ),
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
            "The intelligence profiles (levels): each document with its candidates",
            "/api/settings/intelligence-profiles",
        ),
        owner(
            "intelligence.registry.show",
            "dashboard.read",
            "The intelligence registry: credentials and recent observations",
            "/api/intelligence/registry",
        ),
        owner_post(
            "project.set",
            "project.admin",
            "Create a project or replace its binding (repository, profiles, limits, publication)",
            "/api/projects/{id}",
            &[("id", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["project_id","operation_id","expected_revision","binding"],"properties":{"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":0,"description":"The project's current revision; 0 creates it"},"binding":{"type":"object","description":"The whole binding, as project.show returns it; its project_id must be this project"}}}"#,
        ),
        owner_post(
            "project.disable",
            "project.admin",
            "Disable a project so no new work starts in it",
            "/api/projects/{id}/disable",
            &[("id", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["project_id","operation_id","expected_revision"],"properties":{"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":1}}}"#,
        ),
        owner_post(
            "project.onboard",
            "project.admin",
            "Onboard a GitHub repository as a project (resumes when repeated with the same operation ID)",
            "/api/repositories/onboard",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation_id","project_id","repository_id","github","transport","publication","template"],"properties":{"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"project_id":{"type":"string","format":"uuid"},"repository_id":{"type":"string","format":"uuid"},"github":{"type":"object","additionalProperties":false,"required":["owner","name"],"properties":{"owner":{"type":"string","minLength":1,"maxLength":100},"name":{"type":"string","minLength":1,"maxLength":100}}},"transport":{"enum":["ssh","https"]},"credential_profile":{"type":"string","description":"A configured credential profile of the transport's kind"},"default_branch":{"type":"string"},"publication":{"type":"object","additionalProperties":false,"required":["mode"],"properties":{"mode":{"enum":["pull_request","fast_forward"]},"wait_for_checks":{"type":"object","additionalProperties":false,"required":["timeout_secs"],"properties":{"timeout_secs":{"type":"integer","minimum":0}}}}},"template":{"type":"object","description":"The project binding without its project, repository and publication fields"}}}"#,
        ),
        owner_post(
            "attempt.cancel",
            "attempt.control",
            "Ask a running attempt to stop",
            "/api/execution/attempts/{id}/cancel",
            &[("id", "attempt_id")],
            r#"{"type":"object","additionalProperties":false,"required":["attempt_id","operation_id","reason"],"properties":{"attempt_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"reason":{"type":"string","minLength":1,"maxLength":4096}}}"#,
        ),
        owner_post(
            "attempt.abandon",
            "attempt.control",
            "Abandon a stopped, unsettled attempt and release its hold on the task (cannot be undone)",
            "/api/execution/attempts/{id}/disposition",
            &[("id", "attempt_id")],
            r#"{"type":"object","additionalProperties":false,"required":["attempt_id","operation_id","reason"],"properties":{"attempt_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"reason":{"type":"string","minLength":1,"maxLength":4096}}}"#,
        ),
        owner_post(
            "supervisor.set",
            "supervisor.control",
            "Turn a supervisor on or off and choose the profile it runs on",
            "/api/supervisors/{id}/control",
            &[("id", "conversation_id")],
            r#"{"type":"object","additionalProperties":false,"required":["conversation_id","operation_id","expected_revision","enabled"],"properties":{"conversation_id":{"type":"string","format":"uuid","description":"The supervisor's conversation, as supervisor.list shows it"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":0},"enabled":{"type":"boolean"},"intelligence_profile":{"type":"object","additionalProperties":false,"required":["profile_id","candidate_id"],"description":"A level and one of its candidates","properties":{"profile_id":{"type":"string","description":"The level document's id"},"candidate_id":{"type":"string","description":"provider:model:reasoning, as the list shows it"}}}}}"#,
        ),
        owner_post(
            "notification.retry",
            "notification.admin",
            "Retry a notification delivery whose outcome is unknown, accepting a possible duplicate",
            "/api/notifications/retry",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation_id","delivery_id","expected_revision","accept_duplicate_risk"],"properties":{"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"delivery_id":{"type":"string","format":"uuid"},"expected_revision":{"type":"integer","minimum":0},"accept_duplicate_risk":{"const":true}}}"#,
        ),
        owner_post(
            "session.manual.start",
            "worker.admin",
            "Start a session by hand on a worker for a project",
            "/api/manual-agents/workers/{node}/projects/{project}",
            &[("node", "node_id"), ("project", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["node_id","project_id","operation_id"],"properties":{"node_id":{"type":"string","format":"uuid"},"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Becomes the session's ID; repeat it to retry safely"},"intelligence_profile":{"type":"object","additionalProperties":false,"required":["profile_id","candidate_id"],"description":"A level and one of its candidates","properties":{"profile_id":{"type":"string","description":"The level document's id"},"candidate_id":{"type":"string","description":"provider:model:reasoning, as the list shows it"}}}}}"#,
        ),
        owner_post(
            "session.manual.stop",
            "worker.admin",
            "Stop a session started by hand, keeping its workspace",
            "/api/manual-agents/{id}/stop",
            &[("id", "session_id")],
            r#"{"type":"object","additionalProperties":false,"required":["session_id","operation_id"],"properties":{"session_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"}}}"#,
        ),
        owner_post(
            "session.manual.slots.set",
            "worker.admin",
            "Set how many hand-started sessions a worker offers a project",
            "/api/manual-agents/workers/{node}/projects/{project}/slots",
            &[("node", "node_id"), ("project", "project_id")],
            r#"{"type":"object","additionalProperties":false,"required":["node_id","project_id","operation_id","expected_revision","slots"],"properties":{"node_id":{"type":"string","format":"uuid"},"project_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"integer","minimum":0},"slots":{"type":"integer","minimum":0,"maximum":8}}}"#,
        ),
        owner_post(
            "wait.suspend",
            "attempt.control",
            "Suspend the attempt behind a wait (repeating returns the same directive)",
            "/api/waits/{wait_id}/suspend",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["wait_id"],"properties":{"wait_id":{"type":"string","format":"uuid"}}}"#,
        ),
        owner_post(
            "sandbox.release",
            "attempt.control",
            "Release the protected disk of a suspended attempt",
            "/api/hibernation/{continuation_id}/disk-release",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["continuation_id"],"properties":{"continuation_id":{"type":"string","format":"uuid"}}}"#,
        ),
        owner_post(
            "operation.recover",
            "operation.recover",
            "Retry the dispatch of a task create whose outcome was uncertain",
            "/api/human/operations/{operation}/recover",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation"],"properties":{"operation":{"type":"string","format":"uuid","description":"The operation ID of the original create"}}}"#,
        ),
        owner_post(
            "worker.enroll",
            "worker.admin",
            "Create a one-time enrollment code for a new worker (each call makes a new code)",
            "/api/enrollments",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":[],"properties":{}}"#,
        ),
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
            Some(Route::get(
                "/api/playbooks/sources/{id}",
                &[("id", "source_id")],
            )),
        ),
        owner_post(
            "playbook.source.register",
            "playbook.author",
            "Register a playbook source from a Markdown document and start authoring it (refused if the source exists)",
            "/api/playbooks/sources",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["id","binding","purpose","markdown","commands","events"],"properties":{"id":{"type":"string","minLength":1,"maxLength":128,"description":"The new source's ID; no '/' or control characters"},"binding":{"type":"string","minLength":1,"maxLength":128,"description":"The binding its playbook is activated under"},"purpose":{"type":"string","minLength":1,"maxLength":1024,"description":"What the playbook is for"},"markdown":{"type":"string","description":"The document the playbook is authored from"},"commands":{"type":"array","minItems":1,"uniqueItems":true,"items":{"type":"string"},"description":"Host catalog commands the playbook may call; its abilities follow from these"},"events":{"type":"array","uniqueItems":true,"items":{"type":"string"},"description":"Host catalog events the playbook may react to"}}}"#,
        ),
        with_method(
            owner_post(
                "playbook.source.edit",
                "playbook.author",
                "Replace a registered source's document at an expected revision and start authoring it",
                "/api/playbooks/sources/{id}/markdown",
                &[("id", "source_id")],
                r#"{"type":"object","additionalProperties":false,"required":["source_id","markdown","expected_revision"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"},"markdown":{"type":"string"},"expected_revision":{"type":"integer","minimum":0,"description":"The source's revision as last read"}}}"#,
            ),
            "PUT",
        ),
        owner_post(
            "playbook.source.author",
            "playbook.author",
            "Author a source again from its saved document (does nothing if nothing the agent sees has changed)",
            "/api/playbooks/sources/{id}/runs",
            &[("id", "source_id")],
            r#"{"type":"object","additionalProperties":false,"required":["source_id"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"}}}"#,
        ),
        owner_post(
            "playbook.source.rollback",
            "playbook.author",
            "Put a source's previous playbook back in effect and supersede a run in flight (each call goes back one more)",
            "/api/playbooks/sources/{id}/rollback",
            &[("id", "source_id")],
            r#"{"type":"object","additionalProperties":false,"required":["source_id"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"}}}"#,
        ),
        with_method(
            owner_post(
                "playbook.source.remove",
                "playbook.author",
                "Delete a registered source: its playbook stops and its grant is revoked; run history is kept",
                "/api/playbooks/sources/{id}",
                &[("id", "source_id")],
                r#"{"type":"object","additionalProperties":false,"required":["source_id"],"properties":{"source_id":{"type":"string","minLength":1,"maxLength":128,"description":"The playbook source's ID"}}}"#,
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
        owner_post(
            "playbook.event.retry",
            "playbook.run",
            "Deliver a held event again (refused unless the event is held)",
            "/api/playbooks/events/{id}/retry",
            &[("id", "event_id")],
            r#"{"type":"object","additionalProperties":false,"required":["event_id"],"properties":{"event_id":{"type":"string","format":"uuid","description":"The held event's ID, from playbook.status"}}}"#,
        ),
        owner_post(
            "playbook.event.hold",
            "playbook.run",
            "Hold an event that is waiting for delivery (refused if it is held or delivered)",
            "/api/playbooks/events/{id}/hold",
            &[("id", "event_id")],
            r#"{"type":"object","additionalProperties":false,"required":["event_id"],"properties":{"event_id":{"type":"string","format":"uuid","description":"The waiting event's ID"}}}"#,
        ),
        read(
            "playbook.admission.show",
            "dashboard.read",
            TargetRule::Own,
            "One playbook admission: a request to start a run, and what became of it",
            Some(Route::get(
                "/api/playbooks/admissions/{id}",
                &[("id", "request_id")],
            )),
        ),
        owner_post(
            "playbook.admission.recover",
            "playbook.run",
            "Look up an admission's run in the engine and record it (never starts a run; safe to repeat)",
            "/api/playbooks/admissions/{id}/recover",
            &[("id", "request_id")],
            r#"{"type":"object","additionalProperties":false,"required":["request_id"],"properties":{"request_id":{"type":"string","format":"uuid","description":"The admission's request ID"}}}"#,
        ),
        owner_post(
            "playbook.admission.retry",
            "playbook.run",
            "Admit a retained admission again; one whose outcome was unknown may run its work twice (repeating after it is admitted returns the same run)",
            "/api/playbooks/admissions/{id}/retry",
            &[("id", "request_id")],
            r#"{"type":"object","additionalProperties":false,"required":["request_id"],"properties":{"request_id":{"type":"string","format":"uuid","description":"The admission's request ID"}}}"#,
        ),
        owner_post(
            "playbook.driver.recover",
            "playbook.run",
            "Have the engine recover its runs now (safe to repeat)",
            "/api/playbooks/recover",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":[],"properties":{}}"#,
        ),
        owner_post(
            "playbook.driver.stop",
            "playbook.run",
            "Stop the playbook driver; queued messages are kept (safe to repeat, and allowed during maintenance)",
            "/api/playbooks/stop",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":[],"properties":{}}"#,
        ),
        owner(
            "internal.settings.show",
            "dashboard.read",
            "Which Intelligence profile internal work runs on, by default and per purpose",
            "/api/settings/internal-work",
        ),
        owner_post(
            "internal.settings.set",
            "intelligence.admin",
            "Choose the Intelligence profile internal work runs on, by default and per purpose",
            "/api/settings/internal-work",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["operation_id","expected_revision","overrides"],"properties":{"operation_id":{"type":"string","format":"uuid","description":"Your ID for this change; repeat it to retry safely"},"expected_revision":{"type":"string","pattern":"^[0-9]{1,18}$","description":"The settings revision as last read"},"default_profile_id":{"type":["string","null"],"maxLength":128},"overrides":{"type":"object","description":"Purpose to profile ID","propertyNames":{"enum":["routing_classification","playbook_compilation","playbook_call","decision_revision","result_processing"]},"additionalProperties":{"type":"string","minLength":1,"maxLength":128}}}}"#,
        ),
        read(
            "internal.settings.operation.show",
            "dashboard.read",
            TargetRule::Own,
            "The recorded result of one internal.settings.set",
            Some(Route::get(
                "/api/settings/internal-work/operations/{id}",
                &[("id", "operation_id")],
            )),
        ),
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
            Some(Route::get(
                "/api/internal-work/runs/{id}",
                &[("id", "run_id")],
            )),
        ),
        owner_post(
            "internal.run.retry",
            "intelligence.admin",
            "Run a failed internal work run again as a new run (each call starts another)",
            "/api/internal-work/runs/{id}/retry",
            &[("id", "run_id")],
            r#"{"type":"object","additionalProperties":false,"required":["run_id"],"properties":{"run_id":{"type":"string","format":"uuid","description":"The internal work run's ID"}}}"#,
        ),
        owner_post(
            "internal.run.escalate",
            "intelligence.admin",
            "Hand a failed internal work run to the owner with a diagnosis",
            "/api/internal-work/runs/{id}/escalate",
            &[("id", "run_id")],
            r#"{"type":"object","additionalProperties":false,"required":["run_id","diagnosis"],"properties":{"run_id":{"type":"string","format":"uuid","description":"The internal work run's ID"},"diagnosis":{"type":"string","minLength":1,"maxLength":4000,"description":"What went wrong, at most 4000 bytes"}}}"#,
        ),
        owner_post(
            "internal.run.resolve",
            "intelligence.admin",
            "Mark an escalated internal work run, and the runs it retried, resolved",
            "/api/internal-work/runs/{id}/resolve",
            &[("id", "run_id")],
            r#"{"type":"object","additionalProperties":false,"required":["run_id"],"properties":{"run_id":{"type":"string","format":"uuid","description":"The internal work run's ID"}}}"#,
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
        owner_post(
            "routing.set",
            "intelligence.admin",
            "Turn automatic routing on or off (on only while a routing playbook is active)",
            "/api/settings/routing",
            &[],
            r#"{"type":"object","additionalProperties":false,"required":["enabled"],"properties":{"enabled":{"type":"boolean"}}}"#,
        ),
        read(
            "task.route.history",
            "dashboard.read",
            TargetRule::Own,
            "Every route a task has had, newest first, with reasons, and why it is unrouted if it is",
            Some(Route::get("/api/tasks/{id}/routes", &[("id", "task_id")])),
        ),
        read(
            "project.onboard.show",
            "dashboard.read",
            TargetRule::Own,
            "The state of one repository onboarding",
            Some(Route::get("/api/repositories/onboard/{operation_id}", &[])),
        ),
        read(
            "attempt.disposition.show",
            "dashboard.read",
            TargetRule::Own,
            "The record of an attempt's abandonment",
            Some(Route::get(
                "/api/execution/attempts/{id}/disposition",
                &[("id", "attempt_id")],
            )),
        ),
        read(
            "supervisor.operation.show",
            "dashboard.read",
            TargetRule::Own,
            "The receipt of one supervisor.set",
            Some(Route::get(
                "/api/supervisors/{id}/operations/{operation}",
                &[("id", "conversation_id"), ("operation", "operation_id")],
            )),
        ),
    ];
    // Every supervisor lists projects here, with full access too.
    for c in commands.iter_mut().filter(|c| c.name == "project.list") {
        c.supervisor = c.owner.map(|owner| Route {
            path: "/v1/supervisor/projects",
            ..owner
        });
    }
    // With full access the registry routes these through the owner bridge.
    #[cfg(not(feature = "supervisor_full_access"))]
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
