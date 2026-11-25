//! Tasks, their comment threads and graph queries.
use super::*;

const AGENT_TASK: &str = r#"{"type":"object","additionalProperties":false,"required":["operation_id","title"],"properties":{"operation_id":{"type":"string","format":"uuid"},"title":{"type":"string","minLength":1,"maxLength":512},"body":{"type":"string","maxLength":32768},"acceptance_criteria":{"type":"string","maxLength":8192},"labels":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128}},"parent_id":{"anyOf":[{"type":"string","format":"uuid"},{"const":"self"},{"type":"null"}]},"priority":{"type":"integer","minimum":1,"maximum":300},"task_type":{"type":"string","minLength":1,"maxLength":128},"task_state":{"enum":["active","draft"]},"expected_project_revision":{"type":"string","pattern":"^[1-9][0-9]{0,18}$"},"role":{"type":"string","minLength":1,"maxLength":128,"description":"The role the task expects; it must exist. Absent: the project's default role"},"intelligence_hint":{"type":"null","description":"An agent cannot choose the intelligence of a task it creates"}}}"#;
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

const fn task(name: &'static str, path: &'static str, summary: &'static str) -> CommandSpec {
    read(name, op::TASK_READ, T, summary).with_owner(Some(Route::get(path, TASK)))
}

const fn task_create() -> CommandSpec {
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
    task_create
}

const fn graph_create() -> CommandSpec {
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
    graph_create
}

pub(super) const COMMANDS: &[CommandSpec] = &[
    agent(
        mutation(
            "task.workspace.request",
            op::WORKSPACE_REQUEST,
            TargetRule::Own,
            "Request a sibling project checkout or a private PostgreSQL service for this task; AQ retains its edits before relaunch",
        ),
        Route::post(
            "/v1/agent/workspace/request",
            Body::Params,
            Params::Inline(
                r#"{"type":"object","additionalProperties":false,"properties":{"target_project_id":{"type":"string","format":"uuid"},"access":{"enum":["read","write"]},"service":{"const":"postgres"}},"oneOf":[{"required":["target_project_id","access"],"not":{"required":["service"]}},{"required":["service"],"not":{"anyOf":[{"required":["target_project_id"]},{"required":["access"]}]}}]}"#,
            ),
        ),
    ),
    task_create(),
    graph_create(),
    read("task.list", op::TASK_READ, P, "Tasks in a project").with_owner(Some(Route::get(
        "/api/human/projects/{project}/tasks",
        PROJ,
    ))),
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
    )
    .with_owner(get("/api/human/overview/tasks")),
    agent(
        read(
            "comment.list",
            op::COMMENT_READ,
            T,
            "A task's comment thread, newest first",
        )
        .with_owner(Some(Route::get(
            "/api/human/projects/{project}/tasks/{node}/comments",
            TASK,
        ))),
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
    )
    .with_owner(Some(Route::get(
        "/api/human/projects/{project}/knowledge",
        PROJ,
    ))),
    agent(
        read(
            "knowledge.show",
            op::KNOWLEDGE_READ,
            P,
            "One knowledge record",
        )
        .with_owner(Some(Route::get(
            "/api/human/projects/{project}/knowledge/{node}",
            KNOW,
        ))),
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
        )
        .with_owner(get("/api/v1/search")),
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
];
