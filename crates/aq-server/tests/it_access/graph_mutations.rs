//! AQ-07 real PostgreSQL intent/outbox and bounded public HTTP graph adapter.
use crate::agent_launch::{Launch, enroll, send, state};
use aq_server::{
    AppState,
    agent_access::{AgentAccessConfig, Runtime},
    test_support::TestDb,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
struct Env {
    db: TestDb,
    pool: sqlx::PgPool,
    app: Router,
    admin: Router,
    token: String,
    binding: Value,
    time: Arc<std::sync::atomic::AtomicI64>,
    node_token: String,
    fake: Arc<FakeGraph>,
    state: AppState,
    graph_origin: String,
    _graph_task: tokio::task::JoinHandle<()>,
}
async fn setup() -> Env {
    setup_scoped(false).await
}
async fn setup_scoped(structural: bool) -> Env {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    let (node, node_token) = enroll(&store).await;
    let launch = Launch::new(node, None, |profile, _| {
        profile["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(json!("write.task_content"));
        if structural {
            for edge in ["blocks", "parent-child"] {
                profile["write_scope"]["relationship_types"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(edge));
            }
        }
    });
    launch.install(&pool, node).await;
    let directory = tempfile::tempdir().unwrap();
    let token_file = directory.path().join("graph-token");
    std::fs::write(&token_file, "ordinary-service-credential-0000000000000000").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let fake = Arc::new(FakeGraph::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = format!("http://{}", listener.local_addr().unwrap());
    let service = Router::new().fallback(graph_http).with_state(fake.clone());
    let graph_task = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let runtime = Runtime::new(&AgentAccessConfig {
        mutation_actor_id: Some("api01-writer".into()),
        graph_url: local.clone(),
        graph_token_file: std::env::var_os("AQ_MUTATION_GRAPH_TOKEN_FILE")
            .map(std::path::PathBuf::from)
            .unwrap_or(token_file),
        credential_ttl_ms: 10000,
    })
    .await
    .unwrap();
    let (state, time) = state(store, runtime);
    let app = aq_server::build_worker_router(state.clone());
    let admin = aq_server::build_admin_router(state.clone());
    launch.provision(&admin, None).await;
    let token = launch.issue(&app, &node_token).await;
    Env {
        db,
        pool,
        app,
        admin,
        token,
        binding: launch.binding,
        time,
        node_token,
        fake,
        state,
        graph_origin: local,
        _graph_task: graph_task,
    }
}
use axum::{
    extract::{Request as AxumRequest, State},
    response::{IntoResponse, Response},
};
use std::collections::BTreeMap;
mod changes;
mod cursors;
mod graph;
mod qualified;
mod work_policy;
use changes::Changes;
use graph::{ApiError, FakeGraph, graph_http, graph_response};
fn uid(n: u128) -> String {
    Uuid::from_u128(n).to_string()
}
fn command(e: &Env, commands: Value) -> Value {
    json!({"api_version":1,"operation_id":Uuid::new_v4(),"binding":e.binding,"reason":"Test authorized immutable intent","commands":commands})
}
fn correction(e: &Env) -> Value {
    command(
        e,
        json!([{"kind":"correct_record","id":uid(2),"expected_version":"2","patch":{"title":"Reviewed correction"}}]),
    )
}
fn recovery(e: &Env, op: &Value) -> Value {
    json!({"api_version":1,"request_id":Uuid::new_v4(),"node_id":e.binding["node_id"],"attempt":{"attempt_id":e.binding["attempt_id"],"session_id":e.binding["session_id"],"generation":e.binding["generation"]},"mode":"lookup","operation_ids":[op]})
}
async fn submit(e: &Env, r: Value) -> (StatusCode, Value) {
    send(&e.app, "/v1/agent/graph/commands", Some(&e.token), Some(r)).await
}
#[tokio::test]
async fn confirmed_outbox_replay_byte_conflict_and_truthful_version_conflict() {
    let e = setup().await;
    let r = correction(&e);
    let (status, receipt) = submit(&e, r.clone()).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let change = &receipt["receipt"];
    assert_eq!(
        (
            change["versions"][0]["version"].clone(),
            change["task_completion"].clone()
        ),
        (json!("3"), json!(false))
    );
    assert_eq!(submit(&e, r.clone()).await.1, receipt);
    assert_eq!(e.fake.data.lock().await.applied, 1);
    let mut changed = r.clone();
    changed["commands"][0]["patch"]["title"] = json!("different");
    assert_eq!(submit(&e, changed).await.0, StatusCode::CONFLICT);
    let (status, conflict) = submit(&e, correction(&e)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "id_conflict");
    assert_eq!(e.fake.data.lock().await.applied, 1);
    let agent_graph_outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_graph_outbox")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(agent_graph_outbox, 1);
    let reopened = e.db.store().await;
    drop(reopened);
    assert_eq!(
        send(
            &e.app,
            &format!(
                "/v1/agent/operations/{}",
                r["operation_id"].as_str().unwrap()
            ),
            Some(&e.token),
            None
        )
        .await
        .1,
        receipt
    );
}
#[tokio::test]
async fn response_loss_recovery_after_expiry_and_revocation_replays_exact_intent() {
    let e = setup().await;
    e.fake
        .lose_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = correction(&e);
    let (status, unknown) = submit(&e, r.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{unknown}");
    assert_eq!(unknown["state"], "unknown");
    assert_eq!(e.fake.data.lock().await.applied, 1);
    e.time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(submit(&e, r.clone()).await.0, StatusCode::UNAUTHORIZED);
    let lookup = recovery(&e, &r["operation_id"]);
    let (status, result) = send(
        &e.app,
        "/v2/worker/agent-operations/query",
        Some(&e.node_token),
        Some(lookup.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["operations"][0]["state"], "unknown");
    let (status, replayed) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["operations"][0]["state"], "confirmed");
    let data = e.fake.data.lock().await;
    assert_eq!(data.applied, 1);
    assert_eq!(data.posts.len(), 2);
    assert_eq!(data.posts[0], data.posts[1]);
    drop(data);
    let mut bad = lookup;
    bad["operation_ids"] = json!([Uuid::new_v4()]);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/agent-operations/retry",
            Some(&e.node_token),
            Some(bad)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let agent_graph_operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_graph_operations")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(agent_graph_operations, 1);
}
#[tokio::test]
async fn permissions_protected_authors_and_capabilities_refuse_before_admission() {
    let e = setup().await;
    let mut r = correction(&e);
    r["commands"][0]["id"] = json!(uid(4));
    assert_eq!(submit(&e, r).await.0, StatusCode::NOT_FOUND);
    {
        let mut data = e.fake.data.lock().await;
        data.nodes.get_mut(&uid(2)).unwrap()["created_by"] = json!("foreign");
    }
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::FORBIDDEN);
    {
        let mut data = e.fake.data.lock().await;
        data.nodes.get_mut(&uid(2)).unwrap()["created_by"] = json!("api01-writer");
        data.nodes.get_mut(&uid(2)).unwrap()["verification"] = json!("verified");
    }
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{capabilities}','[\"read.get\"]')").execute(&e.pool).await.unwrap();
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(e.fake.data.lock().await.posts.len(), 0);
    let agent_graph_operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_graph_operations")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(agent_graph_operations, 0);
}
#[tokio::test]
async fn relationship_guard_same_batch_and_findings_created_scope() {
    let e = setup().await;
    let edge = Uuid::new_v4();
    let r = command(
        &e,
        json!([{"kind":"add_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"2","edge_type":"references","source_revision":null,"target_revision":null,"description":"bounded link"}]),
    );
    let (status, body) = submit(&e, r.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = e.fake.data.lock().await;
    assert_eq!(
        data.posts[0]["operations"][0],
        json!({"op":"update_node","id":uid(2),"expected_version":2,"patch":{}})
    );
    assert_eq!(data.posts[0]["operations"][1]["op"], "add_edge");
    drop(data);
    let remove = command(
        &e,
        json!([{"kind":"remove_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"3","expected_version":"1"}]),
    );
    let (status, removed) = submit(&e, remove).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["receipt"]["versions"][0]["kind"], "edge");
    let finding = Uuid::new_v4();
    let r = command(
        &e,
        json!([{"kind":"create_finding","id":finding,"title":"Finding","body":"Attributed knowledge","labels":[],"content_type":"fact"}]),
    );
    let (status, body) = submit(&e, r).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let read = json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":e.binding,"query":{"kind":"get","node_id":finding}});
    let (status, body) = send(&e.app, "/v1/agent/graph/read", Some(&e.token), Some(read)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["node"]["id"], json!(finding));
}
#[tokio::test]
async fn future_guard_versions_and_concurrent_protection_changes_cannot_authorize_a_write() {
    let e = setup().await;
    let mut future = correction(&e);
    future["commands"][0]["expected_version"] = json!("3");
    assert_eq!(submit(&e, future).await.0, StatusCode::CONFLICT);
    assert!(e.fake.data.lock().await.posts.is_empty());
    let source_future = command(
        &e,
        json!([{"kind":"add_relationship","id":Uuid::new_v4(),"source":uid(2),"target":uid(3),"expected_source_version":"3","edge_type":"references","source_revision":null,"target_revision":null,"description":null}]),
    );
    assert_eq!(submit(&e, source_future).await.0, StatusCode::CONFLICT);
    let mut parent_future = proposal(&e);
    parent_future["commands"][0]["expected_parent_version"] = json!("4");
    assert_eq!(submit(&e, parent_future).await.0, StatusCode::CONFLICT);
    assert!(e.fake.data.lock().await.posts.is_empty());
    e.fake
        .race_after_get
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = submit(&e, correction(&e)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["state"], "rejected");
    let d = e.fake.data.lock().await;
    assert_eq!(d.applied, 0);
    assert_eq!(d.nodes[&uid(2)]["verification"], "verified");
    assert_eq!(d.posts[0]["operations"][0]["expected_version"], 2);
}
fn proposal(e: &Env) -> Value {
    command(
        e,
        json!([{"kind":"propose_decomposition","name":"Bounded draft","parent_id":uid(1),"expected_parent_version":"3","tasks":[{"id":Uuid::new_v4(),"title":"Proposed task","body":"Draft only","acceptance_criteria":"No direct apply"}],"relationships":[],"guards":[{"id":uid(3),"expected_version":"2"}]}]),
    )
}
#[tokio::test]
async fn structural_proposals_need_explicit_edge_scopes_and_never_apply() {
    for granted in [false, true] {
        let e = setup_scoped(granted).await;
        let mut r = proposal(&e);
        let task = r["commands"][0]["tasks"][0]["id"].clone();
        r["commands"][0]["relationships"] =
            json!([{"id":Uuid::new_v4(),"source":uid(1),"target":task,"edge_type":"parent-child"}]);
        let (status, body) = submit(&e, r.clone()).await;
        if granted {
            assert_eq!(status, StatusCode::OK, "{body}");
            let draft = &body["receipt"];
            assert_eq!(
                (draft["kind"].clone(), draft["applied"].clone()),
                (json!("graph_draft"), json!(false))
            );
            assert_eq!(e.fake.data.lock().await.nodes.len(), 6);
            assert_eq!(
                send(
                    &e.app,
                    "/v1/agent/graph/changesets/arbitrary/apply",
                    Some(&e.token),
                    Some(json!({}))
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
            let mut future = r.clone();
            future["operation_id"] = json!(Uuid::new_v4());
            future["commands"][0]["guards"][0]["expected_version"] = json!("3");
            assert_eq!(submit(&e, future).await.0, StatusCode::CONFLICT);
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(e.fake.data.lock().await.posts.is_empty());
        }
    }
}
#[tokio::test]
async fn admitted_dispatch_does_not_hold_authority_locks_and_cannot_be_cancelled_as_unsent() {
    let e = setup().await;
    e.fake.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let request = correction(&e);
    let app = e.app.clone();
    let token = e.token.clone();
    let r = request.clone();
    let inflight =
        tokio::spawn(
            async move { send(&app, "/v1/agent/graph/commands", Some(&token), Some(r)).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(3), e.fake.arrived.notified())
        .await
        .unwrap();
    let (_, cancel) = send(
        &e.admin,
        &format!(
            "/api/agent-operations/{}/cancel",
            request["operation_id"].as_str().unwrap()
        ),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(cancel["effect"], "possible");
    let revoke_path = format!(
        "/api/agent-access/{}/revoke",
        e.binding["attempt_id"].as_str().unwrap()
    );
    let revoke = send(&e.admin, &revoke_path, None, Some(json!({})));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), revoke)
            .await
            .expect("HTTP dispatch must not hold authority locks")
            .0,
        StatusCode::OK
    );
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::UNAUTHORIZED);
    e.fake.release.notify_one();
    let (status, result) = inflight.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["state"], "confirmed");
    assert_eq!(
        send(
            &e.admin,
            &format!(
                "/api/agent-operations/{}/retry",
                request["operation_id"].as_str().unwrap()
            ),
            None,
            Some(json!({}))
        )
        .await
        .1,
        result
    );
}

use qualified::qualified_proxy;
#[tokio::test]
#[ignore = "requires scripts/test-agent-mutations.py isolated pinned graph image and ordinary credential"]
async fn actual_qualified_graph_delivery_recovers_lost_receipt_without_duplicate_history() {
    let origin = std::env::var("AQ_MUTATION_GRAPH_URL")
        .expect("isolated qualification wrapper supplies graph");
    let path =
        std::env::var("AQ_MUTATION_GRAPH_TOKEN_FILE").expect("isolated ordinary writer token file");
    let token = std::fs::read_to_string(path).unwrap();
    let mut e = setup_scoped(true).await;
    let after = qualified::verify_replay(&e, &origin, &token).await;
    qualified::verify_stale(&mut e, &origin, &token, after).await;
    qualified::verify_finding_and_relation(&e).await;
    qualified::verify_draft(&e, &origin, &token).await;
}
async fn admit_only(e: &Env, request: &Value) -> (StatusCode, Value) {
    let response = e
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/agent/graph/commands")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", e.token))
                .header("Prefer", "respond-async")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn queued_admission_cancellation_wins_before_first_dispatch() {
    let e = setup().await;
    let request = correction(&e);
    let (status, queued) = admit_only(&e, &request).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["effect"], "not_dispatched");
    assert!(e.fake.data.lock().await.posts.is_empty());
    let node: Uuid = e.binding["node_id"].as_str().unwrap().parse().unwrap();
    let mut tx = e.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .execute(&mut *tx)
        .await
        .unwrap();
    let app = e.admin.clone();
    let retry_path = format!(
        "/api/agent-operations/{}/retry",
        request["operation_id"].as_str().unwrap()
    );
    let dispatch =
        tokio::spawn(async move { send(&app, &retry_path, None, Some(json!({}))).await });
    tokio::task::yield_now().await;
    let cancel_path = format!(
        "/api/agent-operations/{}/cancel",
        request["operation_id"].as_str().unwrap()
    );
    let (status, cancel) = send(&e.admin, &cancel_path, None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(cancel["effect"], "not_applied");
    tx.commit().await.unwrap();
    assert_eq!(dispatch.await.unwrap().1, cancel);
    assert!(e.fake.data.lock().await.posts.is_empty());
}
#[tokio::test]
async fn retained_budget_refuses_new_intents_but_preserves_exact_replay() {
    let e = setup().await;
    let request = correction(&e);
    assert_eq!(admit_only(&e, &request).await.0, StatusCode::ACCEPTED);
    // A conservative retained reservation can exhaust the byte budget without a
    // large payload fixture. Admission must count it, not recalculate/prune it.
    sqlx::query("UPDATE agent_graph_operations SET retained_bytes=33554432")
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(admit_only(&e, &request).await.0, StatusCode::ACCEPTED);
    let (status, body) = admit_only(&e, &correction(&e)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "overloaded");
    let agent_graph_operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_graph_operations")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(agent_graph_operations, 1);
    assert!(e.fake.data.lock().await.posts.is_empty());
}
#[tokio::test]
async fn recovery_cursor_freezes_membership_and_serializes_its_finite_quota() {
    let e = setup().await;
    let request = cursors::membership(&e).await;
    cursors::quota(&e, request).await;
}
#[tokio::test]
async fn retry_budget_and_changed_service_mapping_hold_unknown_without_new_dispatch() {
    let e = setup().await;
    e.fake
        .lose_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let request = correction(&e);
    assert_eq!(submit(&e, request.clone()).await.1["state"], "unknown");
    let temp = tempfile::tempdir().unwrap();
    let token_file = temp.path().join("replacement-service");
    std::fs::write(
        &token_file,
        "replacement-ordinary-credential-0000000000000000",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut state = e.state.clone();
    state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: e.graph_origin.clone(),
            graph_token_file: token_file,
            mutation_actor_id: Some("api01-writer".into()),
            credential_ttl_ms: 10000,
        })
        .await
        .unwrap(),
    ));
    let replacement = aq_server::build_worker_router(state);
    e.time.store(1_002_000, std::sync::atomic::Ordering::SeqCst);
    let lookup = recovery(&e, &request["operation_id"]);
    let (_, held) = send(
        &replacement,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup.clone()),
    )
    .await;
    assert_eq!(held["operations"][0]["state"], "unknown");
    assert_eq!(
        held["operations"][0]["recovery"]["action"],
        "manual_reconciliation"
    );
    assert_eq!(e.fake.data.lock().await.posts.len(), 1);
    e.time.store(1_031_000, std::sync::atomic::Ordering::SeqCst);
    let (_, exhausted) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup),
    )
    .await;
    assert_eq!(
        exhausted["operations"][0]["recovery"]["remaining_elapsed_ms"],
        0
    );
    assert_eq!(
        exhausted["operations"][0]["recovery"]["action"],
        "manual_reconciliation"
    );
    assert_eq!(e.fake.data.lock().await.posts.len(), 1);
    let agent_graph_outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_graph_outbox")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(agent_graph_outbox, 0);
}
#[tokio::test]
async fn local_receipt_commit_failure_keeps_unknown_and_recovery_publishes_once() {
    let e = setup().await;
    sqlx::raw_sql("CREATE FUNCTION aq07_fail_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic local receipt fault'; END $$; CREATE TRIGGER aq07_fail_outbox BEFORE INSERT ON agent_graph_outbox FOR EACH ROW EXECUTE FUNCTION aq07_fail_outbox();").execute(&e.pool).await.unwrap();
    let request = correction(&e);
    let (status, unknown) = submit(&e, request.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{unknown}");
    assert_eq!(
        (unknown["state"].clone(), unknown["effect"].clone()),
        (json!("unknown"), json!("possible"))
    );
    assert_eq!(e.fake.data.lock().await.applied, 1);
    let agent_graph_outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_graph_outbox")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(agent_graph_outbox, 0);
    let state: Value = sqlx::query_scalar("SELECT status FROM agent_graph_operations")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(
        state["state"], "unknown",
        "failed local transaction cannot publish a confirmed status without its outbox"
    );
    sqlx::query("DROP TRIGGER aq07_fail_outbox ON agent_graph_outbox")
        .execute(&e.pool)
        .await
        .unwrap();
    e.time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
    let lookup = recovery(&e, &request["operation_id"]);
    let (_, body) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup),
    )
    .await;
    assert_eq!(body["operations"][0]["state"], "confirmed");
    assert_eq!(e.fake.data.lock().await.applied, 1);
    let agent_graph_outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_graph_outbox")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(agent_graph_outbox, 1);
}

#[tokio::test]
async fn work_kind_policy_fences_raw_task_mutations_but_preserves_knowledge_authoring() {
    for kind in ["investigation", "fidelity_review"] {
        let mut e = setup_scoped(true).await;
        if e.fake.upstream.is_some() {
            return;
        }
        let task = work_policy::install(&mut e, kind).await;
        work_policy::denies_task_changes(&e, kind, task).await;
        work_policy::permits_knowledge(&e, kind).await;
    }
}
