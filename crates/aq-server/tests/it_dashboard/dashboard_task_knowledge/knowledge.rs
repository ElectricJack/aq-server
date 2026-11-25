use super::*;

#[tokio::test]
async fn knowledge_correct_with_stale_version_conflicts() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    let op = Uuid::new_v4();
    let (status, body) = x.act(&correction(&x, id, "2", op)).await;
    assert_eq!(body["data"]["disposition"], "conflict", "{status} {body}");
    assert_eq!(body["data"]["current_versions"], json!({"knowledge":"3"}));
    assert_eq!(body["data"]["recovery"], "refresh_and_review");
    assert!(x.posts().is_empty(), "a stale version never reaches Graph");
}

#[tokio::test]
async fn knowledge_correct_confirms_with_change_set() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    let mut updated = node(id, x.workspace(), "knowledge", 4);
    updated["title"] = json!("Corrected");
    updated["metadata"]["fraction"] = json!(0.125);
    let result = change_set(vec![updated]);
    x.reply(200, result.clone());
    let op = Uuid::new_v4();
    let (status, body) = x.act(&correction(&x, id, "3", op)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    assert_change_set_receipt(&body, &result);
    assert_eq!(
        body["data"]["affected_entities"][0],
        json!({"type":"knowledge","id":id,"revision":"4"})
    );
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0]["idempotency_key"],
        format!("aq-dashboard-action:{op}")
    );
    assert_eq!(
        posts[0]["operations"],
        json!([{"op":"update_node","id":id,"expected_version":3,"patch":{"title":"Corrected"}}])
    );
}

#[tokio::test]
async fn graph_timeout_after_commit_reconciles_with_original_change_set() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    let mut updated = node(id, x.workspace(), "knowledge", 4);
    updated["metadata"]["fraction"] = json!(0.25);
    let receipt = change_set(vec![updated]);
    x.reply(200, receipt.clone());
    x.graph.timeout_after_commit.store(true, Ordering::SeqCst);
    let op = Uuid::new_v4();
    let request = correction(&x, id, "3", op);
    assert_post_times_out_after_commit(&x, &request).await;
    let pool = x.f.db.pool().await;
    let (key, retained): (String, Vec<u8>) = sqlx::query_as(
        "SELECT idempotency_key,body FROM dashboard_graph_intents WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(key, format!("aq-dashboard-action:{op}"));
    assert_eq!(retained, x.graph.bodies.lock().unwrap()[0]);
    assert!(
        sqlx::query("UPDATE dashboard_graph_intents SET body=$2 WHERE operation_id=$1")
            .bind(op)
            .bind(b"changed".as_slice())
            .execute(&pool)
            .await
            .is_err()
    );

    // Later edits must not change either the replay body or the original receipt.
    x.put(node(id, x.workspace(), "knowledge", 99));
    let resolved = x.lookup(op).await;
    assert_eq!(resolved["data"]["disposition"], "confirmed", "{resolved}");
    assert_change_set_receipt(&resolved, &receipt);
    assert_eq!(resolved["data"]["affected_entities"][0]["revision"], "4");
    assert_eq!(x.posts().len(), 2);
    assert_eq!(
        x.graph.receipts.lock().unwrap().len(),
        1,
        "one native mutation committed"
    );
    assert_eq!(
        x.graph.bodies.lock().unwrap().as_slice(),
        &[retained.clone(), retained]
    );
    let terminal = x.lookup(op).await;
    assert_eq!(terminal["data"], resolved["data"]);
    assert_eq!(x.posts().len(), 2, "confirmed receipts never replay");
}

#[tokio::test]
async fn graph_reconcile_keeps_unknown_on_new_rejection_then_recovers() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    x.reply(
        503,
        json!({"error":{"code":"overloaded","message":"unavailable"}}),
    );
    let op = Uuid::new_v4();
    let (_, admitted) = x.act(&correction(&x, id, "3", op)).await;
    assert_eq!(admitted["data"]["disposition"], "unknown");
    x.reply(
        403,
        json!({"error":{"code":"forbidden","message":"credential revoked"}}),
    );
    assert_eq!(x.lookup(op).await["data"]["disposition"], "unknown");
    x.reply(
        200,
        change_set(vec![node(id, x.workspace(), "knowledge", 4)]),
    );
    assert_eq!(x.lookup(op).await["data"]["disposition"], "confirmed");
    let bodies = x.graph.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3);
    assert!(bodies.iter().all(|body| body == &bodies[0]));
}

