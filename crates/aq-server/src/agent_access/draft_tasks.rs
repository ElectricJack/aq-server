//! Scoped task actions for an attempt with draft task grants.
use super::grant_check::{AgentCaller, DISPATCHED, audit};
use super::human::{enqueue, graph_actions};
use super::*;
use crate::dashboard_api::actions::ActionRequest;
use aq_agent_grants::{
    AuditEntry, Decision, Grant, Holder, Refusal, RefusalCode, Relation, Target,
    draft_scope::{MAX_ANCESTRY, TaskScope, batch_paths, uuid},
    store::{Reservation, ReserveError},
};
use aq_command_registry::CommandSpec;
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Response {
    let request_id = Uuid::new_v4();
    let Some(request) = super::scheduling::parse::parse(&bytes)
        .ok()
        .and_then(|value| serde_json::from_value::<ActionRequest>(value).ok())
    else {
        return Error("invalid_request").respond(request_id);
    };
    let Some(spec) = crate::commands::registry()
        .get(&request.kind)
        .filter(|spec| aq_command_registry::is_draft_action(spec.name))
    else {
        return Error("invalid_request").respond(request_id);
    };
    let a = match super::grant_check::agent_caller(&state, &headers).await {
        Ok(a) => a,
        Err(code) => {
            return grant_check::refused(
                &headers,
                Refusal::error(code, spec.name, "The caller is not authenticated."),
                request_id,
            );
        }
    };
    match admit(&state, &a, spec, request, request_id).await {
        Ok(r) => r,
        Err(r) => grant_check::refused(&headers, r, request_id),
    }
}

struct Scope<'a> {
    state: &'a AppState,
    a: &'a AgentCaller,
    spec: &'a CommandSpec,
    grant: Grant,
    task_scope: TaskScope,
    guards: BTreeMap<Uuid, u64>,
}

type Checked<T> = std::result::Result<T, Refusal>;

fn error(spec: &CommandSpec, code: RefusalCode, message: &str) -> Refusal {
    Refusal::error(code, spec.name, message)
}

fn error_for_unavailable(spec: &CommandSpec) -> Refusal {
    error(
        spec,
        RefusalCode::Internal,
        "Draft graph access is unavailable.",
    )
}

