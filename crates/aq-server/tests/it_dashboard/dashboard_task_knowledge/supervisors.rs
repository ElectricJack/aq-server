use super::*;

use aq_central_authority::{
    ArtifactRef, CentralPlan, Clock, Digest, Document, Event, HostLaunchRef, Run, Text, owner::*,
};
struct Verifier;
#[async_trait::async_trait]
impl EvidenceVerifier for Verifier {
    async fn authorize_plan(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &CentralPlan,
    ) -> OwnerResult<()> {
        Ok(())
    }
    async fn verify_event(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &Run,
        _: &Event,
    ) -> OwnerResult<()> {
        Ok(())
    }
}

async fn supervisor_token(x: &Fixture) -> String {
    let examples: Value = serde_json::from_str(include_str!(
        "../../../../aq-central-authority/artifacts/examples.json"
    ))
    .unwrap();
    let Document::Plan(plan) =
        aq_central_authority::decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
    else {
        panic!("plan")
    };
    let actor = plan.subject.actor_id();
    let pool = x.f.db.pool().await;
    configure_reservation_actor(x, &pool, actor).await;
    let sessions = SessionOwner::new(pool.clone(), Verifier);
    let clock = Clock {
        machine_boot_id: Uuid::from_u128(9),
        boot_ms: 1000,
    };
    sessions.prepare(plan.clone(), clock.clone()).await.unwrap();
    let launch = HostLaunchRef {
        binding: plan.binding().unwrap(),
        launch_id: Uuid::new_v4(),
        intent: ArtifactRef {
            id: Text::new("fixture").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"fixture"),
        },
    };
    sessions
        .start(
            &plan.binding().unwrap(),
            launch,
            PermitWindow::new(60_000, clock),
        )
        .await
        .unwrap();
    aq_server::agent_access::supervisor_conversation::reply::issue_credential(
        &pool,
        actor,
        60_000,
        (x.f.state.clock)().timestamp_millis() as u64,
    )
    .await
    .unwrap()
}