#[tokio::test]
async fn graph_reconcile_refuses_changed_service_or_project_binding() {
    let mut x = setup().await;
    let pool = x.f.db.pool().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    x.reply(
        503,
        json!({"error":{"code":"overloaded","message":"unavailable"}}),
    );
    let op = Uuid::new_v4();
    let (_, admitted) = x.act(&correction(&x, id, "3", op)).await;
    assert_eq!(admitted["data"]["disposition"], "unknown");
    let service: Value = sqlx::query_scalar(
        "SELECT service_identity FROM dashboard_graph_intents WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&pool)
    .await
    .unwrap();
    let original_runtime = x.f.state.agent_access.clone();
    x.f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: service["origin"].as_str().unwrap().to_owned(),
            graph_token_file: x._dir.path().join("graph-key"),
            mutation_actor_id: Some("another-actor".into()),
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    assert_eq!(x.lookup(op).await["data"]["disposition"], "unknown");
    assert_eq!(
        x.posts().len(),
        1,
        "a new actor has a different receipt namespace"
    );
    x.f.state.agent_access = original_runtime;
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(x.project())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(x.lookup(op).await["data"]["disposition"], "unknown");
    assert_eq!(
        x.posts().len(),
        1,
        "a changed binding cannot replay old intent"
    );
}

#[tokio::test]
async fn knowledge_verify_forbidden_is_rejected() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 2));
    x.reply(
        403,
        json!({"error":{"code":"forbidden","message":"admin scope required"}}),
    );
    let op = Uuid::new_v4();
    let mut request = action(
        "knowledge.verify",
        Subject::Knowledge(id, "2"),
        json!({"operation_id":op,"project_id":x.project(),"expected_version":"2","verification":"verified",
            "evidence":[{"source_id":"review-1","kind":"url","url":"https://example.test/review","observed_at":"2026-01-01T00:00:00Z","retained":false}]}),
    );
    let (status, missing_reason) = x.act(&request).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "verification needs a reason: {missing_reason}"
    );
    request["reason"] = json!("Reviewed against the source");
    let (status, body) = x.act(&request).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    assert_eq!(x.posts()[0]["operations"][0]["op"], "verify_node");
}

#[tokio::test]
async fn knowledge_author_creates_unverified_note() {
    let x = setup().await;
    let op = Uuid::new_v4();
    let mut created = node(op, x.workspace(), "knowledge", 1);
    created["title"] = json!("New note");
    x.reply(200, change_set(vec![created]));
    let (status, body) = x
        .act(&action(
"knowledge.author",
Subject::Project(x.project(), "1"),
json!({"operation_id":op,"project_id":x.project(),"expected_project_revision":"1","title":"New note","body":"Durable finding"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let operation = &x.posts()[0]["operations"][0];
    assert_eq!(operation["op"], "create_node");
    assert_eq!(operation["id"], json!(op));
    assert_eq!(operation["kind"], "knowledge");
    assert_eq!(operation["verification"], "unverified");
    assert_eq!(operation["content_type"], "note");
    assert_eq!(operation["metadata"]["aq:project_id"], json!(x.project()));
}

#[tokio::test]
async fn retiring_a_retired_record_is_rejected_without_a_graph_write() {
    // QA N-03: a second retirement used to append a revision and overwrite the reason.
    let x = setup().await;
    let id = Uuid::new_v4();
    let mut retired = node(id, x.workspace(), "knowledge", 4);
    retired["retired"] = json!(true);
    retired["retirement_reason"] = json!("Superseded");
    x.put(retired);
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
"knowledge.retire",
Subject::Knowledge(id, "4"),
json!({"operation_id":op,"project_id":x.project(),"expected_version":"4","retirement_reason":"Again"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    assert!(x.posts().is_empty(), "nothing reaches Graph");
}

fn correction(x: &Fixture, id: Uuid, version: &str, op: Uuid) -> Value {
    action(
        "knowledge.correct",
        Subject::Knowledge(id, version),
        json!({
            "operation_id":op,"project_id":x.project(),"expected_version":version,"title":"Corrected"
        }),
    )
}

async fn assert_post_times_out_after_commit(x: &Fixture, request: &Value) {
    let app = aq_server::build_admin_router(x.f.state.clone());
    let pending = tokio::spawn(
        app.oneshot(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        ),
    );
    x.graph.committed.notified().await;
    // Exercise the production POST deadline without a real three-second sleep.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    tokio::time::resume();
    let res = pending.await.unwrap().unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["data"]["disposition"], "unknown", "{body}");
}

fn assert_change_set_receipt(response: &Value, change_set: &Value) {
    let receipt = &response["data"]["receipt_refs"][0];
    assert_eq!(receipt["type"], "graph_change_set");
    assert_eq!(receipt["id"], change_set["change_set_id"]);
}
