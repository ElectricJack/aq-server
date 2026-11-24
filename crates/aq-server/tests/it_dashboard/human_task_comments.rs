//! Task comments (aq-dashboard spec 0004 §4.1): the owner read, the three
//! owner dashboard actions and bootstrap capabilities, over a fake Graph API.
use crate::common;
use aq_server::agent_access::{AgentAccessConfig, Runtime};
use aq_task_comments::{self as comments, Author, Thread};
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode},
    response::Response,
    routing::get,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
use uuid::Uuid;

type Nodes = Arc<Mutex<HashMap<Uuid, Value>>>;
async fn read_node(State(nodes): State<Nodes>, Path((_, id)): Path<(Uuid, Uuid)>) -> Response {
    let (status, body) = match nodes.lock().unwrap().get(&id) {
        Some(n) => (200, n.clone()),
        None => (
            404,
            json!({"error":{"code":"not_found","message":"missing"}}),
        ),
    };
    Response::builder()
        .status(status)
        .header("X-AQ-Graph-API-Version", "1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn task(id: Uuid, w: Uuid) -> Value {
    json!({"id":id,"workspace_id":w,"kind":"task","version":1,"content_revision":1,
        "title":"Commented task","body":"","acceptance_criteria":"","labels":[],"metadata":{},
        "task_state":"active","verification":"unverified","provenance":[],"retired":false,
        "deleted":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z",
        "created_by":"fixture","last_verified_by":null,"last_verified_at":null,
        "retirement_reason":null,"content_type":"note","summary":null,"summary_of_revision":null,
        "valid_from":null,"valid_until":null,"review_at":null,"successor_id":null})
}

struct Fixture {
    f: common::profiled::Fixture,
    nodes: Nodes,
    app: Router,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn setup() -> Fixture {
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let nodes = Nodes::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let graph = Router::new()
        .route("/v1/workspaces/{w}/nodes/{id}", get(read_node))
        .with_state(nodes.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, graph).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("graph-key");
    std::fs::write(&key, b"synthetic-graph-service-key-000000000000").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: url,
            graph_token_file: key,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    // One router for the whole test: read cursors are bound to its boot key.
    let app = aq_server::build_admin_router(f.state.clone());
    Fixture {
        f,
        nodes,
        app,
        server,
        _dir: dir,
    }
}
impl Fixture {
    fn project(&self) -> Uuid {
        self.f.selection.lineage.project_id
    }
    fn task(&self) -> Uuid {
        let id = self.f.selection.lineage.task_id;
        let w = self.f.selection.lineage.workspace_id;
        self.nodes.lock().unwrap().insert(id, task(id, w));
        id
    }
    fn thread(&self) -> Thread {
        Thread {
            project_id: self.project(),
            task_id: self.f.selection.lineage.task_id,
        }
    }
    fn owner(&self) -> Author {
        Author::Owner {
            user_id: self.f.actor,
        }
    }
    async fn call(&self, request: Request<Body>) -> (StatusCode, Value) {
        let res = self.app.clone().oneshot(request).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.get_as(path, "owner@localhost").await
    }
    async fn get_as(&self, path: &str, email: &str) -> (StatusCode, Value) {
        self.call(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }
    /// The owner read of the fixture task's comment thread.
    fn comments(&self) -> String {
        let (p, t) = (self.project(), self.thread().task_id);
        format!("/api/human/projects/{p}/tasks/{t}/comments")
    }
    async fn read(&self, query: &str) -> Value {
        let (status, body) = self.get(&format!("{}{query}", self.comments())).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../protocol/human-graph/v1/schema.json"
        ))
        .unwrap();
        assert!(aq_agent_api::check(&schema, &schema, &body), "{body}");
        assert_eq!(body["contract"], "human-graph-1");
        body["data"].clone()
    }
    async fn act(&self, action: &Value) -> (StatusCode, Value) {
        self.call(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(action.to_string()))
                .unwrap(),
        )
        .await
    }
    /// An agent's comment in the fixture thread.
    async fn add_agent_comment(&self) -> Uuid {
        let agent = Author::Agent {
            session_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
        };
        let now = (self.f.state.clock)();
        let pool = self.f.db.pool().await;
        let added = comments::add(
            &pool,
            Uuid::new_v4(),
            self.thread(),
            &agent,
            "Agent note",
            now,
        );
        added.await.unwrap().comment.comment_id
    }
}
fn add(op: Uuid, project: Uuid, task: Uuid, body: &str) -> Value {
    json!({"operation_id":op,"kind":"task.comment.add","subject":{"type":"task","id":task},
        "expected_versions":{},"payload":{"operation_id":op,"project_id":project,"body":body}})
}
/// An edit of the comment at `revision`, or its delete when `body` is `None`,
/// under a new operation.
fn edit(project: Uuid, comment: Uuid, revision: &str, body: Option<&str>) -> Value {
    let op = Uuid::new_v4();
    let (kind, payload) = match body {
        Some(b) => (
            "task.comment.edit",
            json!({"operation_id":op,"project_id":project,"expected_revision":revision,"body":b}),
        ),
        None => (
            "task.comment.delete",
            json!({"operation_id":op,"project_id":project,"expected_revision":revision}),
        ),
    };
    json!({"operation_id":op,"kind":kind,
        "subject":{"type":"task_comment","id":comment,"revision":revision},
        "expected_versions":{"task_comment":revision},"payload":payload})
}
fn receipt(body: &Value) -> (String, String) {
    let r = &body["data"]["receipt_refs"][0];
    assert_eq!(r["type"], "task_comment", "{body}");
    (
        r["id"].as_str().unwrap().into(),
        r["revision"].as_str().unwrap().into(),
    )
}
fn find_comment(page: &Value, comment: Uuid) -> &Value {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["comment_id"] == json!(comment))
        .unwrap()
}

