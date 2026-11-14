//! Spec 0006 phase 1: the generic command surface on both listeners. An
//! agent reaches its commands through `/api/v1/commands` with the same
//! grants, refusals and audit as the `/v1/agent/*` routes; the owner reaches
//! the same handlers as the REST routes.
use crate::messaging::{send, setup_graph, setup_graph_with, setup_with};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

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
    let (status, me) = send(
        &e.app,
        "/api/v1/commands/whoami",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"], "agent");
    assert_eq!(me["attempt_id"], e.binding["attempt_id"]);
    assert_eq!(me["task_id"], e.binding["task_id"]);
    assert_eq!(me["project_id"], e.binding["project_id"]);
    assert_eq!(me["grants"].as_array().unwrap().len(), 3);
    assert!(me["grants_digest"].as_str().unwrap().starts_with("sha256:"));
    assert!(me["credential"]["expires_at"].is_string());
    // Enough to address its own session mailbox and the supervisor.
    assert_eq!(me["session_id"], e.binding["session_id"]);
    assert_eq!(me["session_generation"], e.binding["generation"]);
    let supervisor: Option<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_optional(&e.pool)
    .await
    .unwrap();
    assert_eq!(me["supervisor_conversation_id"], json!(supervisor));

    let (status, list) = send(&e.app, "/api/v1/commands", Some(&e.token), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let names: Vec<&str> = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expected in [
        "whoami",
        "operation.show",
        "task.comment.add",
        "comment.list",
        "message.send",
    ] {
        assert!(names.contains(&expected), "{expected} in {names:?}");
    }
    // Published means runnable: no grant, no entry.
    for absent in [
        "question.ask",
        "wait.register",
        "task.show",
        "decision.resolve",
    ] {
        assert!(!names.contains(&absent), "{absent}");
    }
    let add = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "task.comment.add")
        .unwrap();
    assert_eq!(add["scope"], "task:subtree");
    let (_, all) = send(&e.app, "/api/v1/commands?all=true", Some(&e.token), None).await;
    let reason = |name: &str| {
        all["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["reason"].clone())
    };
    assert_eq!(reason("question.ask"), Some(json!("grant_missing")));
    assert_eq!(reason("decision.resolve"), Some(json!("not_available")));
    assert_eq!(reason("task.show"), Some(json!("grant_missing")));
    assert_eq!(reason("task.list"), Some(json!("not_available")));
    // No credential.
    let (status, body) = send(&e.app, "/api/v1/commands", None, None).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("unauthorized"))
    );
}

