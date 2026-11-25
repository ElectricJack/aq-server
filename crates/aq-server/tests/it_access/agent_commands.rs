//! Spec 0006 phase 1: the generic command surface on both listeners. An
//! agent reaches its commands through `/api/v1/commands` with the same
//! grants, refusals and audit as the `/v1/agent/*` routes; the owner reaches
//! the same handlers as the REST routes.
use crate::messaging::{send, setup_graph, setup_graph_with, setup_graph_with_attempt, setup_with};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

mod children;
mod comments;
mod discovery;
mod draft;
mod graph;
mod limits;
mod owner;
mod reads;
mod subtask;
use graph::{CreatingGraph, DraftFixture, ScopePause};

/// Any method, and a body that need not be JSON.
async fn send_as(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("X-AQ-Request", "1")
        .body(axum::body::Body::from(
            body.map(|v| v.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    let r = app.clone().oneshot(request).await.unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn invoke(e: &crate::messaging::Env, name: &str, params: Value) -> (StatusCode, Value) {
    send(
        &e.app,
        &format!("/api/v1/commands/{name}"),
        Some(&e.token),
        Some(json!({"params":params})),
    )
    .await
}

fn grants() -> Value {
    json!([
        {"operation":"comment.write","scope":"task:subtree"},
        {"operation":"comment.read","scope":"project:self"},
        {"operation":"message.send","scope":"project:self"}
    ])
}
async fn audit_rows(e: &crate::messaging::Env, command: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT decision,reason_code FROM command_audit WHERE attempt_id=$1 AND command=$2 ORDER BY recorded_at_ms")
        .bind(Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap())
        .bind(command)
        .fetch_all(&e.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn agent_whoami_and_discovery_follow_its_grants() {
    let e = setup_with(Some(grants())).await;
    discovery::identity(&e).await;
    discovery::published_grants(&e).await;
    discovery::unavailable_reasons(&e).await;
}

#[tokio::test]
async fn agent_invokes_commands_through_their_routes_once_audited() {
    let e = setup_with(Some(grants())).await;
    discovery::write_and_read_comment(&e).await;
    discovery::question_audit(&e).await;
    discovery::command_refusals(&e).await;
    discovery::foreign_project(&e).await;
}

/// Spec 0006 §9 step 2: a command and its `/v1/agent/*` route run one domain
/// function, so an operation started on either surface has one receipt: the
/// replay through the other returns it, and a changed body conflicts.
#[tokio::test]
async fn a_legacy_route_and_its_command_share_one_receipt() {
    let e = setup_with(Some(grants())).await;
    let op = Uuid::new_v4();
    let text = "Recorded through the legacy route.";
    let legacy_body = json!({"api_version":1,"operation_id":op,"body":text});
    let (status, legacy) = send(
        &e.app,
        "/v1/agent/comments",
        Some(&e.token),
        Some(legacy_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{legacy}");
    let invoke = |body: &str| {
        let request = json!({"params":{"operation_id":op,"body":body}});
        send(
            &e.app,
            "/api/v1/commands/task.comment.add",
            Some(&e.token),
            Some(request),
        )
    };
    let (status, replay) = invoke(text).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, legacy, "the command replays the legacy receipt");
    let (status, body) = invoke("Something else").await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("id_conflict")),
        "{body}"
    );
    let list = json!({"params":{"limit":10}});
    let (_, page) = send(
        &e.app,
        "/api/v1/commands/comment.list",
        Some(&e.token),
        Some(list),
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "{page}");
}

#[tokio::test]
async fn owner_commands_reach_the_same_handlers() {
    let e = setup_with(Some(grants())).await;

    owner::whoami_and_waits(&e).await;
    owner::read_routes(&e).await;
    owner::discovery(&e).await;
    owner::missing_subject(&e).await;
    owner::mutation_routes(&e).await;
    owner::registered_routes(&e).await;
    owner::action_subject(&e).await;
}

/// Spec 0014 step 3b: an agent may not hand out more than it holds. An attempt
/// that runs as the reviewer files a reviewer task, and is refused a developer
/// task, whose role may write source.
#[tokio::test]
async fn an_agent_files_a_task_only_for_a_role_within_its_own_operations() {
    let e = setup_with(Some(json!([
        {"operation":"task.create.subtask","scope":"task:subtree","limits":{"max_created_tasks":3,"max_depth":2}},
        {"operation":"task.read","scope":"project:self"}
    ])))
    .await;
    let attempt = Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap();
    let active = aq_role_settings::active_snapshot(&e.pool).await.unwrap();
    let digest = active.unwrap().snapshot.digest().to_string();
    let ran_with = aq_role_settings::attempts::RanWith {
        role: "reviewer".into(),
        level: "standard-high".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-5-5".into(),
        reasoning: "high".into(),
        harness: "claude".into(),
        execution_profile: "sandbox".into(),
        snapshot_digest: Some(digest),
    };
    let mut conn = e.pool.acquire().await.unwrap();
    aq_role_settings::attempts::record(&mut conn, attempt, &ran_with)
        .await
        .unwrap();
    drop(conn);
    let create = |role: &str| {
        let (app, token) = (e.app.clone(), e.token.clone());
        let params =
            json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self","role":role});
        async move {
            let path = "/api/v1/commands/task.create";
            send(&app, path, Some(&token), Some(json!({"params":params}))).await
        }
    };
    let (status, body) = create("developer").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert_eq!(
        body["message"],
        "Role `developer` is not within your own role `reviewer`: it may use source.write, which you may not. An agent can file a task only for a role whose operations it holds itself."
    );
    let reserved: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_created_tasks")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(reserved, 0, "refused before the quota is reserved");
    // Within its own operations the rule does not refuse it.
    let (_, body) = create("reviewer").await;
    let message = body["message"].as_str().unwrap_or_default();
    assert!(!message.contains("is not within your own role"), "{body}");
    let reserved: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_created_tasks")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(
        reserved, 1,
        "the reviewer task passed the role rule: {body}"
    );
}

/// Spec 0006 phase 2: subtask creation is confined to the attempt's subtree,
/// limited in count and depth, and cannot pick a child's profile.
#[tokio::test]
async fn agent_task_creation_is_scoped_limited_and_cannot_escalate() {
    let case = limits::Case::new().await;
    case.verify_top_level().await;
    case.verify_refused_inputs().await;
    case.verify_depth_and_grant().await;
    case.verify_quota().await;
}

/// A native launch lists only the edge types its startup projection held, so
/// a task launched with no subtasks has no `parent-child` edge type. Its
/// `task.read` grant still reads its children (`task.children`), which the
/// command surface publishes as runnable (found in the spec 0006 phase 6
/// live proof: every `task.children` was refused with `scope_denied`).
#[tokio::test]
async fn task_children_reads_parent_child_edges_for_a_native_profile() {
    children::outgoing().await;
    children::incoming().await;
}

/// An agent creates a subtask end to end: the grant check, the quota
/// reservation, the journal row under the attempt's own principal and the
/// Graph write. Before migration 0099 the journal bound every non-user
/// principal to a supervisor session, so the insert failed its foreign key and
/// every agent create was refused (503) before the Graph write. The earlier
/// test stopped short of the journal (spec 0006 phase 6 live proof found it).
#[tokio::test]
async fn agent_creates_a_subtask_end_to_end() {
    let case = subtask::Case::new().await;
    let op = case.create().await;
    case.verify_graph(op);
    case.verify_depth(op).await;
    case.verify_unscoped_parent().await;
    case.finish();
}

/// One command-surface call as the agent, with an optional agent API version.
async fn whoami_as(app: &axum::Router, token: &str, version: Option<&str>) -> (StatusCode, Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut b = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/commands/whoami")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"));
    if let Some(v) = version {
        b = b.header("X-AQ-Agent-API-Version", v);
    }
    let body = axum::body::Body::from(json!({"params":{}}).to_string());
    let r = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// On the command surface a 401 means only that the credential was rejected,
/// so the worker's re-issue probe cannot be provoked by anything else: an
/// unsupported agent API version is 426 and agent access that is not
/// configured is 503, both as §6.8 refusals.
#[tokio::test]
async fn command_surface_401_means_only_a_rejected_credential() {
    let e = setup_with(Some(
        json!([{"operation":"task.read","scope":"project:self"}]),
    ))
    .await;
    let code = |b: &Value| b["code"].as_str().unwrap_or_default().to_string();
    let (status, body) = whoami_as(&e.app, &e.token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = whoami_as(&e.app, &e.token, Some("2")).await;
    assert_eq!(
        (status, code(&body)),
        (
            StatusCode::UPGRADE_REQUIRED,
            "unsupported_api_version".into()
        ),
        "{body}"
    );
    assert_eq!(body["effect"], "not_applied");
    let forged = format!("aqa_{}", "A".repeat(43));
    let (status, body) = whoami_as(&e.app, &forged, None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "unauthorized".into()),
        "{body}"
    );
    // The same server without its agent access runtime.
    let config: aq_server::config::Config = toml::from_str(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#,
    )
    .unwrap();
    let state = aq_server::AppState {
        internal_work: Default::default(),
        identity: aq_server::identity::verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store: e.db.store().await,
        clock: Arc::new(chrono::Utc::now),
        agent_access: None,
        source_retention: None,
    };
    let app = aq_server::build_worker_router(state);
    let (status, body) = whoami_as(&app, &e.token, None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::SERVICE_UNAVAILABLE, "overloaded".into()),
        "{body}"
    );
}

/// A graph standing in for aq-graph: the agent-access qualified fixture.
async fn fake_graph() -> (String, tokio::task::JoinHandle<()>) {
    fake_graph_with(|_| {}).await
}
/// `fake_graph` serving an edited copy of the fixture.
async fn fake_graph_with(edit: impl FnOnce(&mut Value)) -> (String, tokio::task::JoinHandle<()>) {
    let mut data: Value = serde_json::from_str(include_str!(
        "../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
    ))
    .unwrap();
    edit(&mut data);
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let data = data.clone();
        async move {
            let path = uri.path();
            let body = if path.ends_with("/context") {
                data["context"].clone()
            } else {
                data["nodes"][path.rsplit('/').next().unwrap()].clone()
            };
            ([("X-AQ-Graph-API-Version", "1")], axum::Json(body))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}
const TASK_3: &str = "00000000-0000-0000-0000-000000000003";
const KNOWLEDGE_2: &str = "00000000-0000-0000-0000-000000000002";

/// The `task.*` reads an agent runs in a sandbox: its own task and the tasks
/// its `task.read` scope reaches, as scoped graph reads of task nodes only.
#[tokio::test]
async fn agent_reads_tasks_in_its_scope_and_only_tasks() {
    let (url, graph) = fake_graph().await;
    reads::project(&url).await;
    reads::subtree(&url).await;
    graph.abort();
}

/// An agent edits and deletes its own comments at their current revision,
/// and no one else's.
#[tokio::test]
async fn agent_edits_and_deletes_only_its_own_comments() {
    let e = setup_with(Some(grants())).await;
    let mine = comments::edit_and_replay(&e).await;
    comments::other_authors(&e).await;
    comments::delete(&e, mine).await;
}

/// `wait.list` and `wait.show` show an agent its own attempt's waits only.
#[tokio::test]
async fn agent_lists_and_shows_only_its_own_waits() {
    let e = setup_with(Some(
        json!([{"operation":"wait.read","scope":"attempt:self"}]),
    ))
    .await;
    let id = |k: &str| Uuid::parse_str(e.binding[k].as_str().unwrap()).unwrap();
    let (mine, theirs) = (Uuid::new_v4(), Uuid::new_v4());
    for (wait, attempt) in [(mine, id("attempt_id")), (theirs, Uuid::new_v4())] {
        sqlx::query("INSERT INTO agent_waits(wait_id,caller_key,operation_id,project_id,workspace_id,task_id,attempt_id,node_id,wait_generation,continuation_generation,predicate_digest,request_digest,phase,revision,next_due_ms,deadline_ms,created_at_ms,snapshot,response) VALUES($1,'k',$1,$2,$3,$4,$5,$6,1,1,'p','r','terminal',1,0,0,0,decode('00','hex'),'{}')")
            .bind(wait).bind(id("project_id")).bind(id("workspace_id")).bind(id("task_id")).bind(attempt).bind(id("node_id"))
            .execute(&e.pool).await.unwrap();
    }
    let (status, list) = send(
        &e.app,
        "/api/v1/commands/wait.list",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let ids: Vec<&Value> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["wait_id"])
        .collect();
    assert_eq!(ids, vec![&json!(mine)]);
    for other in [theirs, Uuid::new_v4()] {
        let (status, body) = send(
            &e.app,
            "/api/v1/commands/wait.show",
            Some(&e.token),
            Some(json!({"params":{"wait_id":other}})),
        )
        .await;
        assert_eq!(
            (status, body["code"].clone()),
            (StatusCode::NOT_FOUND, json!("not_found")),
            "{body}"
        );
    }
    assert_eq!(
        audit_rows(&e, "wait.show").await,
        vec![("allow".into(), None), ("allow".into(), None)]
    );
}

async fn install_task_project(e: &crate::messaging::Env, workspace: Value, project: Value) {
    let observed = json!({"repository_id":Uuid::from_u128(2),"graph_workspace_id":workspace,
        "repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured",
        "repository":"verified","graph":"verified","repository_generation":1,"observed_at_ms":1,"expires_at_ms":i64::MAX});
    let binding = json!({"schema_version":1,"project_id":project,"name":"Agent channel fixture","enabled":true,
        "repository_id":Uuid::from_u128(2),"repository_binding_generation":1,
        "repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured",
        "graph_workspace_id":workspace,"allowed_node_ids":[],
        "allowed_worker_groups":[],"allowed_environments":[],
        "policy":{"identity":"project-policy","revision":"1","digest":"b".repeat(64)},"context":[],
        "ceilings":{"permissions":[],"context_bytes":8192,"wall_time_ms":60000,"memory_bytes":268435456,
            "cpu_millis":60000,"process_limit":8,"cost_limit_microunits":null,"currency":null,
            "require_hard_cost_enforcement":false}});
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,$3)")
        .bind(Uuid::parse_str(project.as_str().unwrap()).unwrap()).bind(&binding).bind(&observed)
        .execute(&e.pool).await.unwrap();
}

#[tokio::test]
async fn agent_draft_graph_mutations_fence_both_endpoints_and_work_subject() {
    let case = draft::Case::new().await;
    let (batch, receipt) = case.verify_batch_creation().await;
    case.verify_budget_and_replay(batch, receipt).await;
    case.verify_unconfirmed_parent().await;
    case.verify_review_motivation().await;
    case.verify_refused_endpoints().await;
    case.verify_dependency_effects().await;
    case.verify_invalid_reservation().await;
    case.verify_competing_parents().await;
    case.verify_expiry_before_journaling().await;
    case.finish();
}