impl<'a> Scope<'a> {
    async fn new(
        state: &'a AppState,
        a: &'a AgentCaller,
        spec: &'a CommandSpec,
        grant: Grant,
    ) -> Checked<Self> {
        let workspace = id(&a.binding, "workspace_id").map_err(|_| error_for_unavailable(spec))?;
        let project = crate::project_bindings::current(state, a.ids.project_id)
            .await
            .map_err(|_| error_for_unavailable(spec))?;
        if project.binding.graph_workspace_id != workspace
            || project.binding.graph_adapter_reference != "aq.graph.configured"
        {
            return Err(Refusal::scope_denied(
                spec.name,
                &grant.operation,
                a.grants.related(&grant.operation),
            ));
        }
        let rt = runtime(state).map_err(|_| error_for_unavailable(spec))?;
        let own = graph_actions::current(rt, workspace, a.ids.task_id, "task")
            .await
            .map_err(|_| error_for_unavailable(spec))?;
        let metadata = &own["metadata"];
        let mut subject = None;
        if let (Some(candidate), Some(namespace), Some(key)) = (
            uuid(&metadata["aq:subject"]["id"]),
            uuid(&metadata["aq:playbook_namespace"]),
            metadata["aq:logical_key"].as_str(),
        ) {
            let ensured: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM playbook_work_keys k JOIN playbook_namespaces n USING(namespace_id,generation)
                 WHERE k.namespace_id=$1 AND k.logical_key=$2 AND k.task_id=$3 AND n.project_id=$4 AND n.workspace_id=$5)",
            ).bind(namespace).bind(key).bind(a.ids.task_id).bind(a.ids.project_id).bind(workspace)
                .fetch_one(&state.store.pool).await.map_err(|_| error_for_unavailable(spec))?;
            if ensured && metadata["aq:project_id"] == json!(a.ids.project_id) {
                subject = Some(candidate);
            }
        }
        let version = own["version"]
            .as_u64()
            .ok_or_else(|| error_for_unavailable(spec))?;
        let task_scope = TaskScope {
            workspace,
            project: a.ids.project_id,
            own_task: a.ids.task_id,
            subject_task: subject,
        };
        Ok(Self {
            state,
            a,
            spec,
            grant,
            task_scope,
            guards: BTreeMap::from([(a.ids.task_id, version)]),
        })
    }

    fn denied(&self) -> Refusal {
        Refusal::scope_denied(
            self.spec.name,
            &self.grant.operation,
            self.a.grants.related(&self.grant.operation),
        )
    }

    fn invalid(&self) -> Refusal {
        error(
            self.spec,
            RefusalCode::InvalidRequest,
            "Invalid draft task action.",
        )
    }

    fn quota(&self, limit: &'static str) -> Refusal {
        Refusal::quota_exceeded(
            self.spec.name,
            &self.grant.operation,
            self.a.grants.related(&self.grant.operation),
            limit,
        )
    }

    async fn context(&self, task: Uuid) -> Checked<Value> {
        let rt = runtime(self.state).map_err(|_| self.denied())?;
        let query = [
            ("depth".into(), MAX_ANCESTRY.to_string()),
            ("direction".into(), "incoming".into()),
            ("limit".into(), "200".into()),
            ("max_bytes".into(), "1048576".into()),
        ];
        let value = rt
            .human_get(
                &format!(
                    "/v1/workspaces/{}/nodes/{task}/context",
                    self.task_scope.workspace
                ),
                &query,
                1_048_576,
            )
            .await
            .map_err(|_| self.denied())?;
        if !contract::upstream_valid("Context", &value) || value["node"]["id"] != json!(task) {
            return Err(self.denied());
        }
        Ok(value)
    }

    fn require(&mut self, context: &Value, draft: bool) -> Checked<(Uuid, u32)> {
        let path = self
            .task_scope
            .path(context, draft)
            .ok_or_else(|| self.denied())?;
        let relation = if path.depth == 0 && path.root == self.a.ids.task_id {
            Relation::Own
        } else {
            Relation::Descendant(path.depth)
        };
        let holder = Holder {
            project: self.a.ids.project_id,
        };
        let target = Target::Task {
            project: holder.project,
            relation,
        };
        if let Decision::Deny(r) = aq_agent_grants::check(
            &self.a.grants,
            holder,
            self.spec.name,
            &self.grant.operation,
            target,
        ) {
            return Err(r);
        }
        for (task, version) in path.versions {
            if self
                .guards
                .insert(task, version)
                .is_some_and(|old| old != version)
            {
                return Err(self.denied());
            }
        }
        Ok((path.root, path.depth))
    }

    async fn reserve(&self, operation: Uuid, tasks: Vec<Reservation>) -> Checked<()> {
        aq_agent_grants::store::reserve(
            &self.state.store.pool,
            self.a.ids,
            operation,
            tasks,
            self.grant.limits.max_created_tasks,
            now(self.state) as i64,
        )
        .await
        .map_err(|error| match error {
            ReserveError::QuotaExceeded => self.quota("max_created_tasks"),
            ReserveError::Conflict => self.invalid(),
            ReserveError::Store(_) => error_for_unavailable(self.spec),
        })
    }

    async fn graph(&mut self, graph: &enqueue::GraphCreate) -> Checked<Vec<Reservation>> {
        if !graph_actions::GraphPayload::valid(graph) {
            return Err(self.invalid());
        }
        let tasks: Vec<_> = graph
            .tasks
            .iter()
            .map(|task| (task.id, task.parent_id))
            .collect();
        let paths = batch_paths(&tasks).ok_or_else(|| self.denied())?;
        if graph
            .edges
            .iter()
            .any(|edge| !paths.contains_key(&edge.source) || !paths.contains_key(&edge.target))
        {
            return Err(self.denied());
        }
        let mut parents = BTreeMap::new();
        let mut rows = Vec::new();
        for task in &graph.tasks {
            if !matches!(task.task_state, enqueue::InitialState::Draft)
                || task.intelligence_hint.is_some()
            {
                return Err(self.denied());
            }
            #[cfg(feature = "project_environments")]
            if task.needs.is_some() {
                return Err(self.denied());
            }
            let parent = task.parent_id.ok_or_else(|| self.denied())?;
            let (ancestor, levels) = paths[&task.id];
            let (root, depth) = if let Some(found) = parents.get(&ancestor).copied() {
                found
            } else {
                let context = self.context(ancestor).await?;
                let found = self.require(&context, false)?;
                parents.insert(ancestor, found);
                found
            };
            let depth = depth + levels;
            if self.grant.limits.max_depth.is_some_and(|max| depth > max) {
                return Err(self.quota("max_depth"));
            }
            if self.grant.scope == aq_agent_grants::Scope::TaskSelf {
                return Err(self.denied());
            }
            rows.push(Reservation {
                task: task.id,
                parent,
                root,
                depth,
            });
        }
        for role in graph
            .tasks
            .iter()
            .map(|task| task.role.as_deref())
            .collect::<BTreeSet<_>>()
        {
            if let Err(message) = crate::role_settings::within_filer(
                self.state,
                self.a.ids.attempt_id,
                self.a.ids.project_id,
                role,
            )
            .await
            .map_err(|_| {
                error(
                    self.spec,
                    RefusalCode::Internal,
                    "Role validation is unavailable.",
                )
            })? {
                return Err(error(self.spec, RefusalCode::InvalidRequest, &message));
            }
        }
        Ok(rows)
    }

    async fn edit(&mut self, request: &ActionRequest) -> Checked<()> {
        let task = request.subject.id.parse().map_err(|_| self.invalid())?;
        let context = self.context(task).await?;
        self.require(&context, true)?;
        let blocker = match self.spec.name {
            "task.dependency.add" => {
                Some(uuid(&request.payload["blocker_id"]).ok_or_else(|| self.invalid())?)
            }
            "task.dependency.remove" => {
                let edge_id = uuid(&request.payload["edge_id"]).ok_or_else(|| self.invalid())?;
                Some(
                    self.task_scope
                        .blocker(&context, task, edge_id)
                        .ok_or_else(|| self.denied())?,
                )
            }
            _ => None,
        };
        if let Some(blocker) = blocker {
            let context = self.context(blocker).await?;
            self.require(&context, true)?;
        }
        Ok(())
    }
}

