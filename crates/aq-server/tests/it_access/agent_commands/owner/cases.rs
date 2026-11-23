use super::*;
pub(super) fn reads(project: &str) -> Vec<(&'static str, String, Value, bool)> {
    vec![
        ("worker.list", "/api/workers".to_string(), json!({}), true),
        ("server.status", "/api/status".to_string(), json!({}), true),
        (
            "project.show",
            format!("/api/projects/{project}"),
            json!({"project_id":project}),
            true,
        ),
        (
            "project.revision.show",
            format!("/api/projects/{project}/revisions/1"),
            json!({"project_id":project,"revision":1}),
            true,
        ),
        (
            "attempt.show",
            format!("/api/execution/attempts/{}", Uuid::nil()),
            json!({"attempt_id":Uuid::nil()}),
            false,
        ),
        (
            "supervisor.list",
            "/api/supervisors".to_string(),
            json!({}),
            false,
        ),
        (
            "session.manual.list",
            format!("/api/manual-agents?project_id={project}"),
            json!({"project_id":project}),
            false,
        ),
        (
            "intelligence.list",
            "/api/settings/intelligence-profiles".to_string(),
            json!({}),
            false,
        ),
        (
            "intelligence.registry.show",
            "/api/intelligence/registry".to_string(),
            json!({}),
            false,
        ),
        (
            "routing.document.show",
            "/api/settings/routing-document".to_string(),
            json!({}),
            false,
        ),
    ]
}
pub(super) fn mutations(project: &str) -> Vec<(&'static str, String, Value)> {
    vec![
        (
            "project.disable",
            format!("/api/projects/{project}/disable"),
            json!({"project_id":project,"expected_revision":999_999}),
        ),
        (
            "attempt.cancel",
            format!("/api/execution/attempts/{}/cancel", Uuid::from_u128(77)),
            json!({"attempt_id":Uuid::from_u128(77),"reason":"test"}),
        ),
        (
            "attempt.abandon",
            format!(
                "/api/execution/attempts/{}/disposition",
                Uuid::from_u128(77)
            ),
            json!({"attempt_id":Uuid::from_u128(77),"reason":"test"}),
        ),
        (
            "supervisor.set",
            format!("/api/supervisors/{}/control", Uuid::from_u128(78)),
            json!({"conversation_id":Uuid::from_u128(78),"expected_revision":1,"enabled":false}),
        ),
        (
            "notification.retry",
            "/api/notifications/retry".to_string(),
            json!({"delivery_id":Uuid::from_u128(79),"expected_revision":1,"accept_duplicate_risk":true}),
        ),
        (
            "session.manual.stop",
            format!("/api/manual-agents/{}/stop", Uuid::from_u128(80)),
            json!({"session_id":Uuid::from_u128(80)}),
        ),
        (
            "wait.suspend",
            format!("/api/waits/{}/suspend", Uuid::from_u128(81)),
            json!({"wait_id":Uuid::from_u128(81)}),
        ),
        (
            "sandbox.release",
            format!("/api/hibernation/{}/disk-release", Uuid::from_u128(82)),
            json!({"continuation_id":Uuid::from_u128(82)}),
        ),
        (
            "operation.recover",
            format!("/api/human/operations/{}/recover", Uuid::from_u128(83)),
            json!({"operation":Uuid::from_u128(83)}),
        ),
    ]
}

