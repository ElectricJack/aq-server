mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::profiled::fixture;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn action_union_rejects_unknown_kind_with_envelope() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let response = app
        .oneshot(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"operation_id":Uuid::new_v4(),"kind":"shell.run",
            "subject":{"type":"project","id":Uuid::new_v4().to_string()},
            "expected_versions":{},"payload":{}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_action_kind");
    assert!(body["request_id"].is_string());
    assert_eq!(body["retryable"], false);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dashboard_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn v1_outer_rejections_and_validation_use_common_safe_errors() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    for (path, method, body, csrf, wanted) in [
        ("/api/v1/actions", "POST", "{}", false, 403),
        ("/api/v1/actions", "POST", "{", true, 422),
        ("/api/v1/actions", "POST", "{}", true, 422),
        ("/api/v1/operations/not-a-uuid", "GET", "", true, 422),
        ("/api/v1/missing", "GET", "", true, 404),
        ("/api/v1/sessions?cursor=bad", "GET", "", true, 410),
    ] {
        let mut request = Request::builder()
            .uri(path)
            .method(method)
            .header("x-aq-dev-email", "owner@localhost")
            .header("content-type", "application/json");
        if csrf {
            request = request.header("x-aq-request", "1");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), wanted, "{path}");
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(body["request_id"].is_string(), "{path}: {body}");
        assert!(body["code"].is_string(), "{path}: {body}");
        assert!(body["retryable"].is_boolean());
        assert!(body["message"].as_str().unwrap().len() <= 256);
    }
}

#[tokio::test]
async fn production_registry_is_enabled_and_lookup_is_neutral() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/bootstrap")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let capability = body["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "dashboard.actions")
        .unwrap();
    assert_eq!(capability["enabled"], true);
    assert_eq!(capability["operation_retention"]["minimum_days"], 90);
    assert_eq!(
        capability["operation_retention"]["unresolved_indefinite"],
        true
    );
    let response = app
        .oneshot(
            Request::get(format!("/api/v1/operations/{}", Uuid::new_v4()))
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["code"], "not_found");
    assert!(body.get("operation_id").is_none());
}

