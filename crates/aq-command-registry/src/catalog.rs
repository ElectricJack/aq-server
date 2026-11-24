//! The static catalog: reads, agent commands and the non-owner routes of
//! action kinds. Mutations that are `ActionKind`s are added by `Registry::new`.
use super::{Body, CommandSpec, Kind, Params, Route, TargetRule};
use aq_agent_grants::op;

const fn read(
    name: &'static str,
    grant: &'static str,
    target: TargetRule,
    summary: &'static str,
) -> CommandSpec {
    CommandSpec {
        kind: Kind::Read,
        ..mutation(name, grant, target, summary)
    }
}

impl CommandSpec {
    const fn with_owner(mut self, route: Option<Route>) -> Self {
        self.owner = route;
        self
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

/// An owner mutation served by its own REST route. Its body supplies the idempotency key.
const fn owner_write(
    name: &'static str,
    grant: &'static str,
    summary: &'static str,
    route: Route,
) -> CommandSpec {
    mutation(name, grant, TargetRule::Own, summary).with_owner(Some(route))
}

const fn post_route(
    path: &'static str,
    rename: &'static [(&'static str, &'static str)],
    params: &'static str,
) -> Route {
    Route {
        method: "POST",
        path,
        body: Body::Params,
        params: Params::Inline(params),
        rename,
    }
}

/// [`owner_write`] for a route that is a `PUT` or a `DELETE`.
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

const fn owner(
    name: &'static str,
    grant: &'static str,
    summary: &'static str,
    path: &'static str,
) -> CommandSpec {
    read(name, grant, TargetRule::Own, summary).with_owner(get(path))
}
const P: TargetRule = TargetRule::Project("project_id");
const T: TargetRule = TargetRule::Task("task_id");
const TASK: &[(&str, &str)] = &[("project", "project_id"), ("node", "task_id")];
const KNOW: &[(&str, &str)] = &[("project", "project_id"), ("node", "node_id")];
const PROJ: &[(&str, &str)] = &[("project", "project_id")];

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

mod automation;
mod collaboration;
mod dashboard;
mod identity;
mod operations;
mod results;
mod tasks;

pub fn catalog() -> Vec<CommandSpec> {
    let mut commands = [
        identity::COMMANDS,
        tasks::COMMANDS,
        collaboration::COMMANDS,
        results::COMMANDS,
        dashboard::COMMANDS,
        operations::COMMANDS,
        automation::COMMANDS,
    ]
    .concat();
    // Every supervisor lists projects here, with full access too.
    for c in commands.iter_mut().filter(|c| c.name == "project.list") {
        c.supervisor = c.owner.map(|owner| Route {
            path: "/v1/supervisor/projects",
            ..owner
        });
    }
    #[cfg(feature = "project_environments")]
    commands.extend(environments());
    for command in &mut commands {
        let path = match command.name {
            "result.list" => "/v1/supervisor/read/api/human/results",
            "result.feed" => "/v1/supervisor/read/api/v1/results",
            "result.show" => "/v1/supervisor/read/api/human/results/{id}/revisions/{revision}",
            "result.detail" => "/v1/supervisor/read/api/v1/results/{id}/revisions/{revision}",
            "attempt.show" => "/v1/supervisor/read/api/execution/attempts/{id}",
            // Evidence routes use ownership in storage. Other full-access
            // reads retain their existing paths through the owner bridge.
            #[cfg(feature = "supervisor_full_access")]
            _ => continue,
            #[cfg(not(feature = "supervisor_full_access"))]
            name => match name {
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
                "knowledge.show" => {
                    "/v1/supervisor/read/api/human/projects/{project}/knowledge/{node}"
                }
                _ => continue,
            },
        };
        if let Some(owner) = command.owner {
            command.supervisor = Some(Route { path, ..owner });
        }
    }
    commands
}

/// Spec 0017 §6.2: an agent tries a recipe of its project. The node's channel
/// proxy commits and pushes the workspace to the task's branch and adds the
/// pushed `commit` to the body; a request without one (this surface, or an
/// older node) is refused with `workspace_not_pushed`. `commit` is not a
/// param: only the node names it, and `forward` refuses one.
#[cfg(feature = "project_environments")]
fn environments() -> [CommandSpec; 2] {
    let mut queue = mutation(
        "environment.try",
        op::ENVIRONMENT_TRY,
        P,
        "Build an environment recipe from the task's pushed head on a builder and run one command in the result; answers the job id to poll",
    );
    queue.agent = Some(Route::post(
        "/v1/agent/environments/try",
        Body::Versioned,
        Params::Inline(
            r#"{"type":"object","additionalProperties":false,"required":["name"],"properties":{"project_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's project; another is refused"},"name":{"type":"string","pattern":"^[a-z0-9][a-z0-9-]{0,63}$","description":"The recipe .aq/environments/<name>.md"},"run":{"type":"string","minLength":1,"description":"One shell command to run in the built image; at most environment_limits.max_arg_bytes"}}}"#,
        ),
    ));
    let show = agent(
        read(
            "environment.job.show",
            op::ENVIRONMENT_TRY,
            P,
            "One of the project's try jobs: its state (queued, claimed, completed) and, once completed, the builder's result",
        ),
        Route::get("/v1/agent/environments/try/{job_id}", &[]),
    );
    [queue, show]
}
