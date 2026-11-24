#![cfg(feature = "supervisor_full_access")]
//! The owner bridge (`/v1/supervisor/owner`) on the machine listener: a
//! supervisor bearer with the `owner` scope reaches the live owner app as the
//! owner who approved the supervisor, and nothing else does.
use super::Fixture;
use crate::dashboard_task_knowledge::{Graph, Subject, action, change_set, graph_runtime, node};
use aq_server::{
    agent_access::supervisor_conversation::reply, config::AuthConfig,
    identity::verifier_from_config, project_bindings::domain,
};
use aq_supervisor_control::{owner::DELEGATED_OWNER, read::ReadOwner};
use axum::{
    Extension, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, Uri},
    routing::any,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PREFIX: &str = "/v1/supervisor/owner";
const SIX: [&str; 6] = [
    "graph.read",
    "graph.write",
    "enqueue",
    "messaging",
    "decisions",
    "settings",
];

struct Owner {
    f: Fixture,
    token: String,
    machine: Router,
    owner: Router,
    project: Uuid,
    /// A second bound project.
    other: Uuid,
    workspace: Uuid,
    graph: Graph,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A global supervisor with a running session and a credential, over a state
/// with a bound project on a fake Graph. A request without the dev header is
/// not the owner, so only the bridge can make a headerless request the owner's.
async fn owner() -> Owner {
    build(false).await
}

/// [`owner`], but the supervisor is the project supervisor of `project`.
async fn project_owner() -> Owner {
    build(true).await
}

async fn build(scoped: bool) -> Owner {
    let mut f = Fixture::new(aq_server::STATE_FORMAT_VERSION).await;
    let (project, workspace) = (Uuid::new_v4(), Uuid::new_v4());
    bind(&f, project, workspace).await;
    if scoped {
        f.scope_to(project).await;
    }
    f.send().await;
    let r = f.reserve().await.reservation.unwrap();
    let p = f.plan(r.id, 1).await;
    f.start(&p).await;
    let token = reply::issue_credential(&f.pool, f.actor, 60_000, 999_000)
        .await
        .unwrap();
    f.state.identity = verifier_from_config(
        &AuthConfig::Dev {
            dev_email: "nobody@localhost".into(),
        },
        reqwest::Client::new(),
    );
    let (graph, server, dir) = graph_runtime(&mut f.state).await;
    let other = Uuid::new_v4();
    bind(&f, other, Uuid::new_v4()).await;
    let owner = aq_server::build_admin_router(f.state.clone());
    let machine = aq_server::build_worker_router_with_owner_app(f.state.clone(), owner.clone());
    Owner {
        f,
        token,
        machine,
        owner,
        project,
        other,
        workspace,
        graph,
        server,
        _dir: dir,
    }
}

/// A project bound to `workspace` on the configured Graph adapter.
async fn bind(f: &Fixture, project: Uuid, workspace: Uuid) {
    let repository = Uuid::new_v4();
    let binding: domain::ProjectBinding = serde_json::from_value(json!({"schema_version":1,
        "project_id":project,"name":"Owner bridge","enabled":true,"repository_id":repository,
        "repository_binding_generation":1,"repository_adapter_reference":"aq.git.configured",
        "graph_adapter_reference":"aq.graph.configured","graph_workspace_id":workspace,
        "allowed_node_ids":[],"allowed_worker_groups":[],"allowed_environments":["confined-worktree"],
        "policy":{"identity":"policy","revision":"1","digest":"b".repeat(64)},"context":[],
        "ceilings":{"permissions":[],"context_bytes":8192,"wall_time_ms":60000,"memory_bytes":268435456,
        "cpu_millis":60000,"process_limit":8,"cost_limit_microunits":null,"currency":null,
        "require_hard_cost_enforcement":false}}))
    .unwrap();
    let observed = domain::BindingObservation {
        repository_id: repository,
        graph_workspace_id: workspace,
        repository_adapter_reference: "aq.git.configured".into(),
        graph_adapter_reference: "aq.graph.configured".into(),
        repository: domain::ExternalStatus::Verified,
        graph: domain::ExternalStatus::Verified,
        repository_generation: Some(1),
        observed_at_ms: 1_000_000,
        expires_at_ms: 1_030_000,
    };
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1000000,$3)")
        .bind(project)
        .bind(json!(binding))
        .bind(json!(observed))
        .execute(&f.pool)
        .await
        .unwrap();
}