pub(super) async fn admit(
    state: &AppState,
    a: &AgentCaller,
    spec: &CommandSpec,
    request: ActionRequest,
    request_id: Uuid,
) -> Checked<Response> {
    let (decision, target) = a.authorize(state, spec, &mut serde_json::Map::new()).await;
    let grant = spec.grant.unwrap_or_default();
    let entry = |d: &Decision| AuditEntry::agent(a.ids, spec.name, grant, target, d, request_id);
    if DISPATCHED.try_with(|_| ()).is_err() {
        audit(state, &entry(&decision)).await;
    }
    let held = match decision {
        Decision::Allow(held) => held,
        Decision::Deny(r) => return Err(r),
    };
    let result = checked_action(state, a, spec, held, request).await;
    if let Err(refusal) = &result {
        audit(state, &entry(&Decision::Deny(refusal.clone()))).await;
    }
    result
}

async fn checked_action(
    state: &AppState,
    a: &AgentCaller,
    spec: &CommandSpec,
    grant: Grant,
    request: ActionRequest,
) -> Checked<Response> {
    let mut scope = Scope::new(state, a, spec, grant).await?;
    let create = matches!(spec.name, "task.create" | "task.graph.create");
    if request.subject.entity_type != if create { "project" } else { "task" }
        || (create && request.subject.id != a.ids.project_id.to_string())
        || (!create && request.payload["project_id"] != json!(a.ids.project_id))
    {
        return Err(scope.denied());
    }
    if create && !scope.grant.limits.draft_only {
        return Err(scope.denied());
    }
    match enqueue::supervisor::agent_replay(state, a.ids, &request).await {
        Ok(Some(response)) => return Ok(response),
        Ok(None) => {}
        Err(e) => return Ok(e.into_response()),
    }
    match spec.name {
        "task.graph.create" => {
            let graph =
                serde_json::from_value(request.payload.clone()).map_err(|_| scope.invalid())?;
            let rows = scope.graph(&graph).await?;
            if graph.operation_id != request.operation_id
                || graph.project_id != a.ids.project_id
                || !matches!(&request.subject.revision, Some(expected) if *expected == graph.expected_version)
                || request.expected_versions.get("project") != Some(&graph.expected_version)
            {
                return Err(scope.invalid());
            }
            if graph.tasks.len() * 2 + graph.edges.len() + scope.guards.len() > 100 {
                return Err(scope.invalid());
            }
            scope.reserve(request.operation_id, rows).await?;
        }
        "task.create" => {
            if request.payload["task_state"] != "draft" {
                return Err(scope.denied());
            }
            let parent = uuid(&request.payload["parent_id"]).ok_or_else(|| scope.denied())?;
            let context = scope.context(parent).await?;
            let (root, depth) = scope.require(&context, false)?;
            let depth = depth + 1;
            if scope.grant.limits.max_depth.is_some_and(|max| depth > max) {
                return Err(scope.quota("max_depth"));
            }
            if scope.grant.scope == aq_agent_grants::Scope::TaskSelf {
                return Err(scope.denied());
            }
            scope
                .reserve(
                    request.operation_id,
                    vec![Reservation {
                        task: request.operation_id,
                        parent,
                        root,
                        depth,
                    }],
                )
                .await?;
        }
        _ => scope.edit(&request).await?,
    }
    Ok(
        enqueue::supervisor::agent_action(state, a.ids, request, scope.guards)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