async fn call(
    app: &axum::Router,
    path: &str,
    body: Option<&Value>,
    email: &str,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(if body.is_some() { "POST" } else { "GET" })
                .uri(path)
                .header("x-aq-dev-email", email)
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(body.map(Value::to_string).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    (status, body)
}
fn message_action(id: &Value, payload: Value) -> Value {
    json!({"operation_id":payload["operation_id"],"kind":"supervisor.message.send",
        "subject":{"type":"conversation","id":id,"revision":payload["expected_conversation_revision"]},
        "expected_versions":{"conversation":payload["expected_conversation_revision"]},"payload":payload})
}

#[tokio::test]
async fn supervisor_action_and_legacy_share_one_enqueue_and_retained_operation() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, discovery) = call(
        &app,
        "/api/human/conversations/global",
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{discovery}");
    let id = &discovery["conversation"]["id"];
    for legacy_first in [false, true] {
        let payload = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),
            "expected_conversation_revision":"1","body":"Hello ☃","context":[],"attachments":[]});
        let action = message_action(id, payload.clone());
        let legacy = format!("/api/human/conversations/{}/messages", id.as_str().unwrap());
        if legacy_first {
            let (status, receipt) = call(&app, &legacy, Some(&payload), "owner@localhost").await;
            assert_eq!(status, StatusCode::OK, "{receipt}");
        }
        let (status, result) =
            call(&app, "/api/v1/actions", Some(&action), "owner@localhost").await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["data"]["disposition"], "confirmed");
        assert_eq!(result["data"]["affected_entities"][0]["type"], "message");
        assert_eq!(
            result["data"]["affected_entities"][0]["id"],
            payload["message_id"]
        );
        let (status, receipt) = call(&app, &legacy, Some(&payload), "owner@localhost").await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt["state"], "queued");
        assert_eq!(receipt["host_delivery"], "not_attempted");
        assert!(receipt["agent_receipt"].is_null());
        let (_, replay) = call(&app, "/api/v1/actions", Some(&action), "owner@localhost").await;
        assert_eq!(replay["data"], result["data"]);
        let (_, lookup) = call(
            &app,
            &format!(
                "/api/v1/operations/{}",
                payload["operation_id"].as_str().unwrap()
            ),
            None,
            "owner@localhost",
        )
        .await;
        assert_eq!(lookup["data"], result["data"]);
        let mut changed = action.clone();
        changed["payload"]["body"] = json!("changed");
        assert_eq!(
            call(&app, "/api/v1/actions", Some(&changed), "owner@localhost")
                .await
                .0,
            StatusCode::CONFLICT
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_operations WHERE response->>'contract'='human-supervisor-conversation-1'")
        .fetch_one(&f.db.pool().await).await.unwrap();
    assert_eq!(count, 2);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dashboard_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn bootstrap_lists_pending_operations() {
    use aq_server::dashboard_api::actions::{Disposition, Operation, Recovery};
    use aq_server::dashboard_api::contract::Scope;
    let f = fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(other).execute(&pool).await.unwrap();
    let mut expected = vec![];
    for n in 0..106u128 {
        let id = Uuid::from_u128(n + 1);
        let principal = if n >= 104 { other } else { f.actor };
        let disposition = match n {
            102 => Disposition::Confirmed,
            103 => Disposition::Rejected,
            n if n % 2 == 0 => Disposition::Pending,
            _ => Disposition::Unknown,
        };
        if n < 100 {
            expected.push(id.to_string());
        }
        let op = Operation {
            operation_id: id,
            disposition,
            request_digest: format!("sha256:{}", "a".repeat(64)),
            scope: Scope::AllAuthorized,
            created_at: "1970-01-01T00:16:40.000Z".into(),
            updated_at: "1970-01-01T00:16:40.000Z".into(),
            subject_refs: vec![],
            receipt_refs: vec![],
            affected_entities: vec![],
            recovery: Recovery::LookupOriginal,
            current_versions: Default::default(),
        };
        if (n >= 100 || n % 3 != 0) && n != 103 && n != 105 {
            sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,'supervisor.message.send',$3,$4,$5,$6,to_timestamp(1000),to_timestamp(1000))")
            .bind(id).bind(principal).bind(json!(op.scope)).bind(&op.request_digest).bind(serde_json::to_value(disposition).unwrap().as_str().unwrap()).bind(json!(op)).execute(&pool).await.unwrap();
        }
        // Discovery reads only native admission metadata. These rows deliberately
        // omit the rest of the native evidence; real decoding is tested with Graph.
        if n < 100 && (n % 3 == 0 || n == 1) || n >= 103 {
            sqlx::query("INSERT INTO human_graph_operations(operation_id,actor_id,project_id,project_revision,project_binding,workspace_id,task_id,request_bytes,request_digest,payload_bytes,payload_digest,service_identity,status,created_at_ms,expires_at_ms,retained_bytes) VALUES($1,$2,$3,1,'{}',$4,$1,$5,$6,$5,$6,'{}',$7,1000000,1001000,1)")
                .bind(id).bind(principal).bind(f.selection.lineage.project_id).bind(f.selection.lineage.workspace_id).bind(b"{}".as_slice()).bind(format!("sha256:{}", "a".repeat(64))).bind(json!({"state":serde_json::to_value(disposition).unwrap()})).execute(&pool).await.unwrap();
        }
    }
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, b) = call(&app, "/api/v1/bootstrap", None, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["api_version"], "dashboard-read-1");
    assert_eq!(b["pending_operations"], json!(expected));
    assert_eq!(b["coverage"]["state"], "partial");
    assert_eq!(b["data"]["contracts"][1]["name"], "dashboard-actions");
    assert_eq!(b["data"]["contracts"][1]["version"], "dashboard-actions-1");
    let bytes = aq_server::dashboard_api::actions::contract::schema_bytes();
    use sha2::{Digest, Sha256};
    assert_eq!(
        b["data"]["contracts"][1]["digest"],
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    );
    let (_, other_bootstrap) = call(&app, "/api/v1/bootstrap", None, "other@localhost").await;
    assert_eq!(
        other_bootstrap["pending_operations"],
        json!([Uuid::from_u128(105), Uuid::from_u128(106)])
    );
}

#[tokio::test]
async fn supervisor_action_lookup_reconciles_native_commit_without_reenqueue() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, d) = call(
        &app,
        "/api/human/conversations/global",
        None,
        "owner@localhost",
    )
    .await;
    let action = message_action(
        &d["conversation"]["id"],
        json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),
        "expected_conversation_revision":"1","body":"Durable admission only","context":[],"attachments":[]}),
    );
    let pool = f.db.pool().await;
    sqlx::query("CREATE FUNCTION refuse_finish() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test interruption'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_finish BEFORE UPDATE ON dashboard_operations FOR EACH ROW EXECUTE FUNCTION refuse_finish()").execute(&pool).await.unwrap();
    let (status, admitted) = call(&app, "/api/v1/actions", Some(&action), "owner@localhost").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
    assert_eq!(admitted["data"]["disposition"], "pending");
    sqlx::query("DROP TRIGGER refuse_finish ON dashboard_operations")
        .execute(&pool)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!(
        "/api/v1/operations/{}",
        action["operation_id"].as_str().unwrap()
    );
    let (status, recovered) = call(&app, &path, None, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{recovered}");
    assert_eq!(recovered["data"]["disposition"], "confirmed");
    assert_eq!(
        recovered["data"]["request_digest"],
        admitted["data"]["request_digest"]
    );
    let (_, replay) = call(&app, "/api/v1/actions", Some(&action), "owner@localhost").await;
    assert_eq!(replay["data"], recovered["data"]);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mailbox_operations WHERE operation_id=$1")
            .bind(
                action["operation_id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap(),
            )
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, &path, None, "owner@localhost").await.0,
        StatusCode::NOT_FOUND
    );
    let (_, b) = call(&app, "/api/v1/bootstrap", None, "owner@localhost").await;
    assert_eq!(b["pending_operations"], json!([]));
    assert!(
        !b["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "dashboard.actions")
            .unwrap()["enabled"]
            .as_bool()
            .unwrap()
    );
}

