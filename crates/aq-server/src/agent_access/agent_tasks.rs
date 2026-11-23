//! Spec 0006 phase 2: an agent creates a task (`task.create`, agent route
//! `/v1/agent/tasks`). The grant, scope and depth are checked as on the
//! command surface; the per-attempt quota is reserved in one transaction;
//! the create itself is the supervisor's single-task Graph action under the
//! attempt's own journal identity, so idempotency and receipts are the same.
use super::grant_check::{AgentCaller, DISPATCHED, audit};
use super::*;
use crate::commands::refuse;
use aq_agent_grants::{AuditEntry, Decision, Refusal, RefusalCode};
use aq_command_registry::agent_task::AgentTask as Body;

pub(super) fn machine_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/agent/task-actions",
            post(super::draft_tasks::submit).layer(axum::extract::DefaultBodyLimit::max(65536)),
        )
        .route("/v1/agent/tasks", post(create))
}
fn internal<E>(_: E) -> Refusal {
    Refusal::error(
        RefusalCode::Internal,
        "task.create",
        "Task creation is unavailable.",
    )
}
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Response {
    let request_id = Uuid::new_v4();
    let a = match super::grant_check::agent_caller(&state, &headers).await {
        Ok(a) => a,
        Err(code) => {
            return refuse(
                Refusal::error(code, "task.create", "The caller is not authenticated."),
                request_id,
            );
        }
    };
    let Ok(raw) = serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes) else {
        return Error("invalid_request").respond(request_id);
    };
    let Ok(b) = serde_json::from_value::<Body>(Value::Object(raw.clone())) else {
        return Error("invalid_request").respond(request_id);
    };
    if b.api_version != 1 {
        return Error("unsupported_api_version").respond(request_id);
    }
    let submission = Submission {
        state: &state,
        caller: &a,
        headers: &headers,
        body: &b,
        request_id,
    };
    match submission.admit(raw).await {
        Ok(response) => response,
        Err(r) => refuse(r, request_id),
    }
}
struct Submission<'a> {
    state: &'a AppState,
    caller: &'a AgentCaller,
    headers: &'a HeaderMap,
    body: &'a Body,
    request_id: Uuid,
}
impl Submission<'_> {
    async fn admit(
        &self,
        mut params: serde_json::Map<String, Value>,
    ) -> std::result::Result<Response, Refusal> {
        let spec = crate::commands::registry()
            .get("task.create")
            .expect("registered");
        let (decision, target) = self.caller.authorize(self.state, spec, &mut params).await;
        let grant = aq_command_registry::required_grant(spec, &target).unwrap_or_default();
        let checked = aq_agent_grants::GrantRequest::new(spec.name, grant, target);
        if DISPATCHED.try_with(|_| ()).is_err() {
            self.note(checked, &decision).await;
        }
        let held = match decision {
            Decision::Allow(held) => held,
            Decision::Deny(r) => return Err(r),
        };
        self.check_role(checked).await?;
        let (project, own) = (self.caller.ids.project_id, self.caller.ids.task_id);
        if held.limits.draft_only {
            return self.admit_draft(spec).await;
        }
        self.reserve(checked, &held).await?;
        let revision = project_revision(self.state, project, self.body).await?;
        let request =
            serde_json::from_value(self.body.action(project, own, &revision)).map_err(|_| {
                Refusal::error(
                    RefusalCode::InvalidRequest,
                    "task.create",
                    "The task is malformed.",
                )
            })?;
        match super::human::enqueue::supervisor::agent_action(
            self.state,
            self.caller.ids,
            request,
            Default::default(),
            None,
            crate::dashboard_api::actions::AgentAdmission {
                caller: self.caller,
                headers: self.headers,
                reservation: None,
            },
        )
        .await
        {
            Err(e) if e.status() == axum::http::StatusCode::FORBIDDEN => {
                let r = Refusal::parent_outside_project(
                    spec.name,
                    grant,
                    self.caller.grants.related(grant),
                );
                self.note(checked, &Decision::Deny(r.clone())).await;
                Err(r)
            }
            result => Ok(result.unwrap_or_else(axum::response::IntoResponse::into_response)),
        }
    }
    async fn admit_draft(
        &self,
        spec: &aq_command_registry::CommandSpec,
    ) -> std::result::Result<Response, Refusal> {
        let (project, own) = (self.caller.ids.project_id, self.caller.ids.task_id);
        let mut task = self.body.action(
            project,
            own,
            &project_revision(self.state, project, self.body).await?,
        );
        if self.body.task_state.is_none() {
            task["payload"]["task_state"] = json!("draft");
        }
        let request = serde_json::from_value(task).map_err(internal)?;
        super::draft_tasks::admit(
            self.state,
            self.caller,
            self.headers,
            spec,
            request,
            self.request_id,
        )
        .await
    }
    async fn note(&self, checked: aq_agent_grants::GrantRequest<'_>, decision: &Decision) {
        let entry = AuditEntry::agent(self.caller.ids, checked, decision, self.request_id);
        audit(self.state, &entry).await;
    }
    async fn check_role(
        &self,
        checked: aq_agent_grants::GrantRequest<'_>,
    ) -> std::result::Result<(), Refusal> {
        if self.body.sets_hint() {
            let r = Refusal::scope_denied(
                checked.required,
                self.caller.grants.related(checked.required.operation),
            );
            let r = Refusal {
                message: "An agent cannot set intelligence_hint on a task it creates.".into(),
                ..r
            };
            self.note(checked, &Decision::Deny(r.clone())).await;
            return Err(r);
        }
        // Before the quota is reserved: a role that does not exist is refused
        // with the roles that do (spec 0014 §5.5).
        if let Err(unknown) = crate::role_settings::named(self.state, self.body.role.as_slice())
            .await
            .map_err(internal)?
        {
            let message = unknown.to_string();
            return Err(Refusal::error(
                RefusalCode::InvalidRequest,
                checked.required.command,
                &message,
            ));
        }
        let project = self.caller.ids.project_id;
        // An agent may not hand out more than it holds: the role of the new task
        // (named, or the project's default) stays within the filer's own role.
        let filer = self.caller.ids.attempt_id;
        let named = self.body.role.as_deref();
        if let Err(message) = crate::role_settings::within_filer(self.state, filer, project, named)
            .await
            .map_err(internal)?
        {
            let r = Refusal::error(
                RefusalCode::InvalidRequest,
                checked.required.command,
                &message,
            );
            self.note(checked, &Decision::Deny(r.clone())).await;
            return Err(r);
        }
        Ok(())
    }
    async fn reserve(
        &self,
        checked: aq_agent_grants::GrantRequest<'_>,
        held: &aq_agent_grants::Grant,
    ) -> std::result::Result<(), Refusal> {
        use aq_agent_grants::store::{SingleReservation, reserve_single};
        let ids = self.caller.ids;
        let row = SingleReservation {
            ids,
            operation: self.body.operation_id,
            parent: self.body.parent(ids.task_id),
            target: checked.target,
        };
        let mut tx = self.state.store.pool.begin().await.map_err(internal)?;
        let reserved = reserve_single(&mut tx, row, held.limits.max_created_tasks, || {
            now(self.state) as i64
        })
        .await
        .map_err(internal)?;
        if !reserved {
            let r = Refusal::quota_exceeded(
                checked.required.command,
                checked.required.operation,
                self.caller.grants.related(checked.required.operation),
                "max_created_tasks",
            );
            self.note(checked, &Decision::Deny(r.clone())).await;
            return Err(r);
        }
        tx.commit().await.map_err(internal)?;
        Ok(())
    }
}
async fn project_revision(
    state: &AppState,
    project: Uuid,
    b: &Body,
) -> std::result::Result<String, Refusal> {
    Ok(match &b.expected_project_revision {
        Some(r) => r.clone(),
        None => crate::project_bindings::current(state, project)
            .await
            .map_err(internal)?
            .revision
            .to_string(),
    })
}