enum As<'a> {
    Supervisor(&'a str),
    /// The dev-mode owner browser, with or without the CSRF header.
    Owner {
        csrf: bool,
    },
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    who: As<'_>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    request = match who {
        As::Supervisor(token) => request.header("authorization", format!("Bearer {token}")),
        As::Owner { csrf } => {
            let request = request.header("x-aq-dev-email", "owner@localhost");
            if csrf {
                request.header("x-aq-request", "1")
            } else {
                request
            }
        }
    };
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

impl Owner {
    async fn supervisor(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let path = format!("{PREFIX}{path}");
        call(
            &self.machine,
            method,
            &path,
            As::Supervisor(&self.token),
            body,
        )
        .await
    }
    /// A draft task at version 3, and Graph's answer to activating it.
    fn draft(&self) -> (Uuid, Value) {
        let id = Uuid::new_v4();
        let mut draft = node(id, self.workspace, "task", 3);
        draft["task_state"] = json!("draft");
        self.graph.nodes.lock().unwrap().insert(id, draft.clone());
        let mut active = draft;
        active["version"] = json!(4);
        active["task_state"] = json!("active");
        *self.graph.reply.lock().unwrap() = Some((200, change_set(vec![active])));
        let payload =
            json!({"operation_id":Uuid::new_v4(),"project_id":self.project,"expected_version":"3"});
        (id, action("task.activate", Subject::Task(id, "3"), payload))
    }
}

fn owner_path(name: &str) -> &'static str {
    aq_server::commands::registry()
        .get(name)
        .and_then(|c| c.owner.as_ref())
        .unwrap_or_else(|| panic!("{name} has an owner route"))
        .path
}

#[tokio::test]
async fn supervisor_activates_a_draft_through_the_owner_mount() {
    let o = owner().await;
    let (id, activate) = o.draft();
    let (status, out) = o
        .supervisor("POST", "/api/v1/actions", Some(activate))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["disposition"], "confirmed", "{out}");
    // Re-read as the owner on the admin listener.
    let path = format!("/api/human/projects/{}/tasks/{id}", o.project);
    let (status, task) = call(&o.owner, "GET", &path, As::Owner { csrf: false }, None).await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(task["data"]["task_state"], "active", "{task}");
}

#[tokio::test]
async fn supervisor_reads_swarm_and_supervisor_list_through_the_owner_mount() {
    let o = owner().await;
    for name in ["dashboard.swarm", "supervisor.list"] {
        let path = owner_path(name);
        let (status, out) = o.supervisor("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{name} {path}: {out}");
        // Without the bridge the same headerless request is not the owner.
        let (status, _) = call(&o.owner, "GET", path, As::Supervisor(&o.token), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{name} on the admin listener"
        );
    }
}