#[tokio::test]
async fn unknown_fields_duplicate_keys_and_oversized_bodies_are_rejected_before_admission() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let base = json!({"operation_id":Uuid::new_v4(),"kind":"test.effect","subject":{"type":"test","id":"one"},"expected_versions":{},"payload":{}});
    let mut extra = base.clone();
    extra["command"] = json!("never");
    for (raw, status) in [
        (extra.to_string(), 422),
        (
            format!("{{\"kind\":\"test.effect\",{}", &base.to_string()[1..]),
            422,
        ),
        (" ".repeat(65537), 413),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/actions")
                    .header("x-aq-dev-email", "owner@localhost")
                    .header("x-aq-request", "1")
                    .header("content-type", "application/json")
                    .body(Body::from(raw))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(body["request_id"].is_string());
        assert!(body["message"].as_str().unwrap().len() <= 256);
        assert!(body.get("operation_id").is_none());
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dashboard_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn supervisor_union_bounds_identity_and_concurrent_legacy_admission() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, discovery) = call(
        &app,
        "/api/human/conversations/global",
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{discovery}");
    let conversation = &discovery["conversation"]["id"];
    let payload = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":"é".repeat(8192),"context":[],"attachments":[]});
    let original = message_action(conversation, payload.clone());
    for bad in [
        {
            let mut b = original.clone();
            b["payload"]["body"] = json!("é".repeat(8193));
            b
        },
        {
            let mut b = original.clone();
            b["payload"]["context"] = json!([{}]);
            b
        },
        {
            let mut b = original.clone();
            b["payload"]["attachments"] = json!([{}]);
            b
        },
        {
            let mut b = original.clone();
            b["payload"]["message_id"] = payload["operation_id"].clone();
            b
        },
        {
            let mut b = original.clone();
            b["payload"]["unexpected"] = json!(true);
            b
        },
        {
            let mut b = original.clone();
            b["expected_versions"]["conversation"] = json!("2");
            b
        },
    ] {
        let (status, body) = call(&app, "/api/v1/actions", Some(&bad), "owner@localhost").await;
        assert_eq!(status, 422, "{body}");
    }
    let legacy = format!(
        "/api/human/conversations/{}/messages",
        conversation.as_str().unwrap()
    );
    let (union, native) = tokio::join!(
        call(&app, "/api/v1/actions", Some(&original), "owner@localhost"),
        call(&app, &legacy, Some(&payload), "owner@localhost")
    );
    assert!(matches!(union.0.as_u16(), 200 | 202), "{union:?}");
    // A racing legacy handler may arrive before native admission. It returns a
    // retryable native error; neither route can authorize another dispatch.
    assert!(matches!(native.0.as_u16(), 200 | 503), "{native:?}");
    let id = payload["operation_id"].as_str().unwrap();
    let (status, retained) = call(
        &app,
        &format!("/api/v1/operations/{id}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{retained}");
    assert_eq!(retained["data"]["disposition"], "confirmed");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(other).execute(&f.db.pool().await).await.unwrap();
    let (_, hidden) = call(
        &app,
        &format!("/api/v1/operations/{id}"),
        None,
        "other@localhost",
    )
    .await;
    assert_eq!(hidden["code"], "not_found");
    assert!(hidden.get("operation_id").is_none());
}

#[tokio::test]
async fn session_message_send_enqueues_to_bound_session_mailbox() {
    let f = fixture().await;
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, list) = call(&app, "/api/v1/sessions", None, "owner@localhost").await;
    let session = list["data"]["items"][0]["id"].clone();
    let path = format!("/api/v1/sessions/{}", session.as_str().unwrap());
    let (_, detail) = call(&app, &path, None, "owner@localhost").await;
    assert_eq!(detail["data"]["permissions"], json!(["session.inspect"]));
    let l = &f.selection.lineage;
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}')")
        .bind(attempt).bind(f.node.node_id)
        .bind(json!({"task_id":l.task_id,"project_id":l.project_id,"workspace_id":l.workspace_id,"attempt_id":attempt,"session_id":session,"generation":1}))
        .execute(&pool).await.unwrap();
    let (_, detail) = call(&app, &path, None, "owner@localhost").await;
    assert_eq!(
        detail["data"]["permissions"],
        json!(["session.inspect", "session.message.send", "session.nudge"])
    );
    let (_, b) = call(&app, "/api/v1/bootstrap", None, "owner@localhost").await;
    assert!(
        b["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "session.message.enqueue" && c["enabled"] == true)
    );
    let action = |generation: &str, body: &str, op: Uuid, message: Uuid| {
        json!({"operation_id":op,"kind":"session.message.send",
            "subject":{"type":"session","id":session,"revision":generation},
            "expected_versions":{"session":generation},
            "payload":{"operation_id":op,"project_id":l.project_id,"message_id":message,"expected_generation":generation,"body":body}})
    };
    let (op, message) = (Uuid::new_v4(), Uuid::new_v4());
    let request = action("1", "Please rebase", op, message);
    let (status, result) = call(&app, "/api/v1/actions", Some(&request), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["data"]["disposition"], "confirmed");
    assert_eq!(result["data"]["affected_entities"][0]["type"], "message");
    assert_eq!(result["data"]["affected_entities"][0]["id"], json!(message));
    let (_, replay) = call(&app, "/api/v1/actions", Some(&request), "owner@localhost").await;
    assert_eq!(replay["data"], result["data"]);
    let (_, lookup) = call(
        &app,
        &format!("/api/v1/operations/{op}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(lookup["data"], result["data"]);
    let changed = action("1", "changed", op, message);
    assert_eq!(
        call(&app, "/api/v1/actions", Some(&changed), "owner@localhost")
            .await
            .0,
        StatusCode::CONFLICT
    );
    let queued: Value =
        sqlx::query_scalar("SELECT recipient FROM mailbox_messages WHERE message_id=$1")
            .bind(message)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(queued["kind"], "session");
    assert_eq!(queued["session_id"], session);
    let stale = action("2", "stale", Uuid::new_v4(), Uuid::new_v4());
    let (_, result) = call(&app, "/api/v1/actions", Some(&stale), "owner@localhost").await;
    assert_eq!(result["data"]["disposition"], "conflict", "{result}");
    assert_eq!(result["data"]["current_versions"], json!({"session":"1"}));
    let unknown = action("1", "nobody", Uuid::new_v4(), Uuid::new_v4());
    let mut unknown = unknown;
    unknown["subject"]["id"] = json!(Uuid::new_v4());
    assert_eq!(
        call(&app, "/api/v1/actions", Some(&unknown), "owner@localhost")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn session_nudge_action_uses_the_stall_nudge_path() {
    let f = fixture().await;
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, list) = call(&app, "/api/v1/sessions", None, "owner@localhost").await;
    let session = list["data"]["items"][0]["id"].clone();
    let l = &f.selection.lineage;
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}')")
        .bind(attempt).bind(f.node.node_id)
        .bind(json!({"task_id":l.task_id,"project_id":l.project_id,"workspace_id":l.workspace_id,"attempt_id":attempt,"session_id":session,"generation":1}))
        .execute(&pool).await.unwrap();
    let op = Uuid::new_v4();
    let request = json!({"operation_id":op,"kind":"session.nudge",
        "subject":{"type":"session","id":session,"revision":"1"},
        "expected_versions":{"session":"1"},
        "payload":{"operation_id":op,"project_id":l.project_id,"expected_generation":"1","body":"Are you stuck?"}});
    let (status, result) = call(&app, "/api/v1/actions", Some(&request), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["data"]["disposition"], "confirmed");
    let message = Uuid::new_v5(&op, b"stall-nudge-message");
    assert_eq!(result["data"]["affected_entities"][0]["id"], json!(message));
    let (_, replay) = call(&app, "/api/v1/actions", Some(&request), "owner@localhost").await;
    assert_eq!(replay["data"], result["data"]);
    let (inbox_only, sender): (bool, Value) = sqlx::query_as("SELECT n.inbox_only,m.sender FROM stall_nudges n JOIN mailbox_messages m USING(message_id) WHERE n.message_id=$1")
        .bind(message).fetch_one(&pool).await.unwrap();
    assert!(!inbox_only, "no terminal: input is ready");
    assert_eq!(sender["kind"], "owner");
}