async fn machine(
    x: &Fixture,
    token: &str,
    path: &str,
    request: Option<&Value>,
) -> (StatusCode, Value) {
    let res = aq_server::build_worker_router(x.f.state.clone())
        .oneshot(
            Request::builder()
                .method(if request.is_some() { "POST" } else { "GET" })
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    request.map(Value::to_string).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}

#[tokio::test]
async fn supervisor_creates_graph_with_dependencies_idempotently() {
    let x = setup().await;
    let token = supervisor_token(&x).await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    x.reply(
        200,
        change_set(vec![
            node(a, x.workspace(), "task", 1),
            node(b, x.workspace(), "task", 1),
            edge(e, x.workspace(), a, b),
        ]),
    );
    let request = graph_create(x.project(), a, b, e);
    let (status, receipt) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
    assert_eq!(
        receipt["data"]["affected_entities"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(x.posts().len(), 1);
    assert_eq!(x.posts()[0]["operations"].as_array().unwrap().len(), 3);
    assert_eq!(
        x.posts()[0]["operations"][0]["metadata"]["aq:priority"],
        300
    );
    let (_, replay) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(replay["data"], receipt["data"]);
    let path = format!(
        "/api/v1/operations/{}",
        request["operation_id"].as_str().unwrap()
    );
    let (status, lookup) = machine(&x, &token, &path, None).await;
    assert_eq!(status, 200, "{lookup}");
    assert_eq!(lookup["data"], receipt["data"]);
    let mut changed = request.clone();
    changed["payload"]["tasks"][0]["title"] = json!("Changed");
    assert_eq!(
        machine(&x, &token, "/api/v1/actions", Some(&changed))
            .await
            .0,
        409
    );
    assert_eq!(x.act(&request).await.0, 404);
    assert_eq!(
        machine(&x, "invalid", "/api/v1/actions", Some(&request))
            .await
            .0,
        401
    );
    // Synthetic reduced grant: production credentials are otherwise immutable.
    sqlx::query("ALTER TABLE central_supervisor_credentials DISABLE TRIGGER central_supervisor_credential_guard").execute(&x.f.db.pool().await).await.unwrap();
    sqlx::query("UPDATE central_supervisor_credentials SET scopes=ARRAY['graph.read']")
        .execute(&x.f.db.pool().await)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE central_supervisor_credentials ENABLE TRIGGER central_supervisor_credential_guard").execute(&x.f.db.pool().await).await.unwrap();
    assert_eq!(machine(&x, &token, &path, None).await.0, 403);
    assert_eq!(
        machine(&x, &token, "/api/v1/actions", Some(&request))
            .await
            .0,
        403
    );
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn supervisor_single_create_and_credential_denials() {
    let mut x = setup().await;
    let token = supervisor_token(&x).await;
    let id = Uuid::new_v4();
    let request = action(
        "task.create",
        Subject::Project(x.project(), "1"),
        json!({
            "operation_id":id,"expected_project_revision":"1","title":"Scoped single","body":"","acceptance_criteria":"","labels":[],"task_state":"draft","priority":42
        }),
    );
    x.reply(200, change_set(vec![node(id, x.workspace(), "task", 1)]));
    let (status, receipt) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    let metadata = x.posts()[0]["operations"][0]["metadata"].clone();
    assert_eq!(metadata["aq:priority"], 42);
    assert!(metadata.get("aq:human_actor_id").is_none());
    assert!(metadata["aq:supervisor_session_id"].is_string());
    let path = format!("/api/v1/operations/{id}");
    let pool = x.f.db.pool().await;
    let principal: (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT owner_id,supervisor_session_id FROM dashboard_operations WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(principal.0, None);
    assert_eq!(
        json!(principal.1.unwrap()),
        metadata["aq:supervisor_session_id"]
    );
    // An Owner account credential never authenticates on the machine route.
    assert_eq!(machine(&x, "owner@localhost", &path, None).await.0, 401);
    let mut edit = request.clone();
    edit["kind"] = json!("task.edit");
    assert_eq!(
        machine(&x, &token, "/api/v1/actions", Some(&edit)).await.0,
        422
    );
    let now = (x.f.state.clock)().timestamp_millis();
    sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=$1")
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    assert_credential_denied(&x, &token, &path, &request).await;
    // Issue a replacement credential, then advance the injected wall clock.
    let actor: Uuid =
        sqlx::query_scalar("SELECT actor_id FROM central_supervisor_credentials LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let expired = aq_server::agent_access::supervisor_conversation::reply::issue_credential(
        &pool, actor, 60_000, now as u64,
    )
    .await
    .unwrap();
    x.f.state.clock =
        Arc::new(move || chrono::DateTime::from_timestamp_millis(now + 60_000).unwrap());
    assert_credential_denied(&x, &expired, &path, &request).await;
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn graph_create_counts_parent_edges_at_atomic_limit() {
    let x = setup().await;
    let parent = Uuid::new_v4();
    let mut tasks: Vec<_> = (0..99)
        .map(|i| json!({"id":Uuid::new_v4(),"title":format!("Task {i}")}))
        .collect();
    tasks[1]["parent_id"] = tasks[0]["id"].clone();
    let request = action(
        "task.graph.create",
        Subject::Project(x.project(), "1"),
        json!({
            "operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"1","tasks":tasks
        }),
    );
    // 99 nodes + one parent edge fit; another parent would exceed the limit.
    let mut oversized = request.clone();
    oversized["payload"]["tasks"][2]["parent_id"] = json!(parent);
    assert_eq!(x.act(&oversized).await.0, 422);
    assert!(x.posts().is_empty());
    let mut results: Vec<_> = tasks
        .iter()
        .map(|t| {
            node(
                t["id"].as_str().unwrap().parse().unwrap(),
                x.workspace(),
                "task",
                1,
            )
        })
        .collect();
    results.push(edge(Uuid::new_v4(), x.workspace(), parent, parent));
    x.reply(200, change_set(results));
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"].as_array().unwrap().len(), 100);
    assert_eq!(posts[0]["operations"][99]["edge_type"], "parent-child");
    assert_eq!(posts[0]["operations"][99]["source"], tasks[0]["id"]);
    assert_eq!(posts[0]["operations"][99]["target"], tasks[1]["id"]);
}

#[tokio::test]
async fn supervisor_graph_lost_response_replays_original_batch() {
    let x = setup().await;
    let token = supervisor_token(&x).await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let request = graph_create(x.project(), a, b, e);
    let op: Uuid = request["operation_id"].as_str().unwrap().parse().unwrap();
    let receipt = change_set(vec![
        node(a, x.workspace(), "task", 1),
        node(b, x.workspace(), "task", 1),
        edge(e, x.workspace(), a, b),
    ]);
    x.reply(200, receipt.clone());
    x.graph.timeout_after_commit.store(true, Ordering::SeqCst);
    let app = aq_server::build_worker_router(x.f.state.clone());
    let mut pending = tokio::spawn(
        app.oneshot(
            Request::post("/api/v1/actions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        ),
    );
    tokio::select! {
        _ = x.graph.committed.notified() => {},
        result = &mut pending => panic!("request ended before Graph committed: {result:?}"),
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    tokio::time::resume();
    let response = pending.await.unwrap().unwrap();
    assert_eq!(response.status(), 202);
    let unknown: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(unknown["data"]["disposition"], "unknown");
    let (status, recovered) = machine(&x, &token, &format!("/api/v1/operations/{op}"), None).await;
    assert_eq!(status, 200, "{recovered}");
    assert_eq!(recovered["data"]["disposition"], "confirmed");
    assert_eq!(
        recovered["data"]["receipt_refs"][0]["id"],
        receipt["change_set_id"]
    );
    let (_, replay) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(replay["data"], recovered["data"]);
    let bodies = x.graph.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(
        x.graph.receipts.lock().unwrap().len(),
        1,
        "one atomic commit"
    );
}

#[tokio::test]
async fn enqueue_principal_migration_preserves_retired_tombstones() {
    use aq_server::test_support::{TestDb, migrate_profile};
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 56).await.unwrap();
    let principal = Uuid::new_v4();
    let operation = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'migration@example.test','owner',now())")
        .bind(principal)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at,retired_at) VALUES($1,$2,'task.create','{\"kind\":\"project\"}', $3,'confirmed',NULL,'1970-01-01','1970-01-01','1971-01-01')")
        .bind(operation).bind(principal).bind(format!("sha256:{}", "0".repeat(64))).execute(&pool).await.unwrap();
    let before: Value =
        sqlx::query_scalar("SELECT to_jsonb(d) FROM dashboard_operations d WHERE operation_id=$1")
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
    migrate_profile(&pool, aq_server::STATE_FORMAT_VERSION)
        .await
        .expect("retired operation receipts must upgrade");
    let mut after: Value =
        sqlx::query_scalar("SELECT to_jsonb(d) FROM dashboard_operations d WHERE operation_id=$1")
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after["owner_id"], json!(principal));
    assert!(after["supervisor_session_id"].is_null());
    after.as_object_mut().unwrap().remove("owner_id");
    after
        .as_object_mut()
        .unwrap()
        .remove("supervisor_session_id");
    assert_eq!(
        before, after,
        "immutable tombstone identity must survive exactly"
    );
    assert!(
        sqlx::query("UPDATE dashboard_operations SET scope='{}' WHERE operation_id=$1")
            .bind(operation)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE dashboard_operations SET owner_id=NULL WHERE operation_id=$1")
            .bind(operation)
            .execute(&pool)
            .await
            .is_err()
    );
}

async fn configure_reservation_actor(x: &Fixture, pool: &sqlx::PgPool, actor: Uuid) {
    // Managed credentials resolve scope through the reservation actor registry.
    // This synthetic session fixture must configure that actor just as the
    // production supervisor controller does before issuing its credential.
    use aq_server::agent_access::supervisor_conversation::reservations::{
        Configure, LocalPolicy, PolicyKind, ReservationOwner,
    };
    let user = x.f.state.store.find_user(x.f.actor).await.unwrap().unwrap();
    let conversation: Uuid =
        sqlx::query_scalar("SELECT conversation_id FROM global_supervisor_conversation")
            .fetch_one(pool)
            .await
            .unwrap();
    ReservationOwner::new(&x.f.state)
        .configure_actor(
            &user,
            &serde_json::to_vec(&Configure {
                operation_id: Uuid::new_v4(),
                actor_id: actor,
                expected_actor_revision: 0,
                policy: LocalPolicy {
                    schema_version: 1,
                    kind: PolicyKind::GlobalSupervisorReservation,
                    actor_id: actor,
                    conversation_id: conversation,
                    enabled: true,
                },
            })
            .unwrap(),
        )
        .await
        .unwrap();
}

async fn assert_credential_denied(x: &Fixture, token: &str, path: &str, request: &Value) {
    assert_eq!(machine(x, token, path, None).await.0, 401);
    assert_eq!(
        machine(x, token, "/api/v1/actions", Some(request)).await.0,
        401
    );
}
