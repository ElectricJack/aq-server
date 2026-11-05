//! HTTP search behavior against disposable SQL and a strict Graph protocol stub.
mod common;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query},
    http::{Request, StatusCode},
    routing::get,
};
use common::profiled::{NOW, fixture, insert};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{Arc, atomic::Ordering},
};
use tower::ServiceExt;
use uuid::Uuid;
async fn read(app: &Router, path: &str, email: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}
fn group<'a>(body: &'a Value, kind: &str) -> &'a Value {
    body["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["type"] == kind)
        .unwrap()
}
async fn graph(
    Path(workspace): Path<Uuid>,
    Query(q): Query<HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    assert_eq!(q["limit"], "32");
    assert!(matches!(q["kind"].as_str(), "task" | "knowledge"));
    let mut items = vec![];
    if q["q"] == "example" || q["q"] == "private-search-canary" {
        for n in 1..=3 {
            let mut item: Value = serde_json::from_str(include_str!(
                "../../../protocol/human-graph/v1/examples/task.json"
            ))
            .unwrap();
            item = item["data"].take();
            item["workspace_id"] = json!(workspace);
            item["id"] = json!(Uuid::from_u128(n));
            item["kind"] = json!(q["kind"]);
            item["version"] = json!(4);
            item["content_revision"] = json!(3);
            item["title"] = json!(format!("example {} {n}", q["kind"]));
            item["summary"] = json!("Example public summary\nwith control removed");
            item["summary_of_revision"] = json!(3);
            item["metadata"] = json!({"private":"private-search-canary"});
            items.push(item);
        }
    }
    (
        [("X-AQ-Graph-API-Version", "1")],
        Json(json!({"items":items,"next_cursor":null})),
    )
}
#[tokio::test]
async fn search_graph_groups_are_pinned_sanitized_scoped_and_replay_bound() {
    let mut f = fixture().await;
    insert(&f).await;
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            socket,
            Router::new().route("/v1/workspaces/{workspace}/search", get(graph)),
        )
        .await
        .unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "private-search-canary-service-token-0123456789").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: format!("http://{address}"),
            graph_token_file: token,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let app = aq_server::build_admin_router(f.state.clone());
    // A separately configured Graph project has no retained attempt foreign
    // keys, so removal is a legitimate fixture operation.
    let project = Uuid::new_v4();
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) SELECT $1,revision,jsonb_set(binding,'{project_id}',to_jsonb($1::uuid)),validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$2")
        .bind(project).bind(f.selection.lineage.project_id).execute(&pool).await.unwrap();
    let base = format!("/api/v1/search?q=example&limit=1&scope=project:{project}");
    let (status, body) = read(&app, &base, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for kind in ["task", "knowledge"] {
        let g = group(&body, kind);
        assert_eq!(g["hits"].as_array().unwrap().len(), 1, "{body}");
        assert_eq!(g["hits"][0]["ref"]["revision"], "3");
        assert_eq!(g["hits"][0]["project"]["id"], json!(project));
        assert_eq!(
            g["hits"][0]["excerpt"],
            "Example public summarywith control removed"
        );
        assert_eq!(g["has_more"], true);
    }
    assert!(!body.to_string().contains("private-search-canary"));
    let cursor = group(&body, "task")["next_cursor"].as_str().unwrap();
    let next = format!("{base}&cursor={cursor}");
    let (status, page) = read(&app, &next, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_ne!(
        group(&body, "task")["hits"][0]["ref"]["id"],
        group(&page, "task")["hits"][0]["ref"]["id"]
    );
    let (_, replay) = read(&app, &next, "owner@localhost").await;
    assert_eq!(replay["data"]["snapshot_id"], page["data"]["snapshot_id"]);
    assert_eq!(replay["data"]["sources"], page["data"]["sources"]);
    for kind in ["task", "knowledge"] {
        assert_eq!(group(&replay, kind)["hits"], group(&page, kind)["hits"]);
        let cursor = group(&replay, kind)["next_cursor"].as_str().unwrap();
        assert_eq!(
            read(&app, &format!("{base}&cursor={cursor}"), "owner@localhost")
                .await
                .0,
            StatusCode::OK
        );
    }
    let last_cursor = group(&page, "task")["next_cursor"].as_str().unwrap();
    let (_, last) = read(
        &app,
        &format!("{base}&cursor={last_cursor}"),
        "owner@localhost",
    )
    .await;
    assert_eq!(group(&last, "task")["has_more"], false);
    assert!(group(&last, "task")["next_cursor"].is_null());
    for response in [&body, &page, &last] {
        assert!(
            response["data"]["groups"]
                .as_array()
                .unwrap()
                .iter()
                .all(|g| g["has_more"].as_bool().unwrap() == g["next_cursor"].is_string())
        );
    }
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../protocol/dashboard-read/v1/fixtures/search.json"
            ),
            format!("{}\n", serde_json::to_string_pretty(&body).unwrap()),
        )
        .unwrap();
    }
    for changed in [
        next.replace("q=example", "q=changed"),
        next.replace("limit=1", "limit=2"),
        next.replace(&format!("project:{project}"), "all_authorized"),
        format!("{next}&types=task"),
    ] {
        assert_eq!(
            read(&app, &changed, "owner@localhost").await.0,
            StatusCode::GONE
        );
    }
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    assert_eq!(
        read(&app, &next, "other@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(&app, &next, "unknown@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    let restarted = aq_server::build_admin_router(f.state.clone());
    assert_eq!(
        read(&restarted, &next, "owner@localhost").await.0,
        StatusCode::GONE
    );
    f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
    assert_eq!(
        read(&app, &next, "owner@localhost").await.0,
        StatusCode::GONE
    );
    f.time.store(NOW as i64, Ordering::SeqCst);
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        read(&app, &next, "owner@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    let (_, empty) = read(
        &app,
        &format!("/api/v1/search?q=example&scope=project:{}", Uuid::new_v4()),
        "owner@localhost",
    )
    .await;
    assert!(group(&empty, "task")["hits"].as_array().unwrap().is_empty());
    let (_, secret) = read(
        &app,
        "/api/v1/search?q=private-search-canary",
        "owner@localhost",
    )
    .await;
    assert!(
        secret["data"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["hits"].as_array().unwrap().is_empty())
    );
    // Distinguish this snapshot from earlier ones when the cache evicts by age.
    f.time.store((NOW + 1) as i64, Ordering::SeqCst);
    let (_, fresh) = read(&app, &base, "owner@localhost").await;
    let fresh_cursor = group(&fresh, "task")["next_cursor"].as_str().unwrap();
    let fresh_next = format!("{base}&cursor={fresh_cursor}");
    server.abort();
    let (_, partial) = read(&app, &base, "owner@localhost").await;
    assert!(
        group(&partial, "task")["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(partial["coverage"]["state"], "partial");
    assert!(
        partial["data"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["source"] == "task"
                && s["coverage"]["reason"] == "owner_unavailable_or_not_visible")
    );
    sqlx::query("DELETE FROM project_execution_bindings WHERE project_id=$1")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        read(&app, &fresh_next, "owner@localhost").await.0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn search_session_visibility_uses_exact_current_acl_and_scope() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let actor = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'viewer@localhost','viewer','dev','viewer@localhost',now())").bind(actor).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let path = "/api/v1/search?q=Remote";
    let (_, denied) = read(&app, path, "viewer@localhost").await;
    assert!(
        group(&denied, "session")["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut wrong = f.profiled.offer.binding.session.clone();
    if let aq_session_protocol::SessionIdentity::Remote { boot_id, .. } = &mut wrong {
        *boot_id = Uuid::new_v4();
    }
    let wrong_key = aq_server::session_access::session_key(&wrong).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(wrong_key).bind(json!(wrong)).bind(actor).bind((NOW+1000)as i64).execute(&pool).await.unwrap();
    let (_, denied) = read(&app, path, "viewer@localhost").await;
    assert!(
        group(&denied, "session")["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let key = aq_server::session_access::session_key(&f.profiled.offer.binding.session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(&key).bind(json!(f.profiled.offer.binding.session)).bind(actor).bind((NOW+1000)as i64).execute(&pool).await.unwrap();
    let (_, visible) = read(&app, path, "viewer@localhost").await;
    assert_eq!(
        group(&visible, "session")["hits"].as_array().unwrap().len(),
        1,
        "{visible}"
    );
    let (_, scoped) = read(
        &app,
        &format!("{path}&scope=project:{}", Uuid::new_v4()),
        "viewer@localhost",
    )
    .await;
    assert!(
        group(&scoped, "session")["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE session_inspection_acl SET can_inspect=false,revision=2 WHERE session_key=$1 AND principal_id=$2").bind(key).bind(actor).execute(&pool).await.unwrap();
    let (_, revoked) = read(&app, path, "viewer@localhost").await;
    assert!(
        group(&revoked, "session")["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn search_session_cursor_refuses_revoked_acl_partition() {
    let mut f = fixture().await;
    insert(&f).await;
    let first = f.profiled.offer.binding.session.clone();
    let task = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    let second = aq_session_protocol::SessionIdentity::Remote {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
        attempt_id: attempt,
        assignment_generation: 1,
        session_id: Uuid::new_v4(),
    };
    f.selection.lineage.task_id = task;
    f.decision = Uuid::new_v4();
    f.profiled.offer.binding.attempt_id = attempt;
    f.profiled.offer.binding.offer_id = Uuid::new_v4();
    f.profiled.offer.binding.session = second.clone();
    f.profiled.offer.manifest.bindings.session = second.clone();
    f.profiled.offer.manifest.bindings.attempt_id = attempt;
    f.profiled.offer.manifest.bindings.task_id = task;
    f.profiled.offer.manifest.selection_explanation.digest =
        aq_selection::digest(&f.selection).unwrap();
    f.profiled.offer.binding.execution_spec_digest = aq_selection::digest(
        &json!({"schema_version":1,"kind":"synthetic_profiled_authority_fixture","task_id":task}),
    )
    .unwrap();
    f.profiled.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
    if let Some(grant) = &mut f.profiled.grant {
        grant.binding = f.profiled.offer.binding.clone();
        grant.grant_id = Uuid::new_v4();
    }
    common::profiled::insert_sibling(&f).await;
    let pool = f.db.pool().await;
    let actor = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'viewer@localhost','viewer','dev','viewer@localhost',now())").bind(actor).execute(&pool).await.unwrap();
    for identity in [first, second] {
        let key = aq_server::session_access::session_key(&identity).unwrap();
        sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(key).bind(json!(identity)).bind(actor).bind((NOW+1000)as i64).execute(&pool).await.unwrap();
    }
    let app = aq_server::build_admin_router(f.state.clone());
    let path = "/api/v1/search?q=Remote&limit=1&types=session";
    let (status, body) = read(&app, path, "viewer@localhost").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cursor = group(&body, "session")["next_cursor"]
        .as_str()
        .expect("two visible sessions have a continuation");
    let path = format!("{path}&cursor={cursor}");
    assert_eq!(
        read(&app, &path, "viewer@localhost").await.0,
        StatusCode::OK
    );
    sqlx::query(
        "UPDATE session_inspection_acl SET can_inspect=false,revision=2 WHERE principal_id=$1",
    )
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = read(&app, &path, "viewer@localhost").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "authorization_changed");
}