#[tokio::test]
async fn agent_invokes_commands_through_their_routes_once_audited() {
    let e = setup_with(Some(grants())).await;
    let op = Uuid::new_v4();
    let (status, added) = send(
        &e.app,
        "/api/v1/commands/task.comment.add",
        Some(&e.token),
        Some(json!({"params":{"operation_id":op,"body":"Through the command surface."}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(added["comment"]["comment_id"], json!(op));
    assert_eq!(added["comment"]["author"]["kind"], "agent");
    // One audit row for the call, not one per layer.
    assert_eq!(
        audit_rows(&e, "task.comment.add").await,
        vec![("allow".into(), None)]
    );
    let (status, page) = send(
        &e.app,
        "/api/v1/commands/comment.list",
        Some(&e.token),
        Some(json!({"params":{"limit":5}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["comment_id"], json!(op));

    // Refusals are the §6.8 shape and audited.
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/question.ask",
        Some(&e.token),
        Some(json!({"params":{"operation_id":Uuid::new_v4(),"question":"May I?"}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let refusal: aq_agent_grants::Refusal = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(refusal.code, aq_agent_grants::RefusalCode::GrantMissing);
    assert_eq!(refusal.grant.as_deref(), Some("question.ask"));
    assert_eq!(
        audit_rows(&e, "question.ask").await,
        vec![("deny".into(), Some("grant_missing".into()))]
    );
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/decision.resolve",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("owner_only")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/task.list",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("not_available")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/no.such",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::NOT_FOUND, json!("unknown_command")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/comment.list",
        Some(&e.token),
        Some(json!({"bogus":1})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_request")),
        "{body}"
    );
    // A foreign project is outside project:self.
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/knowledge.search",
        Some(&e.token),
        Some(json!({"params":{"text":"x","project_id":Uuid::new_v4()}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
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
    let (status, me) = send(&e.admin, "/api/v1/commands/whoami", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"], "owner");
    // A read: the command's response is the REST route's response.
    let (status, direct) = send(&e.admin, "/api/waits", None, None).await;
    assert_eq!(status, StatusCode::OK, "{direct}");
    let (status, via) = send(
        &e.admin,
        "/api/v1/commands/wait.list",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{via}");
    assert_eq!(via, direct);
    // An undeclared query param is refused before the route is asked.
    let (status, refused) = send(
        &e.admin,
        "/api/v1/commands/wait.list",
        None,
        Some(json!({"params":{"projet_id":e.binding["project_id"]}})),
    )
    .await;
    assert_eq!(
        (status, refused["code"].clone(), refused["message"].clone()),
        (
            StatusCode::BAD_REQUEST,
            json!("invalid_request"),
            json!("unknown param")
        ),
        "{refused}"
    );
    // The owner reads registered from REST answer as their routes do,
    // whatever the route answers in this fixture.
    let project = e.binding["project_id"].as_str().unwrap();
    for (name, path, params, same_body) in [
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
            "conversation.project",
            format!("/api/human/conversations/projects/{project}"),
            json!({"project":project}),
            false,
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
            "/api/settings/intelligence".to_string(),
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
            "profile.catalog.active",
            "/api/profiles/catalogs".to_string(),
            json!({}),
            false,
        ),
        (
            "profile.catalog.show",
            format!("/api/profiles/catalogs/{}", "0".repeat(64)),
            json!({"digest":"0".repeat(64)}),
            false,
        ),
    ] {
        let (direct_status, direct) = send(&e.admin, &path, None, None).await;
        let (status, via) = send(
            &e.admin,
            &format!("/api/v1/commands/{name}"),
            None,
            Some(json!({"params":params})),
        )
        .await;
        assert_eq!(status, direct_status, "{name}: {via} / {direct}");
        // Bodies with clocks or request IDs are compared by status only.
        if same_body && status == StatusCode::OK {
            assert_eq!(via, direct, "{name}");
        }
    }
    let (status, listed) = send(&e.admin, "/api/v1/commands", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = listed["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"decision.resolve") && names.contains(&"task.show"));
    assert!(
        !names.contains(&"wait.register"),
        "agent-only commands are not the owner's"
    );
    // A mutation: the command builds the same ActionRequest as /api/v1/actions.
    let (status, body) = send(
        &e.admin,
        "/api/v1/commands/task.comment.add",
        None,
        Some(json!({"params":{"operation_id":Uuid::new_v4(),"project_id":e.binding["project_id"],"body":"x"}})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_request")),
        "subject is required: {body}"
    );
    // With the task named, the command builds the subject itself and answers
    // exactly as the action union does for the same request.
    let (op, task) = (Uuid::new_v4(), Uuid::new_v4());
    let payload = json!({"operation_id":op,"project_id":e.binding["project_id"],"body":"x"});
    let (direct_status, direct) = send(
        &e.admin,
        "/api/v1/actions",
        None,
        Some(
            json!({"operation_id":op,"kind":"task.comment.add","subject":{"type":"task","id":task},
            "expected_versions":{},"payload":payload}),
        ),
    )
    .await;
    let mut params = payload.clone();
    params["task_id"] = json!(task);
    let (status, via) = send(
        &e.admin,
        "/api/v1/commands/task.comment.add",
        None,
        Some(json!({"operation_id":op,"params":params})),
    )
    .await;
    assert_ne!(via["message"], "an action needs a subject", "{via}");
    assert_eq!(
        (status, via["code"].clone()),
        (direct_status, direct["code"].clone()),
        "{via} / {direct}"
    );
    let owner_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM command_audit WHERE caller_kind='owner'")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert!(owner_rows >= 3);
}

/// Spec 0006 phase 2: subtask creation is confined to the attempt's subtree,
/// limited in count and depth, and cannot pick a child's profile.
#[tokio::test]
async fn agent_task_creation_is_scoped_limited_and_cannot_escalate() {
    let e = setup_with(Some(json!([
        {"operation":"task.create.subtask","scope":"task:subtree","limits":{"max_created_tasks":3,"max_depth":2}},
        {"operation":"task.read","scope":"project:self"}
    ])))
    .await;
    let attempt = Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap();
    let own = Uuid::parse_str(e.binding["task_id"].as_str().unwrap()).unwrap();
    let project = Uuid::parse_str(e.binding["project_id"].as_str().unwrap()).unwrap();
    let create = |params: Value| {
        let app = e.app.clone();
        let token = e.token.clone();
        async move {
            send(
                &app,
                "/api/v1/commands/task.create",
                Some(&token),
                Some(json!({"params":params})),
            )
            .await
        }
    };
    let code = |b: &Value| b["code"].as_str().unwrap_or_default().to_string();
    // No parent: a top-level task needs task.create.project, which is not held.
    let (status, body) =
        create(json!({"operation_id":Uuid::new_v4(),"title":"Unrelated top-level task"})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "grant_missing".into()),
        "{body}"
    );
    assert_eq!(body["grant"], "task.create.project");
    assert_eq!(body["held"][0]["operation"], "task.create.subtask");
    // Its audit row names the grant the refusal names, not the command's
    // default `task.create.subtask` (spec 0006 phase 6 proof, item 9).
    let grants = || async {
        sqlx::query_as::<_, (String, Option<String>, Option<String>)>("SELECT decision,reason_code,grant_operation FROM command_audit WHERE attempt_id=$1 AND command='task.create' ORDER BY recorded_at_ms,audit_id")
            .bind(attempt).fetch_all(&e.pool).await.unwrap()
    };
    let deny = |grant: &str| {
        (
            "deny".to_string(),
            Some("grant_missing".to_string()),
            Some(grant.to_string()),
        )
    };
    assert_eq!(grants().await, vec![deny("task.create.project")]);
    // A parent outside the subtree.
    let (status, body) =
        create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":Uuid::new_v4()})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "scope_denied".into()),
        "{body}"
    );
    // A hint that could launch the child under another profile.
    let (status, body) = create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self","profile_hint":"admin"})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "scope_denied".into()),
        "{body}"
    );
    // Depth: a grandchild (depth 2) is allowed, a great-grandchild is not.
    let (child, grandchild) = (Uuid::new_v4(), Uuid::new_v4());
    for (task, parent, depth) in [(child, own, 1), (grandchild, child, 2)] {
        sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$1,0)")
            .bind(task).bind(attempt).bind(project).bind(parent).bind(own).bind(depth)
            .execute(&e.pool).await.unwrap();
    }
    let (status, body) =
        create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":grandchild})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "quota_exceeded".into()),
        "{body}"
    );
    assert!(body["message"].as_str().unwrap().contains("max_depth"));
    // Within scope and depth: admitted and reserved against the quota (the
    // Graph write itself has no project binding in this fixture).
    let op = Uuid::new_v4();
    let (status, body) =
        create(json!({"operation_id":op,"title":"Verify changelog links","parent_id":child})).await;
    assert!(
        !matches!(
            code(&body).as_str(),
            "grant_missing" | "scope_denied" | "quota_exceeded"
        ),
        "{status} {body}"
    );
    let depth: Option<i32> =
        sqlx::query_scalar("SELECT depth FROM agent_created_tasks WHERE task_id=$1")
            .bind(op)
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(depth, Some(2));
    // An admitted subtask create records the grant it used.
    let allowed = grants()
        .await
        .into_iter()
        .filter(|r| r.0 == "allow")
        .collect::<Vec<_>>();
    assert!(!allowed.is_empty());
    assert!(
        allowed
            .iter()
            .all(|r| r.2.as_deref() == Some("task.create.subtask")),
        "{allowed:?}"
    );
    // Quota: three created by this attempt; a fourth is refused.
    let (status, body) =
        create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self"})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "quota_exceeded".into()),
        "{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("max_created_tasks")
    );
    let denials: i64 = sqlx::query_scalar("SELECT count(*) FROM command_audit WHERE attempt_id=$1 AND command='task.create' AND decision='deny'")
        .bind(attempt).fetch_one(&e.pool).await.unwrap();
    assert!(denials >= 5, "{denials}");
}

/// A native launch lists only the edge types its startup projection held, so
/// a task launched with no subtasks has no `parent-child` edge type. Its
/// `task.read` grant still reads its children (`task.children`), which the
/// command surface publishes as runnable (found in the spec 0006 phase 6
/// live proof: every `task.children` was refused with `scope_denied`).
#[tokio::test]
async fn task_children_reads_parent_child_edges_for_a_native_profile() {
    let (url, graph) = fake_graph().await;
    let native = |profile: &mut Value, context: &mut Value| {
        profile["read_scope"]["edge_types"] = json!(["references"]);
        context["edges"]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["edge_type"] == "references");
    };
    let e = setup_graph_with(
        Some(json!([{"operation":"task.read","scope":"project:self"}])),
        &url,
        None,
        native,
    )
    .await;
    let (status, children) = send(
        &e.app,
        "/api/v1/commands/task.children",
        Some(&e.token),
        Some(json!({"params":{}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{children}");
    let edges = children["result"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{children}");
    assert_eq!(edges[0]["edge_type"], "parent-child");
    assert_eq!(edges[0]["target"], TASK_3);
    // Without task.read the edge type stays outside the profile.
    let e = setup_graph_with(
        Some(json!([{"operation":"knowledge.read","scope":"project:self"}])),
        &url,
        None,
        native,
    )
    .await;
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/graph.read",
        Some(&e.token),
        Some(
            json!({"params":{"query":{"kind":"relationships","node_id":e.binding["task_id"],
            "direction":"outgoing","edge_types":["parent-child"],"limit":64,"cursor":null}}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    graph.abort();

    // Which parent-child edges come back follows the task.read scope: here
    // the attempt's own task is the child of task 3. Its parent edge
    // (source task 3) is readable at project:self, not at task:self, even
    // though knowledge.read reads every record of the project.
    let (url, graph) = fake_graph_with(|data| {
        for e in data["context"]["edges"].as_array_mut().unwrap() {
            if e["edge_type"] == "parent-child" {
                let source = e["source"].clone();
                e["source"] = e["target"].clone();
                e["target"] = source;
            }
        }
    })
    .await;
    let parents = |scope: &'static str| {
        let url = url.clone();
        async move {
            let e = setup_graph_with(
                Some(json!([
                    {"operation":"task.read","scope":scope},
                    {"operation":"knowledge.read","scope":"project:self"}
                ])),
                &url,
                None,
                native,
            )
            .await;
            let (status, body) = send(
                &e.app,
                "/api/v1/commands/graph.read",
                Some(&e.token),
                Some(json!({"params":{"query":{"kind":"relationships","node_id":e.binding["task_id"],
                    "direction":"incoming","edge_types":["parent-child"],"limit":64,"cursor":null}}})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["result"]["edges"].as_array().unwrap().len()
        }
    };
    assert_eq!(parents("project:self").await, 1);
    assert_eq!(parents("task:self").await, 0);
    assert_eq!(parents("task:subtree").await, 0);
    graph.abort();
}

/// A Graph standing in for aq-graph's node read and mutation routes: tasks
/// are served with `aq:project_id` (unless `bare`), and a mutation commits
/// the posted operations, recording each body.
async fn creating_graph(
    workspace: Value,
    project: Value,
    bare: Arc<AtomicBool>,
    posted: Arc<Mutex<Vec<Value>>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let node = move |id: &Value, title: &str, metadata: Value| {
        json!({"id":id,"workspace_id":workspace,"kind":"task","version":1,"content_revision":1,"title":title,
            "body":"","acceptance_criteria":"","labels":[],"metadata":metadata,"task_state":"active",
            "verification":"unverified","provenance":[],"retired":false,"deleted":false,
            "created_at":"2026-11-07T00:00:00Z","updated_at":"2026-11-07T00:00:00Z","created_by":"dev-service",
            "last_verified_by":null,"last_verified_at":null,"retirement_reason":null,"content_type":"note",
            "summary":null,"summary_of_revision":null,"valid_from":null,"valid_until":null,"review_at":null,
            "successor_id":null})
    };
    let app = axum::Router::new().fallback(
        move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
            let (node, project, bare, posted) =
                (node.clone(), project.clone(), bare.clone(), posted.clone());
            async move {
                let path = uri.path().to_owned();
                let reply = if method == axum::http::Method::POST && path.ends_with("/mutations") {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    posted.lock().unwrap().push(request.clone());
                    let results: Vec<Value> = request["operations"].as_array().unwrap().iter().map(|op| {
                        if op["op"] == "create_node" {
                            node(&op["id"], op["title"].as_str().unwrap(), op["metadata"].clone())
                        } else {
                            json!({"id":op["id"],"workspace_id":node(&op["id"],"x",json!({}))["workspace_id"],
                                "source":op["source"],"target":op["target"],"edge_type":op["edge_type"],
                                "source_revision":null,"target_revision":null,"version":1,"deleted":false,
                                "created_at":"2026-11-07T00:00:00Z","description":null})
                        }
                    }).collect();
                    json!({"change_set_id":Uuid::new_v4(),"results":results})
                } else {
                    let id = json!(path.rsplit('/').next().unwrap());
                    let metadata = if bare.load(Ordering::SeqCst) {
                        json!({})
                    } else {
                        json!({"aq:project_id":project})
                    };
                    node(&id, "Draft release notes for X", metadata)
                };
                ([("X-AQ-Graph-API-Version", "1")], axum::Json(reply))
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

/// An agent creates a subtask end to end: the grant check, the quota
/// reservation, the journal row under the attempt's own principal and the
/// Graph write. Before migration 0099 the journal bound every non-user
/// principal to a supervisor session, so the insert failed its foreign key and
/// every agent create was refused (503) before the Graph write. The earlier
/// test stopped short of the journal (spec 0006 phase 6 live proof found it).
#[tokio::test]
async fn agent_creates_a_subtask_end_to_end() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../protocol/agent-access/v1/fixtures/base.json"
    ))
    .unwrap();
    let base = &fixture["context"]["binding"];
    let (workspace, project) = (base["workspace_id"].clone(), base["project_id"].clone());
    let bare = Arc::new(AtomicBool::new(false));
    let posted = Arc::new(Mutex::new(Vec::new()));
    let (url, graph) = creating_graph(
        workspace.clone(),
        project.clone(),
        bare.clone(),
        posted.clone(),
    )
    .await;
    let e = setup_graph_with(
        Some(json!([
            {"operation":"task.create.subtask","scope":"task:subtree","limits":{"max_created_tasks":5,"max_depth":2}},
            {"operation":"task.read","scope":"project:self"}
        ])),
        &url,
        Some("dev-service"),
        |_, _| {},
    )
    .await;
    let attempt = Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap();
    let own = e.binding["task_id"].clone();
    let observed = json!({"repository_id":Uuid::from_u128(2),"graph_workspace_id":workspace,
        "repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured",
        "repository":"verified","graph":"verified","repository_generation":1,"observed_at_ms":1,"expires_at_ms":i64::MAX});
    let binding = json!({"schema_version":1,"project_id":project,"name":"Agent channel fixture","enabled":true,
        "repository_id":Uuid::from_u128(2),"repository_binding_generation":1,
        "repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured",
        "graph_workspace_id":workspace,"catalog_digest":"a".repeat(64),"allowed_agent_profiles":[],
        "allowed_intelligence_profiles":[],"allowed_execution_profiles":[],"allowed_node_ids":[],
        "allowed_worker_groups":[],"allowed_environments":[],
        "policy":{"identity":"project-policy","revision":"1","digest":"b".repeat(64)},"context":[],
        "ceilings":{"permissions":[],"context_bytes":8192,"wall_time_ms":60000,"memory_bytes":268435456,
            "cpu_millis":60000,"process_limit":8,"cost_limit_microunits":null,"currency":null,
            "require_hard_cost_enforcement":false}});
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,$3)")
        .bind(Uuid::parse_str(project.as_str().unwrap()).unwrap()).bind(&binding).bind(&observed)
        .execute(&e.pool).await.unwrap();
    let create = |params: Value| {
        let (app, token) = (e.app.clone(), e.token.clone());
        async move {
            send(
                &app,
                "/api/v1/commands/task.create",
                Some(&token),
                Some(json!({"params":params})),
            )
            .await
        }
    };
    let op = Uuid::new_v4();
    let (status, body) = create(json!({"operation_id":op,"title":"Verify changelog links",
        "body":"Created by the agent.","parent_id":"self"}))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    // Journaled under the attempt's own principal domain: neither an owner
    // nor a supervisor session.
    let row: (Uuid, Option<Uuid>, Option<Uuid>, String) = sqlx::query_as(
        "SELECT principal_id,owner_id,supervisor_session_id,kind FROM dashboard_operations WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&e.pool)
    .await
    .unwrap();
    assert_eq!(row, (attempt, None, None, "task.create".into()));
    // The Graph write: an active task with the agent's attribution, under
    // the attempt's own task.
    let posted = posted.lock().unwrap().clone();
    assert_eq!(posted.len(), 1);
    let ops = posted[0]["operations"].as_array().unwrap();
    let created = ops.iter().find(|o| o["op"] == "create_node").unwrap();
    assert_eq!(created["id"], json!(op));
    assert_eq!(created["task_state"], "active");
    assert_eq!(created["metadata"]["aq:agent_attempt_id"], json!(attempt));
    assert_eq!(created["metadata"]["aq:project_id"], project);
    assert!(
        ops.iter().any(|o| o["op"] == "add_edge"
            && o["edge_type"] == "parent-child"
            && o["source"] == own
            && o["target"] == json!(op)),
        "{posted:?}"
    );
    let depth: Option<i32> =
        sqlx::query_scalar("SELECT depth FROM agent_created_tasks WHERE task_id=$1")
            .bind(op)
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(depth, Some(1));
    // A parent the Graph does not record as a task of this project is a
    // §6.8 refusal with the reason, not a bare 403, and nothing is written.
    bare.store(true, Ordering::SeqCst);
    let (status, body) =
        create(json!({"operation_id":Uuid::new_v4(),"title":"x","parent_id":"self"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_denied", "{body}");
    assert_eq!(body["grant"], "task.create.subtask");
    assert_eq!(body["effect"], "not_applied");
    assert!(
        body["message"].as_str().unwrap().contains("aq:project_id"),
        "{body}"
    );
    graph.abort();
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
    let e = setup_graph(
        Some(json!([
            {"operation":"task.read","scope":"project:self"},
            {"operation":"comment.read","scope":"project:self"}
        ])),
        &url,
    )
    .await;
    let run = |name: &'static str, params: Value| {
        let (app, token) = (e.app.clone(), e.token.clone());
        async move {
            send(
                &app,
                &format!("/api/v1/commands/{name}"),
                Some(&token),
                Some(json!({"params":params})),
            )
            .await
        }
    };
    let (status, own) = run("task.show", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert_eq!(own["result"]["node"]["id"], e.binding["task_id"]);
    assert_eq!(own["result"]["node"]["kind"], "task");
    let (status, other) = run("task.show", json!({"task_id":TASK_3})).await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(other["result"]["node"]["id"], TASK_3);
    // A knowledge record is not a task: the task grant does not read it.
    let (status, body) = run("task.show", json!({"task_id":KNOWLEDGE_2})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found")),
        "{body}"
    );
    let (status, children) = run("task.children", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{children}");
    let edges = children["result"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{children}");
    assert_eq!(edges[0]["target"], TASK_3);
    // Another project is refused before any read.
    let (status, body) = run("task.show", json!({"project_id":Uuid::new_v4()})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    // Comments of another task in scope.
    let (status, page) = run("comment.list", json!({"task_id":TASK_3})).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["task_id"], TASK_3);
    assert_eq!(
        audit_rows(&e, "task.show").await,
        vec![
            ("allow".into(), None),
            ("allow".into(), None),
            ("allow".into(), None),
            ("deny".into(), Some("scope_denied".into()))
        ]
    );

    // task:self reads only the attempt's own task; task:subtree also the
    // tasks it created below it.
    let e = setup_graph(
        Some(json!([
            {"operation":"task.read","scope":"task:subtree"},
            {"operation":"comment.read","scope":"task:self"}
        ])),
        &url,
    )
    .await;
    let run = |name: &'static str, params: Value| {
        let (app, token) = (e.app.clone(), e.token.clone());
        async move {
            send(
                &app,
                &format!("/api/v1/commands/{name}"),
                Some(&token),
                Some(json!({"params":params})),
            )
            .await
        }
    };
    let (status, body) = run("task.show", json!({"task_id":TASK_3})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    assert_eq!(
        body["held"],
        json!([{"operation":"task.read","scope":"task:subtree"}])
    );
    let id = |k: &str| Uuid::parse_str(e.binding[k].as_str().unwrap()).unwrap();
    sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms) VALUES($1,$2,$3,$4,$4,1,$1,0)")
        .bind(Uuid::parse_str(TASK_3).unwrap()).bind(id("attempt_id")).bind(id("project_id")).bind(id("task_id"))
        .execute(&e.pool).await.unwrap();
    let (status, body) = run("task.show", json!({"task_id":TASK_3})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // comment.read at task:self: the own thread, not the child's, on the
    // command surface and on the route itself.
    let (status, _) = run("comment.list", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = run("comment.list", json!({"task_id":TASK_3})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1,"task_id":TASK_3})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    graph.abort();
}

/// An agent edits and deletes its own comments at their current revision,
/// and no one else's.
#[tokio::test]
async fn agent_edits_and_deletes_only_its_own_comments() {
    let e = setup_with(Some(grants())).await;
    let id = |k: &str| Uuid::parse_str(e.binding[k].as_str().unwrap()).unwrap();
    let run = |name: &'static str, params: Value| {
        let (app, token) = (e.app.clone(), e.token.clone());
        async move {
            send(
                &app,
                &format!("/api/v1/commands/{name}"),
                Some(&token),
                Some(json!({"params":params})),
            )
            .await
        }
    };
    let mine = Uuid::new_v4();
    let (status, _) = run(
        "task.comment.add",
        json!({"operation_id":mine,"body":"First draft."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let edit = json!({"operation_id":Uuid::new_v4(),"comment_id":mine,"expected_revision":"1","body":"Edited."});
    let (status, body) = run("task.comment.edit", edit.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["revision"].clone(), body["comment"]["body"].clone()),
        (json!("2"), json!("Edited."))
    );
    // Replay returns the recorded effect; a stale revision changes nothing.
    let (status, again) = run("task.comment.edit", edit.clone()).await;
    assert_eq!(
        (status, again["revision"].clone()),
        (StatusCode::OK, json!("2"))
    );
    let mut stale = edit;
    stale["operation_id"] = json!(Uuid::new_v4());
    let (status, body) = run("task.comment.edit", stale).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("stale_revision")),
        "{body}"
    );
    // The owner's comment on the same task is not the agent's to change.
    let theirs = Uuid::new_v4();
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    aq_task_comments::add(
        &e.pool,
        theirs,
        aq_task_comments::Thread {
            project_id: id("project_id"),
            task_id: id("task_id"),
        },
        &aq_task_comments::Author::Owner { user_id: owner },
        "Owner note.",
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    for target in [theirs, Uuid::new_v4()] {
        let (status, body) = run(
            "task.comment.delete",
            json!({"operation_id":Uuid::new_v4(),"comment_id":target,"expected_revision":"1"}),
        )
        .await;
        assert_eq!(
            (status, body["code"].clone()),
            (StatusCode::FORBIDDEN, json!("scope_denied")),
            "{body}"
        );
        assert_eq!(body["grant"], "comment.write");
    }
    // The route's own author check, without the command surface.
    let (status, body) = send(
        &e.app,
        "/v1/agent/comments/edit",
        Some(&e.token),
        Some(json!({"api_version":1,"operation_id":Uuid::new_v4(),"comment_id":theirs,"expected_revision":"1","body":"Mine now."})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    let (status, body) = run(
        "task.comment.delete",
        json!({"operation_id":Uuid::new_v4(),"comment_id":mine,"expected_revision":"2"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["comment"]["body"], Value::Null);
    assert_eq!(
        audit_rows(&e, "task.comment.delete").await,
        vec![
            ("deny".into(), Some("scope_denied".into())),
            ("deny".into(), Some("scope_denied".into())),
            ("allow".into(), None)
        ]
    );
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