#[tokio::test]
async fn owner_adds_edits_and_deletes_with_revisions_ownership_and_replay() {
    let x = setup().await;
    let (p, t) = (x.project(), x.task());
    let op = Uuid::new_v4();
    let added = add_replays_its_receipt_and_conflicts_when_changed(&x, op).await;
    let comment = op;
    edit_applies_at_the_exact_revision_only(&x, comment).await;
    let theirs = x.add_agent_comment().await;
    // Owners may not edit or delete an agent's comment.
    for body in [Some("Hijack"), None] {
        let (status, refused) = x.act(&edit(p, theirs, "1", body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert_eq!(refused["code"], "comment_not_author");
    }
    delete_leaves_a_tombstone_and_replays(&x, comment).await;

    // The journal lookup reconciles to the same receipt from a fresh router.
    let (status, looked) = x.get(&format!("/api/v1/operations/{op}")).await;
    assert_eq!(status, StatusCode::OK, "{looked}");
    assert_eq!(
        looked["data"]["receipt_refs"],
        added["data"]["receipt_refs"]
    );

    // A comment outside the project and a task Graph does not know are refused.
    let other = edit(Uuid::new_v4(), theirs, "1", Some("x"));
    let (status, _) = x.act(&other).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = x
        .act(&add(Uuid::new_v4(), p, Uuid::new_v4(), "Nowhere"))
        .await;
    assert!(!status.is_success(), "{status}");
    // Invalid bodies are refused before admission.
    for body in ["   ", &"x".repeat(16385)] {
        let (status, _) = x.act(&add(Uuid::new_v4(), p, t, body)).await;
        assert!(status.is_client_error(), "{status}");
    }
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM task_comments")
        .fetch_one(&x.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(stored, 2);
}

/// Adds the owner's comment under `op`. Replaying the operation returns its
/// receipt; a changed request conflicts. Returns the first reply.
async fn add_replays_its_receipt_and_conflicts_when_changed(x: &Fixture, op: Uuid) -> Value {
    let (p, t) = (x.project(), x.task());
    let request = add(op, p, t, "First **finding**.");
    let (status, body) = x.act(&request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    assert_eq!(receipt(&body), (op.to_string(), "1".into()));
    let affected = body["data"]["affected_entities"].as_array().unwrap();
    assert!(affected.contains(&json!({"type":"task","id":t})));
    let (_, replay) = x.act(&request).await;
    assert_eq!(replay["data"], body["data"]);
    let (status, _) = x.act(&add(op, p, t, "Changed body")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    body
}

/// Edit at the exact revision; a stale revision conflicts with the live one.
async fn edit_applies_at_the_exact_revision_only(x: &Fixture, comment: Uuid) {
    let p = x.project();
    let (_, edited) = x.act(&edit(p, comment, "1", Some("Edited."))).await;
    assert_eq!(edited["data"]["disposition"], "confirmed", "{edited}");
    assert_eq!(receipt(&edited).1, "2");
    let (_, stale) = x.act(&edit(p, comment, "1", Some("Stale."))).await;
    assert_eq!(stale["data"]["disposition"], "conflict", "{stale}");
    assert_eq!(
        stale["data"]["current_versions"],
        json!({"task_comment":"2"})
    );
    let page = x.read("").await;
    let latest = &page["items"][0];
    assert_eq!(latest["body"], "Edited.");
    assert_eq!(latest["revision"], "2");
    assert!(latest["edited_at"].is_string());
    assert_eq!(latest["author"]["kind"], "owner");
    assert_eq!(latest["author"]["display_name"], "owner@localhost");
    assert_eq!(latest["can_edit"], true);
}

/// Delete leaves a tombstone; the deleted comment cannot change again, but
/// replaying the delete still returns its receipt. The agent's newer comment
/// heads the thread, and the owner cannot edit it.
async fn delete_leaves_a_tombstone_and_replays(x: &Fixture, comment: Uuid) {
    let p = x.project();
    let delete = edit(p, comment, "2", None);
    let (_, deleted) = x.act(&delete).await;
    assert_eq!(deleted["data"]["disposition"], "confirmed", "{deleted}");
    assert_eq!(receipt(&deleted).1, "3");
    let (status, again) = x.act(&delete).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again["data"]["receipt_refs"],
        deleted["data"]["receipt_refs"]
    );
    let (status, refused) = x.act(&edit(p, comment, "3", Some("Undelete"))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "comment_deleted");
    let page = x.read("").await;
    let mine = find_comment(&page, comment);
    assert_eq!(mine["body"], Value::Null);
    assert!(mine["deleted_at"].is_string());
    assert_eq!(mine["revision"], "3");
    assert_eq!(mine["can_edit"], false);
    assert_eq!(page["items"][0]["author"]["kind"], "agent");
    assert_eq!(page["items"][0]["can_edit"], false);
}

/// Adds `count` owner comments `c0`, `c1`, ... to the fixture thread.
async fn add_owner_comments(
    x: &Fixture,
    pool: &sqlx::PgPool,
    now: chrono::DateTime<chrono::Utc>,
    count: usize,
) -> Vec<Uuid> {
    let owner = x.owner();
    let mut ids = Vec::new();
    for n in 0..count {
        let body = format!("c{n}");
        let added = comments::add(pool, Uuid::new_v4(), x.thread(), &owner, &body, now);
        ids.push(added.await.unwrap().comment.comment_id);
    }
    ids
}

#[tokio::test]
async fn thread_pages_newest_first_with_tombstones_and_bounds() {
    let x = setup().await;
    let pool = &x.f.db.pool().await;
    let now = (x.f.state.clock)();
    let ids = add_owner_comments(&x, pool, now, 55).await;
    let delete = comments::DeleteComment {
        operation_id: Uuid::new_v4(),
        project_id: x.project(),
        expected_revision: "1".into(),
    };
    comments::apply(pool, &delete, ids[54], &x.owner(), now)
        .await
        .unwrap();
    // Other threads never leak into this one.
    let other = Thread {
        task_id: Uuid::new_v4(),
        ..x.thread()
    };
    comments::add(pool, Uuid::new_v4(), other, &x.owner(), "elsewhere", now)
        .await
        .unwrap();

    let first = x.read("").await;
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 50);
    assert_eq!(items[0]["comment_id"], json!(ids[54]));
    assert_eq!(items[0]["body"], Value::Null);
    assert!(items[0]["deleted_at"].is_string());
    assert_eq!(items[1]["body"], "c53");
    assert_eq!(items[49]["body"], "c5");
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = x.read(&format!("?cursor={cursor}")).await;
    let bodies: Vec<&str> = second["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, ["c4", "c3", "c2", "c1", "c0"]);
    assert_eq!(second["next_cursor"], Value::Null);

    let small = x.read("?limit=100").await;
    assert_eq!(small["items"].as_array().unwrap().len(), 55);
    let base = x.comments();
    for query in ["?limit=0", "?limit=101", "?other=1"] {
        let (status, _) = x.get(&format!("{base}{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    // A cursor for another page size, or a forged one, must restart.
    for query in [
        format!("?cursor={cursor}&limit=10"),
        "?cursor=forged".into(),
    ] {
        let (status, _) = x.get(&format!("{base}{query}")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{query}");
    }
    let (status, _) = x.get_as(&base, "other@localhost").await;
    assert!(status.is_client_error(), "{status}");
}

fn capability<'a>(bootstrap: &'a Value, name: &str) -> Option<&'a Value> {
    bootstrap["data"]["capabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == name)
}

#[tokio::test]
async fn bootstrap_advertises_comment_capabilities() {
    let x = setup().await;
    let (status, body) = x.get("/api/v1/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for name in ["tasks.comments.read", "tasks.comments.write"] {
        let capability = capability(&body, name).unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(capability["enabled"], true, "{capability}");
        assert_eq!(capability["version"], "1");
    }
    let (_, other) = x.get_as("/api/v1/bootstrap", "other@localhost").await;
    let enabled = |name: &str| {
        other["data"]["capabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["name"] == name && c["enabled"] == true)
    };
    assert!(!enabled("tasks.comments.read") && !enabled("tasks.comments.write"));
}