pub(super) struct Route {
    pub(super) name: &'static str,
    pub(super) method: &'static str,
    pub(super) path: String,
    pub(super) params: Value,
    pub(super) path_param: Option<&'static str>,
}
impl Route {
    fn new(name: &'static str, method: &'static str, path: String, params: Value) -> Self {
        Self {
            name,
            method,
            path,
            params,
            path_param: None,
        }
    }
    fn with_path(mut self, param: &'static str) -> Self {
        self.path_param = Some(param);
        self
    }
}
pub(super) fn registry() -> Vec<Route> {
    let run = Uuid::from_u128(90);
    let mut routes = sources();
    routes.extend(internal_settings(run));
    routes.extend(internal_runs(run));
    routes.extend(routing(run));
    routes.extend(playbook_controls(run));
    routes
}
fn sources() -> Vec<Route> {
    vec![
        Route::new(
            "playbook.source.list",
            "GET",
            "/api/playbooks/sources".to_string(),
            json!({}),
        ),
        Route::new(
            "playbook.source.show",
            "GET",
            "/api/playbooks/sources/absent".to_string(),
            json!({"source_id":"absent"}),
        )
        .with_path("source_id"),
        Route::new(
            "playbook.source.register",
            "POST",
            "/api/playbooks/sources".to_string(),
            json!({"id":"bad/id","binding":"b","purpose":"p","markdown":"m","commands":["x"],"events":[]}),
        ),
        Route::new(
            "playbook.source.edit",
            "PUT",
            "/api/playbooks/sources/absent/markdown".to_string(),
            json!({"source_id":"absent","markdown":"m","expected_revision":1}),
        )
        .with_path("source_id"),
        Route::new(
            "playbook.source.author",
            "POST",
            "/api/playbooks/sources/absent/runs".to_string(),
            json!({"source_id":"absent"}),
        )
        .with_path("source_id"),
        Route::new(
            "playbook.source.rollback",
            "POST",
            "/api/playbooks/sources/absent/rollback".to_string(),
            json!({"source_id":"absent"}),
        )
        .with_path("source_id"),
        Route::new(
            "playbook.source.remove",
            "DELETE",
            "/api/playbooks/sources/absent".to_string(),
            json!({"source_id":"absent"}),
        )
        .with_path("source_id"),
    ]
}
fn internal_settings(run: Uuid) -> Vec<Route> {
    vec![
        Route::new(
            "internal.settings.show",
            "GET",
            "/api/settings/internal-work".to_string(),
            json!({}),
        ),
        Route::new(
            "internal.settings.set",
            "POST",
            "/api/settings/internal-work".to_string(),
            json!({"operation_id":Uuid::nil(),"expected_revision":"0","expected_catalog_revision":"0","overrides":{}}),
        ),
        Route::new(
            "internal.settings.operation.show",
            "GET",
            format!("/api/settings/internal-work/operations/{run}"),
            json!({"operation_id":run}),
        )
        .with_path("operation_id"),
    ]
}
fn internal_runs(run: Uuid) -> Vec<Route> {
    vec![
        Route::new(
            "internal.run.list",
            "GET",
            "/api/internal-work/runs?status=failed&limit=5".to_string(),
            json!({"status":"failed","limit":5}),
        ),
        Route::new(
            "internal.run.show",
            "GET",
            format!("/api/internal-work/runs/{run}"),
            json!({"run_id":run}),
        )
        .with_path("run_id"),
        Route::new(
            "internal.run.retry",
            "POST",
            format!("/api/internal-work/runs/{run}/retry"),
            json!({"run_id":run}),
        )
        .with_path("run_id"),
        Route::new(
            "internal.run.escalate",
            "POST",
            format!("/api/internal-work/runs/{run}/escalate"),
            json!({"run_id":run,"diagnosis":"d"}),
        )
        .with_path("run_id"),
        Route::new(
            "internal.run.resolve",
            "POST",
            format!("/api/internal-work/runs/{run}/resolve"),
            json!({"run_id":run}),
        )
        .with_path("run_id"),
    ]
}
fn routing(run: Uuid) -> Vec<Route> {
    vec![
        Route::new(
            "routing.show",
            "GET",
            "/api/settings/routing".to_string(),
            json!({}),
        ),
        Route::new(
            "routing.set",
            "POST",
            "/api/settings/routing".to_string(),
            json!({"enabled":false}),
        ),
        Route::new(
            "task.route.history",
            "GET",
            format!("/api/tasks/{run}/routes"),
            json!({"task_id":run}),
        )
        .with_path("task_id"),
    ]
}
fn playbook_controls(run: Uuid) -> Vec<Route> {
    vec![
        // The playbook engine's controls. No driver runs in this fixture, so
        // these are unmounted here; playbook_bridge runs them against one.
        Route::new(
            "playbook.status",
            "GET",
            "/api/playbooks/status".to_string(),
            json!({}),
        ),
        Route::new(
            "playbook.event.retry",
            "POST",
            format!("/api/playbooks/events/{run}/retry"),
            json!({"event_id":run}),
        )
        .with_path("event_id"),
        Route::new(
            "playbook.event.hold",
            "POST",
            format!("/api/playbooks/events/{run}/hold"),
            json!({"event_id":run}),
        )
        .with_path("event_id"),
        Route::new(
            "playbook.admission.show",
            "GET",
            format!("/api/playbooks/admissions/{run}"),
            json!({"request_id":run}),
        )
        .with_path("request_id"),
        Route::new(
            "playbook.admission.recover",
            "POST",
            format!("/api/playbooks/admissions/{run}/recover"),
            json!({"request_id":run}),
        )
        .with_path("request_id"),
        Route::new(
            "playbook.admission.retry",
            "POST",
            format!("/api/playbooks/admissions/{run}/retry"),
            json!({"request_id":run}),
        )
        .with_path("request_id"),
        Route::new(
            "playbook.driver.recover",
            "POST",
            "/api/playbooks/recover".to_string(),
            json!({}),
        ),
        Route::new(
            "playbook.driver.stop",
            "POST",
            "/api/playbooks/stop".to_string(),
            json!({}),
        ),
    ]
}