#[tokio::test]
async fn owner_mount_refuses_without_bearer_even_with_dev_owner_header() {
    let o = owner().await;
    let path = format!("{PREFIX}{}", owner_path("supervisor.list"));
    let (status, out) = call(&o.machine, "GET", &path, As::Owner { csrf: true }, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{out}");
    let (_, activate) = o.draft();
    let (status, _) = call(
        &o.machine,
        "POST",
        &format!("{PREFIX}/api/v1/actions"),
        As::Owner { csrf: true },
        Some(activate),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(o.graph.posts.lock().unwrap().is_empty(), "nothing written");
}

#[tokio::test]
async fn owner_mount_refuses_expired_and_revoked_credentials() {
    let o = owner().await;
    let path = owner_path("supervisor.list");
    assert_eq!(o.supervisor("GET", path, None).await.0, StatusCode::OK);
    // Expiry: the credential was issued at 999000 for 60000 ms.
    o.f.skew.store(60_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        o.supervisor("GET", path, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    o.f.skew.store(0, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(o.supervisor("GET", path, None).await.0, StatusCode::OK);
    sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=999500 WHERE actor_id=$1")
        .bind(o.f.actor)
        .execute(&o.f.pool)
        .await
        .unwrap();
    assert_eq!(
        o.supervisor("GET", path, None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn owner_mount_refuses_a_credential_without_owner_scope() {
    use sha2::Digest as _;
    let mut o = owner().await;
    let digest = |t: &str| hex::encode(sha2::Sha256::digest(t.as_bytes()));
    let old = format!("aqsv1_{}", digest("six scopes"));
    sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=999500 WHERE actor_id=$1 AND revoked_at_ms IS NULL")
        .bind(o.f.actor).execute(&o.f.pool).await.unwrap();
    sqlx::query("INSERT INTO central_supervisor_credentials(token_digest,actor_id,generation,session_id,scopes,issued_at_ms,expires_at_ms) SELECT $1,actor_id,generation,session_id,$2,999000,1059000 FROM central_supervisor_credentials WHERE token_digest=$3")
        .bind(digest(&old)).bind(SIX.map(String::from).to_vec()).bind(digest(&o.token))
        .execute(&o.f.pool).await.unwrap();
    o.token = old;
    let (status, projects) = call(
        &o.machine,
        "GET",
        "/v1/supervisor/projects",
        As::Supervisor(&o.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "otherwise live: {projects}");
    let path = owner_path("supervisor.list");
    let (status, out) = o.supervisor("GET", path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
}

#[tokio::test]
async fn admin_listener_still_requires_csrf() {
    let o = owner().await;
    let (_, activate) = o.draft();
    let post = |csrf| {
        call(
            &o.owner,
            "POST",
            "/api/v1/actions",
            As::Owner { csrf },
            Some(activate.clone()),
        )
    };
    let (status, out) = post(false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
    assert_eq!(out["code"], "forbidden", "{out}");
    assert!(o.graph.posts.lock().unwrap().is_empty(), "nothing written");
    // The header was all that was missing.
    let (status, out) = post(true).await;
    assert_eq!(status, StatusCode::OK, "{out}");
}

#[tokio::test]
async fn owner_mount_does_not_forward_the_bearer() {
    let o = owner().await;
    let echo = Router::new().route(
        "/{*rest}",
        any(
            |uri: Uri, headers: HeaderMap, Extension(owner): Extension<ReadOwner>| async move {
                let has = |name: &str| headers.contains_key(name);
                axum::Json(json!({
                    "uri": uri.to_string(),
                    "authorization": has("authorization"),
                    "cookie": has("cookie"),
                    "dev": has("x-aq-dev-email"),
                    "csrf": headers.get("x-aq-request").and_then(|v| v.to_str().ok()),
                    "owner": owner.email,
                    "delegated": DELEGATED_OWNER.try_with(|o| o.id).ok(),
                }))
            },
        ),
    );
    let machine = aq_server::build_worker_router_with_owner_app(o.f.state.clone(), echo);
    let response = machine
        .oneshot(
            Request::post(format!("{PREFIX}/api/v1/echo?x=1&y=2"))
                .header("authorization", format!("Bearer {}", o.token))
                .header("cookie", "session=browser")
                .header("x-aq-dev-email", "someone@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let seen: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        seen,
        json!({"uri": "/api/v1/echo?x=1&y=2", "authorization": false, "cookie": false,
            "dev": false, "csrf": "1", "owner": "owner@localhost", "delegated": o.f.user.id})
    );
}

#[test]
fn owner_prefix_matches_the_bridge() {
    assert_eq!(
        aq_command_registry::OWNER_PREFIX,
        aq_supervisor_control::owner::PREFIX
    );
    assert_eq!(PREFIX, aq_supervisor_control::owner::PREFIX);
}

impl Owner {
    /// `POST /api/v1/commands/{name}` on the machine listener as the supervisor.
    async fn command(&self, name: &str, request: Value) -> (StatusCode, Value) {
        let path = format!("/api/v1/commands/{name}");
        let who = As::Supervisor(&self.token);
        call(&self.machine, "POST", &path, who, Some(request)).await
    }
}

#[tokio::test]
async fn supervisor_invokes_owner_commands_through_the_command_endpoint() {
    let o = owner().await;
    let (id, a) = o.draft();
    let request = json!({"operation_id":a["operation_id"],"subject":a["subject"],
        "expected_versions":a["expected_versions"],"params":a["payload"]});
    let (status, out) = o.command("task.activate", request).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["disposition"], "confirmed", "{out}");
    assert_eq!(o.graph.nodes.lock().unwrap()[&id]["task_state"], "active");
    let who = As::Supervisor(&o.token);
    let (status, listed) = call(&o.machine, "GET", "/api/v1/commands", who, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let names: Vec<&str> = listed["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    for name in [
        "task.delete",
        "message.send",
        "playbook.catalog.definitions",
    ] {
        assert!(names.contains(&name), "{name}: {names:?}");
    }
}

#[tokio::test]
async fn task_list_limit_survives_the_bridge() {
    let o = owner().await;
    for version in 1..=3 {
        let task = node(Uuid::new_v4(), o.workspace, "task", version);
        let id = task["id"].as_str().unwrap().parse().unwrap();
        o.graph.nodes.lock().unwrap().insert(id, task);
    }
    let params = json!({"project_id":o.project,"limit":2});
    let (status, out) = o.command("task.list", json!({"params":params})).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["items"].as_array().unwrap().len(), 2, "{out}");
    assert!(out["data"]["next_cursor"].is_string(), "{out}");
}

impl Owner {
    /// The supervisor's own (not bridged) routes on the machine listener.
    async fn direct(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        call(
            &self.machine,
            method,
            path,
            As::Supervisor(&self.token),
            body,
        )
        .await
    }
    /// A `task.create` in the fixture project, and Graph's answer to it.
    fn create(&self) -> Value {
        let id = Uuid::new_v4();
        let created = change_set(vec![node(id, self.workspace, "task", 1)]);
        *self.graph.reply.lock().unwrap() = Some((200, created));
        let payload = json!({"operation_id":id,"expected_project_revision":"1","title":"Single",
            "body":"","acceptance_criteria":"","labels":[],"task_state":"active"});
        action("task.create", Subject::Project(self.project, "1"), payload)
    }
}

#[tokio::test]
async fn project_supervisor_cannot_create_a_task_in_another_project() {
    let mut o = project_owner().await;
    let (status, out) = o.direct("POST", "/api/v1/actions", Some(o.create())).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    // Another project is refused as invalid, as without full access.
    o.project = o.other;
    let (status, out) = o.direct("POST", "/api/v1/actions", Some(o.create())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
}

#[tokio::test]
async fn project_supervisor_operation_show_has_no_owner_fallback() {
    let o = project_owner().await;
    let (_, a) = o.draft();
    let request = json!({"operation_id":a["operation_id"],"subject":a["subject"],
        "expected_versions":a["expected_versions"],"params":a["payload"]});
    let (status, sent) = o.command("task.activate", request).await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let path = format!("/api/v1/operations/{}", a["operation_id"].as_str().unwrap());
    let (status, shown) = o.direct("GET", &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{shown}");
}

#[tokio::test]
async fn operation_show_finds_an_operation_sent_through_the_owner_mount() {
    let o = owner().await;
    let (_, a) = o.draft();
    let request = json!({"operation_id":a["operation_id"],"subject":a["subject"],
        "expected_versions":a["expected_versions"],"params":a["payload"]});
    let (status, sent) = o.command("task.activate", request).await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let path = format!("/api/v1/operations/{}", a["operation_id"].as_str().unwrap());
    let (status, shown) = o.direct("GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["data"], sent["data"]);
}

#[tokio::test]
async fn operation_show_still_finds_the_supervisors_own_task_create() {
    let o = owner().await;
    let create = o.create();
    let (status, sent) = o
        .direct("POST", "/api/v1/actions", Some(create.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let path = format!(
        "/api/v1/operations/{}",
        create["operation_id"].as_str().unwrap()
    );
    let (status, shown) = o.direct("GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["data"], sent["data"]);
}

const REFUSAL: &str = "Project supervisor can act only on its own project";

#[tokio::test]
async fn project_supervisor_activates_and_comments_in_its_own_project() {
    let o = project_owner().await;
    let (id, a) = o.draft();
    let request = json!({"operation_id":a["operation_id"],"subject":a["subject"],
        "expected_versions":a["expected_versions"],"params":a["payload"]});
    let (status, out) = o.command("task.activate", request).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(o.graph.nodes.lock().unwrap()[&id]["task_state"], "active");
    let op = Uuid::new_v4();
    let params = json!({"task_id":id,"project_id":o.project,"operation_id":op,"body":"Own"});
    let (status, out) = o
        .command(
            "task.comment.add",
            json!({"operation_id":op,"params":params}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let params = json!({"project_id":o.project});
    let (status, out) = o.command("task.list", json!({"params":params})).await;
    assert_eq!(status, StatusCode::OK, "{out}");
}

#[tokio::test]
async fn project_supervisor_can_submit_the_retry_form_from_a_budget_notification() {
    let o = project_owner().await;
    let (task, _) = o.draft();
    let operation = Uuid::new_v4();
    let (status, out) = o
        .command(
            "task.retry",
            json!({
                "operation_id":operation, "reason":"The reported prerequisite has been resolved.",
                "params":{"project_id":o.project,"task_id":task,"expected_version":"3"}
            }),
        )
        .await;
    // The registered action examined this exact task. It has never had an attempt.
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["disposition"], "rejected", "{out}");
    assert_eq!(out["data"]["operation_id"], operation.to_string());
    assert_eq!(
        out["data"]["subject_refs"],
        json!([{"type":"task","id":task,"revision":"3"}])
    );
}

#[tokio::test]
async fn project_supervisor_is_refused_outside_its_project() {
    let o = project_owner().await;
    let (_, mut a) = o.draft();
    a["payload"]["project_id"] = json!(o.other);
    let activate = json!({"operation_id":a["operation_id"],"subject":a["subject"],
        "expected_versions":a["expected_versions"],"params":a["payload"]});
    let list = json!({"params":{"project_id":o.other}});
    for (name, request) in [
        ("task.activate", activate),
        ("task.list", list),
        ("dashboard.swarm", json!({})),
        ("worker.list", json!({})),
        ("playbook.source.list", json!({})),
    ] {
        let (status, out) = o.command(name, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {out}");
        assert_eq!(out["message"], REFUSAL, "{name}: {out}");
    }
    assert!(o.graph.posts.lock().unwrap().is_empty(), "nothing written");
}

#[tokio::test]
async fn project_supervisor_action_body_bounds() {
    let o = project_owner().await;
    let path = format!("{PREFIX}/api/v1/actions");
    for (body, expected) in [
        ("{not json".to_string(), StatusCode::BAD_REQUEST),
        (" ".repeat(70_000), StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        let request = Request::post(&path)
            .header("authorization", format!("Bearer {}", o.token))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = o.machine.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[tokio::test]
async fn project_supervisor_is_refused_when_its_workspace_is_shared() {
    let o = project_owner().await;
    let path = format!("/api/human/projects/{}/tasks", o.project);
    assert_eq!(o.supervisor("GET", &path, None).await.0, StatusCode::OK);
    let shared = Uuid::new_v4();
    bind(&o.f, shared, o.workspace).await;
    let (status, out) = o.supervisor("GET", &path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
    assert_eq!(out["message"], REFUSAL, "{out}");
}

#[tokio::test]
async fn project_supervisor_cannot_replace_its_binding() {
    let o = project_owner().await;
    let path = format!("/api/projects/{}", o.project);
    let (status, shown) = o.supervisor("GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    let mut binding = shown["binding"].clone();
    binding["graph_workspace_id"] = json!(Uuid::new_v4());
    let put = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"binding":binding});
    let (status, out) = o.supervisor("POST", &path, Some(put)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
    assert_eq!(out["message"], REFUSAL, "{out}");
    let (_, after) = o.supervisor("GET", &path, None).await;
    assert_eq!(after, shown, "binding unchanged");
}

#[tokio::test]
async fn project_supervisor_cannot_send_playbook_actions() {
    let o = project_owner().await;
    let payload = json!({"operation_id":Uuid::new_v4(),"project_id":o.project});
    let start = action(
        "playbook.run.start",
        Subject::Project(o.project, "1"),
        payload,
    );
    let (status, out) = o.supervisor("POST", "/api/v1/actions", Some(start)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
    assert_eq!(out["message"], REFUSAL, "{out}");
}

/// Read-only code access (`/v1/supervisor/git`), with feature
/// `delivery_playbook`: aq-git's git socket is a fake served by
/// `git http-backend` that holds both bound projects' repositories.
#[cfg(feature = "delivery_playbook")]
mod git {
    use super::*;
    use aq_delivery::git_grant::fake::{FakeGitHttp, git};

    struct Git {
        o: Owner,
        fake: FakeGitHttp,
        /// The repositories bound to `o.project` and `o.other`.
        mine: Uuid,
        other: Uuid,
        _dir: tempfile::TempDir,
    }

    async fn repository(o: &Owner, project: Uuid) -> Uuid {
        let id: String = sqlx::query_scalar(
            "SELECT binding->>'repository_id' FROM project_execution_bindings WHERE project_id=$1",
        )
        .bind(project)
        .fetch_one(&o.f.pool)
        .await
        .unwrap();
        id.parse().unwrap()
    }

    /// `o`'s machine listener with the server serving git through the fake.
    async fn serve(mut o: Owner) -> Git {
        let dir = tempfile::tempdir().unwrap();
        let (mine, other) = (
            repository(&o, o.project).await,
            repository(&o, o.other).await,
        );
        let fake = FakeGitHttp::start(dir.path(), mine, "refs/heads/main").await;
        let bare = dir
            .path()
            .join("git-http-repositories")
            .join(other.to_string());
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-q"]);
        let mut config = (*o.f.state.config).clone();
        config.delivery = Some(aq_delivery::github::DeliveryConfig {
            git_http_socket: Some(fake.socket.clone()),
            ..Default::default()
        });
        o.f.state.config = std::sync::Arc::new(config);
        o.machine =
            aq_server::build_worker_router_with_owner_app(o.f.state.clone(), o.owner.clone());
        Git {
            o,
            fake,
            mine,
            other,
            _dir: dir,
        }
    }

    fn refs(repository: impl std::fmt::Display) -> String {
        format!("/v1/supervisor/git/{repository}/info/refs?service=git-upload-pack")
    }

    impl Git {
        async fn get(&self, path: &str) -> StatusCode {
            let who = As::Supervisor(&self.o.token);
            call(&self.o.machine, "GET", path, who, None).await.0
        }
    }

    #[tokio::test]
    async fn global_supervisor_fetches_any_bound_repository() {
        let g = serve(owner().await).await;
        assert_eq!(g.get(&refs(g.mine)).await, StatusCode::OK);
        assert_eq!(g.get(&refs(g.other)).await, StatusCode::OK);
        // An uppercase id names the same repository.
        let upper = g.other.to_string().to_uppercase();
        assert_eq!(g.get(&refs(upper)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn project_supervisor_fetches_only_its_repository() {
        let g = serve(project_owner().await).await;
        assert_eq!(g.get(&refs(g.mine)).await, StatusCode::OK);
        let path = refs(g.other);
        let who = As::Supervisor(&g.o.token);
        let (status, out) = call(&g.o.machine, "GET", &path, who, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
        let why = "Supervisor can read only its projects' repositories";
        assert_eq!(out["message"], why, "{out}");
    }

    #[tokio::test]
    async fn supervisor_git_refuses_unbound_repositories_and_pushes() {
        let g = serve(owner().await).await;
        let before = git(&g.fake.repository, &["for-each-ref"]);
        assert_eq!(g.get(&refs(Uuid::new_v4())).await, StatusCode::FORBIDDEN);
        assert_eq!(g.get(&refs("not-a-uuid")).await, StatusCode::FORBIDDEN);
        let seen = g.fake.seen.lock().unwrap().len();
        let receive = format!("/v1/supervisor/git/{}/git-receive-pack", g.mine);
        let advertise = format!(
            "/v1/supervisor/git/{}/info/refs?service=git-receive-pack",
            g.mine
        );
        for (method, path) in [("POST", &receive), ("GET", &advertise)] {
            let who = As::Supervisor(&g.o.token);
            let (status, out) = call(&g.o.machine, method, path, who, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {out}");
            assert_eq!(out["message"], "Supervisors can only fetch", "{out}");
        }
        // Nothing reached aq-git, and its refs are unchanged.
        assert_eq!(g.fake.seen.lock().unwrap().len(), seen);
        assert_eq!(git(&g.fake.repository, &["for-each-ref"]), before);
    }

    #[tokio::test]
    async fn supervisor_git_requires_a_live_credential() {
        let g = serve(owner().await).await;
        let path = refs(g.mine);
        let who = As::Owner { csrf: true };
        let (status, out) = call(&g.o.machine, "GET", &path, who, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{out}");
        assert_eq!(g.get(&path).await, StatusCode::OK);
        sqlx::query(
            "UPDATE central_supervisor_credentials SET revoked_at_ms=999500 WHERE actor_id=$1",
        )
        .bind(g.o.f.actor)
        .execute(&g.o.f.pool)
        .await
        .unwrap();
        assert_eq!(g.get(&path).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn supervisor_git_refreshes_shallow_tips_over_smart_http() {
        let g = serve(project_owner().await).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let machine = g.o.machine.clone();
        let server = tokio::spawn(async move { axum::serve(listener, machine).await });
        let remote = format!("http://{address}/v1/supervisor/git/{}", g.mine);
        let auth = format!("http.extraHeader=Authorization: Bearer {}", g.o.token);
        let repository = g.fake.repository.clone();
        let base = g.fake.base.clone();

        tokio::task::spawn_blocking(move || {
            let checkout = tempfile::tempdir().unwrap();
            let checkout = checkout.path();
            git(checkout, &["init", "--bare", "--template="]);
            let tree = git(&repository, &["mktree"]);
            let advance = |parent| {
                let head = git(
                    &repository,
                    &["commit-tree", &tree, "-p", parent, "-m", "new tip"],
                );
                git(&repository, &["update-ref", "refs/heads/main", &head]);
                head
            };
            let task_ref = format!("refs/heads/aq/{}", Uuid::new_v4());
            let local_task_ref = task_ref.replacen("refs/heads/", "refs/remotes/origin/", 1);
            let main_ref = "refs/remotes/origin/main";
            let fetch = || {
                git(
                    checkout,
                    &[
                        "-c",
                        &auth,
                        "fetch",
                        "--quiet",
                        "--prune",
                        "--depth=1",
                        "--no-tags",
                        "--no-auto-maintenance",
                        "--",
                        &remote,
                        "+refs/heads/main:refs/remotes/origin/main",
                        "+refs/heads/aq/*:refs/remotes/origin/aq/*",
                    ],
                );
            };

            let first = advance(&base);
            git(&repository, &["update-ref", &task_ref, &first]);
            fetch();
            assert_eq!(
                git(checkout, &["rev-parse", "--is-shallow-repository"]),
                "true"
            );
            assert_eq!(git(checkout, &["rev-parse", main_ref]), first);
            assert_eq!(git(checkout, &["rev-parse", &local_task_ref]), first);
            assert_eq!(git(checkout, &["rev-list", "--count", main_ref]), "1");

            let next = advance(&first);
            git(&repository, &["update-ref", "-d", &task_ref]);
            fetch();
            assert_eq!(git(checkout, &["rev-parse", main_ref]), next);
            assert_eq!(git(checkout, &["rev-list", "--count", main_ref]), "1");
            assert!(git(checkout, &["for-each-ref", "refs/remotes/origin/aq"]).is_empty());
        })
        .await
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn supervisor_git_passes_gzip_fetch_bodies() {
        use std::io::Write as _;
        let g = serve(owner().await).await;
        let want = format!("want {}\n", g.fake.base);
        let request = format!("{:04x}{want}00000009done\n", want.len() + 4);
        let mut gzip = std::process::Command::new("gzip")
            .arg("-c")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        gzip.stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        let body = gzip.wait_with_output().unwrap().stdout;
        let path = format!("/v1/supervisor/git/{}/git-upload-pack", g.mine);
        let request = Request::post(path)
            .header("authorization", format!("Bearer {}", g.o.token))
            .header("content-type", "application/x-git-upload-pack-request")
            .header("content-encoding", "gzip")
            .body(Body::from(body))
            .unwrap();
        let response = g.o.machine.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // git http-backend inflated the body and answered the want with a pack.
        let pack = response.into_body().collect().await.unwrap().to_bytes();
        assert!(pack.starts_with(b"0008NAK\n"), "{pack:?}");
        let seen = g.fake.seen.lock().unwrap();
        let header = |name: &str| seen[0][name].to_str().unwrap().to_owned();
        assert_eq!(header("content-encoding"), "gzip");
        assert_eq!(header("aq-repository"), g.mine.to_string());
        assert_eq!(header("aq-base"), aq_delivery::git_grant::READ_BASE);
        let task_ref = aq_delivery::git_grant::READ_TASK_REF;
        assert_eq!(header("aq-task-ref"), task_ref);
        assert_eq!(header("aq-max-push-bytes"), "1");
        assert!(!seen[0].contains_key("authorization"));
    }
}
