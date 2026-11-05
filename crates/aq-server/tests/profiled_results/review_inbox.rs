use super::*;
use aq_server::reviews::{InboxQuery, InboxState};
use serde_json::Value;
use tower::ServiceExt;
fn query() -> InboxQuery {
    InboxQuery {
        project_id: None,
        state: InboxState::Open,
        order: Default::default(),
        limit: 1,
        cursor: None,
    }
}
#[tokio::test]
async fn authorized_review_inbox_pages_actual_owner_records_and_binds_cursors() {
    let env = review_env(1).await;
    let f = &env.f;
    for _ in 0..3 {
        let request = aq_review_protocol::DecisionRequest {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            decision_id: Uuid::new_v4(),
            subject: env.prepared.subject().artifact().unwrap(),
            eligible_owners: vec![f.actor],
            eligibility_project_revision: 1,
            review_policy: env.prepared.subject().validation_policy.clone(),
            expires_at_ms: NOW + 30000,
        };
        env.service
            .admit(f.actor, &env.prepared, &request)
            .await
            .unwrap();
    }
    let first = env.service.inbox(f.actor, &query()).await.unwrap();
    assert_eq!(first.items.len(), 1);
    let detail = env
        .service
        .inspect(f.actor, first.items[0].decision_id)
        .await
        .unwrap();
    assert_eq!(
        detail["recorded_result"]["report_id"],
        json!(env.report.report_id)
    );
    assert!(
        !detail["acceptance_criteria"][0]["text"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    let token = first.next_cursor.clone().unwrap();
    assert!(!token.contains(&first.items[0].decision_id.to_string()));
    f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
    let mut expired = query();
    expired.cursor = Some(token.clone());
    assert!(env.service.inbox(f.actor, &expired).await.is_err());
    f.time.store(NOW as i64, Ordering::SeqCst);
    let mut next = query();
    next.cursor = Some(token.clone());
    let second = env.service.inbox(f.actor, &next).await.unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.items[0].decision_id > first.items[0].decision_id);
    assert!(first.items[0].eligible_owner && first.items[0].current_report_head);
    assert_eq!(first.items[0].report_id, env.report.report_id);
    for changed in [
        InboxQuery {
            state: InboxState::Closed,
            ..next.clone()
        },
        InboxQuery {
            project_id: Some(Uuid::new_v4()),
            ..next.clone()
        },
        InboxQuery {
            limit: 2,
            ..next.clone()
        },
    ] {
        assert!(env.service.inbox(f.actor, &changed).await.is_err());
    }
    next.cursor = Some(format!("ff{}", &token[2..]));
    assert!(env.service.inbox(f.actor, &next).await.is_err());
    let foreign_project = env
        .service
        .inbox(
            f.actor,
            &InboxQuery {
                project_id: Some(Uuid::new_v4()),
                ..query()
            },
        )
        .await
        .unwrap();
    assert!(foreign_project.items.is_empty() && foreign_project.next_cursor.is_none());
    let outsider = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'unlisted-owner@localhost','owner',now())").bind(outsider).execute(&f.db.pool().await).await.unwrap();
    let hidden = env.service.inbox(outsider, &query()).await.unwrap();
    assert!(hidden.items.is_empty());
    let mut foreign_cursor = query();
    foreign_cursor.cursor = Some(token.clone());
    assert!(env.service.inbox(outsider, &foreign_cursor).await.is_err());
    let app = aq_server::build_admin_router_with_policy_owners(
        f.state.clone(),
        None,
        Some(Arc::new(env.service.clone())),
    );
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/reviews/decisions?state=open&limit=2")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = axum::body::to_bytes(response.into_body(), 524288)
        .await
        .unwrap();
    let response: aq_server::reviews::InboxPage = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response.api_version, "review-inbox-1");
    assert_eq!(response.items.len(), 2);
    optional_browser(&env, first.items[0].decision_id).await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert!(env.service.inbox(f.actor, &query()).await.is_err());
    env.peer.abort();
}

/// Explicit local-development qualification only. Uses the persisted fixture's
/// actual protected owner and the supplied verified production bundle. No mock
/// interception, fabricated browser responses or browser decision mutation.
async fn optional_browser(env: &ReviewEnv, decision: Uuid) {
    let Some(script) = std::env::var_os("AQ_TEST_REVIEW_BROWSER_SCRIPT") else {
        assert!(std::env::var_os("AQ_TEST_REVIEW_DASHBOARD_DIR").is_none());
        return;
    };
    let bundle = std::env::var_os("AQ_TEST_REVIEW_DASHBOARD_DIR")
        .expect("explicit browser probe requires production dashboard bundle");
    let assets =
        aq_server::dashboard_assets::DashboardAssets::load(std::path::Path::new(&bundle)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = env.f.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = Arc::new(config);
    let router = aq_server::build_admin_router_with_dashboard_and_policy_owners(
        state,
        None,
        Some(Arc::new(env.service.clone())),
        Arc::new(assets),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut probe = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", origin)
        .env("AQ_LIVE_DECISION_ID", decision.to_string())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(25), probe.wait()).await;
    server.abort();
    assert!(
        result
            .expect("bounded actual review browser probe")
            .unwrap()
            .success()
    );
}

async fn dashboard_call(
    app: &axum::Router,
    path: &str,
    body: Option<Value>,
) -> (axum::http::StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(path)
                .method(if body.is_some() { "POST" } else { "GET" })
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    body.map(|b| b.to_string()).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 524288)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn dashboard_decision_resolve_binds_subject_and_shares_native_finality() {
    let env = review_env(1).await;
    let f = &env.f;
    let app = aq_server::build_admin_router_with_policy_owners(
        f.state.clone(),
        None,
        Some(Arc::new(env.service.clone())),
    );
    for legacy_first in [false, true] {
        let request = aq_review_protocol::DecisionRequest {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            decision_id: Uuid::new_v4(),
            subject: env.prepared.subject().artifact().unwrap(),
            eligible_owners: vec![f.actor],
            eligibility_project_revision: 1,
            review_policy: env.prepared.subject().validation_policy.clone(),
            expires_at_ms: NOW + 30000,
        };
        env.service
            .admit(f.actor, &env.prepared, &request)
            .await
            .unwrap();
        let op = Uuid::new_v4();
        let answer = json!({"schema_version":1,"operation_id":op,"decision_id":request.decision_id,"expected_decision_revision":1,"choice":"approve","comment":"Reviewed exact source"});
        let mut payload = answer.clone();
        payload["expected_subject"] = json!(request.subject);
        let action = json!({"operation_id":op,"kind":"decision.resolve","subject":{"type":"decision","id":request.decision_id,"revision":"1"},"expected_versions":{"decision":"1"},"payload":payload});
        let legacy = format!("/api/reviews/decisions/{}/resolve", request.decision_id);
        if legacy_first {
            assert_eq!(
                dashboard_call(&app, &legacy, Some(answer.clone())).await.0,
                200
            );
        }
        let mut wrong = action.clone();
        wrong["payload"]["expected_subject"]["version"] = json!("2");
        assert_eq!(
            dashboard_call(&app, "/api/v1/actions", Some(wrong)).await.0,
            409
        );
        let (code, created) = dashboard_call(&app, "/api/v1/actions", Some(action.clone())).await;
        assert_eq!(code, 200, "{created}");
        assert_eq!(created["data"]["disposition"], "confirmed");
        assert_eq!(
            created["data"]["affected_entities"][0]["id"],
            json!(request.decision_id)
        );
        let (code, native) = dashboard_call(&app, &legacy, Some(answer.clone())).await;
        assert_eq!(code, 200, "{native}");
        assert_eq!(native["finality"]["operation_id"], json!(op));
        assert_eq!(
            dashboard_call(&app, "/api/v1/actions", Some(action.clone()))
                .await
                .1["data"],
            created["data"]
        );
        assert_eq!(
            dashboard_call(&app, &format!("/api/v1/operations/{op}"), None)
                .await
                .1["data"],
            created["data"]
        );
        let mut changed = action;
        changed["payload"]["choice"] = json!("reject");
        assert_eq!(
            dashboard_call(&app, "/api/v1/actions", Some(changed))
                .await
                .0,
            409
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM human_decision_finality")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM review_owner_outbox")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 2);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    let op: Uuid = sqlx::query_scalar("SELECT operation_id FROM dashboard_operations LIMIT 1")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(
        dashboard_call(&app, &format!("/api/v1/operations/{op}"), None)
            .await
            .0,
        404
    );
    env.peer.abort();
}

#[tokio::test]
async fn overview_attention_lists_waiting_decision_and_failed_task() {
    let env = Box::pin(review_env_for_outcome(1, None, false, true)).await;
    let request = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: env.prepared.subject().artifact().unwrap(),
        eligible_owners: vec![env.f.actor],
        eligibility_project_revision: 1,
        review_policy: env.prepared.subject().validation_policy.clone(),
        expires_at_ms: NOW + 30000,
    };
    env.service
        .admit(env.f.actor, &env.prepared, &request)
        .await
        .unwrap();
    let app = aq_server::build_admin_router_with_policy_owners(
        env.f.state.clone(),
        None,
        Some(Arc::new(env.service.clone())),
    );
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get("/api/v1/overview")
                .header("x-aq-dev-email", "owner@localhost")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1048576)
            .await
            .unwrap(),
    )
    .unwrap();
    let items = body["data"]["attention"].as_array().unwrap();
    assert!(items.iter().any(|v| v["kind"] == "decision"), "{body}");
    assert!(items.iter().any(|v| v["kind"] == "failed"), "{body}");
    assert!(items.iter().all(|v| v["subject"]["revision"].is_string()));
    assert!(
        !body["data"]["recent_outcomes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let typed: aq_server::dashboard_api::contract::Envelope<
        aq_server::dashboard_api::contract::Overview,
    > = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), body);
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        std::fs::write(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../protocol/dashboard-read/v1/fixtures/overview-nonempty.json"),
            format!("{}\n", serde_json::to_string_pretty(&body).unwrap()),
        )
        .unwrap();
    }
    let project = env.prepared.subject().project_id;
    let decision = items.iter().find(|v| v["kind"] == "decision").unwrap();
    assert_eq!(decision["subject"]["id"], json!(request.decision_id));
    assert_eq!(decision["subject"]["revision"], "1");
    assert!(decision["since"].is_null());
    assert!(decision["action"].is_null());
    assert_eq!(decision["project"]["id"], json!(project));
    assert_eq!(body["data"]["counts"]["needs_you"]["value"], 1);
    assert_eq!(body["data"]["counts"]["needs_you"]["accuracy"], "estimated");
    let retained = Box::pin(aq_server::profiled_execution::human_results::read_as(
        &env.f.state,
        &env.f
            .state
            .store
            .find_user_by_binding("dev", "owner@localhost")
            .await
            .unwrap()
            .unwrap(),
        &aq_server::profiled_execution::human_results::Page::default(),
        None,
    ))
    .await
    .unwrap();
    let digest = format!("sha256:{}", retained.items[0].revision);
    let failed = items.iter().find(|v| v["kind"] == "failed").unwrap();
    assert_eq!(failed["subject"]["id"], json!(env.report.report_id));
    assert_eq!(failed["subject"]["revision"], digest);
    assert_eq!(failed["project"]["id"], json!(project));
    let outcome = body["data"]["recent_outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["subject"]["id"] == json!(env.report.report_id))
        .unwrap();
    assert_eq!(outcome["subject"]["revision"], digest);
    assert_eq!(outcome["outcome"], "process_failed");
    assert_eq!(outcome["at"], "1970-01-01T00:16:40.000Z");
    assert_eq!(body["coverage"]["state"], "partial");
    let sources = body["data"]["sources"].as_array().unwrap();
    assert!(sources.iter().any(|v| v["source"] == "questions"
        && v["total"]["value"].is_null()
        && v["coverage"]["reason"] == "question_owner_not_mounted"));
    assert!(sources.iter().any(|v| v["source"] == "results"
        && v["observation"]["observed_at"] == "1970-01-01T00:16:40.000Z"
        && v["total"]["value"] == 1));
    let pool = env.f.db.pool().await;
    for (email, role) in [
        ("other-owner@localhost", "owner"),
        ("excluded-viewer@localhost", "viewer"),
    ] {
        sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,$3,'dev',$2,now())").bind(Uuid::new_v4()).bind(email).bind(role).execute(&pool).await.unwrap();
        let other = overview_as(&app, "/api/v1/overview", email).await;
        assert!(!other.to_string().contains(&request.decision_id.to_string()));
        if role == "viewer" {
            assert!(other["data"]["attention"].as_array().unwrap().is_empty());
        }
    }
    let excluded = overview_as(
        &app,
        &format!("/api/v1/overview?scope=project:{}", Uuid::new_v4()),
        "owner@localhost",
    )
    .await;
    assert!(excluded["data"]["attention"].as_array().unwrap().is_empty());
    assert!(
        excluded["data"]["recent_outcomes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !excluded
            .to_string()
            .contains(&env.report.report_id.to_string())
    );
    // A missing mounted review owner is an unknown contribution, never zero.
    let unmounted = aq_server::build_admin_router(env.f.state.clone());
    let partial = overview_as(&unmounted, "/api/v1/overview", "owner@localhost").await;
    assert!(partial["data"]["counts"]["needs_you"]["value"].is_null());
    assert!(
        partial["data"]["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "failed")
    );
    // An unavailable result owner is contained within the source contribution.
    sqlx::query("ALTER TABLE profiled_result_reports RENAME TO unavailable_result_reports")
        .execute(&pool)
        .await
        .unwrap();
    let partial = overview_as(&unmounted, "/api/v1/overview", "owner@localhost").await;
    assert!(
        partial["data"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["source"] == "results" && v["total"]["value"].is_null())
    );
    assert!(
        partial["data"]["recent_outcomes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    env.peer.abort();
}

pub(super) async fn overview_as(app: &axum::Router, path: &str, email: &str) -> Value {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get(path)
                .header("x-aq-dev-email", email)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1048576)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn overview_running_and_recovering_use_started_and_stopped_owners() {
    let (f, start, _) = Box::pin(prepare(true, false)).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let running = overview_as(&app, "/api/v1/overview", "owner@localhost").await;
    assert_eq!(
        running["data"]["counts"]["running"]["value"], 1,
        "{running}"
    );
    assert_eq!(running["data"]["counts"]["recovering"]["value"], 0);
    let auth = context(&f).await;
    stopped::record(&f.state, &auth, &native_started::stop_request(&start))
        .await
        .unwrap();
    let stopped = overview_as(&app, "/api/v1/overview", "owner@localhost").await;
    assert_eq!(stopped["data"]["counts"]["running"]["value"], 0);
    assert_eq!(
        stopped["data"]["counts"]["recovering"]["value"], 1,
        "{stopped}"
    );
}

/// G11 X14–X19 contract over the actual owner: kind/payload/judgment,
/// requester and submission time, open total and impact order, previous
/// responses and change summary, the Defer choice, and evidence viewed state.
#[tokio::test]
async fn decision_view_contract_serves_kind_history_defer_and_evidence() {
    let env = review_env(1).await;
    let f = &env.f;
    let app = aq_server::build_admin_router_with_policy_owners(
        f.state.clone(),
        None,
        Some(Arc::new(env.service.clone())),
    );
    let admit = || async {
        let request = aq_review_protocol::DecisionRequest {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            decision_id: Uuid::new_v4(),
            subject: env.prepared.subject().artifact().unwrap(),
            eligible_owners: vec![f.actor],
            eligibility_project_revision: 1,
            review_policy: env.prepared.subject().validation_policy.clone(),
            expires_at_ms: NOW + 30000,
        };
        env.service
            .admit(f.actor, &env.prepared, &request)
            .await
            .unwrap();
        request.decision_id
    };
    let first = admit().await;
    let answer = json!({"schema_version":1,"operation_id":Uuid::new_v4(),"decision_id":first,"expected_decision_revision":1,"choice":"reject","comment":"Needs a test"});
    let (code, body) = dashboard_call(
        &app,
        &format!("/api/reviews/decisions/{first}/resolve"),
        Some(answer),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let second = admit().await;
    let third = admit().await;

    // X14/X15/X16 on the inbox item and page.
    let page = env
        .service
        .inbox(
            f.actor,
            &InboxQuery {
                limit: 50,
                ..query()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.open_total, Some(2));
    assert_eq!(page.items.len(), 2);
    let item = &page.items[0];
    assert_eq!(item.kind, aq_server::reviews::DecisionKind::Review);
    assert_eq!(item.requested_by.user_id, f.actor);
    assert_eq!(item.submitted_at_ms, Some(NOW));
    assert_eq!(item.kind_payload.subject_profile, "code_review_v1");
    assert_eq!(
        item.kind_payload.criteria_count as usize,
        env.prepared.subject().criteria.len()
    );
    // No graph runtime in this fixture: enrichment is null, never an error.
    assert!(item.title.is_none() && item.impact.is_none());
    assert!(
        item.requested_judgment
            .contains(&format!("task {}", item.task_id))
    );
    // X18: defer is non-final, per-owner, bounded by expiry, idempotent.
    let op = Uuid::new_v4();
    let defer = format!("/api/reviews/decisions/{second}/defer");
    let late = json!({"operation_id":op,"expected_decision_revision":1,"until_ms":NOW+30001});
    assert_eq!(dashboard_call(&app, &defer, Some(late)).await.0, 400);
    let body = json!({"operation_id":op,"expected_decision_revision":1,"until_ms":NOW+20000});
    let (code, deferred) = dashboard_call(&app, &defer, Some(body.clone())).await;
    assert_eq!(code, 200, "{deferred}");
    assert_eq!(deferred["deferred_until_ms"], NOW + 20000);
    assert_eq!(dashboard_call(&app, &defer, Some(body)).await.0, 200);
    let changed = json!({"operation_id":op,"expected_decision_revision":1,"until_ms":NOW+10000});
    assert_eq!(dashboard_call(&app, &defer, Some(changed)).await.0, 409);
    let closed =
        json!({"operation_id":Uuid::new_v4(),"expected_decision_revision":1,"until_ms":NOW+10000});
    assert_eq!(
        dashboard_call(
            &app,
            &format!("/api/reviews/decisions/{first}/defer"),
            Some(closed)
        )
        .await
        .0,
        409
    );
    let impact = env
        .service
        .inbox(
            f.actor,
            &InboxQuery {
                order: aq_server::reviews::InboxOrder::Impact,
                ..query()
            },
        )
        .await
        .unwrap();
    assert_eq!(impact.order, aq_server::reviews::InboxOrder::Impact);
    assert!(!impact.order_complete);
    assert_eq!(impact.open_total, Some(2));
    // Equal (unknown) impact: the undeferred decision ranks first.
    assert_eq!(impact.items[0].decision_id, third);
    let mut more = query();
    more.order = aq_server::reviews::InboxOrder::Impact;
    more.cursor = impact.next_cursor.clone();
    let rest = env.service.inbox(f.actor, &more).await.unwrap();
    assert_eq!(rest.items[0].decision_id, second);
    assert_eq!(rest.items[0].deferred_until_ms, Some(NOW + 20000));
    assert!(rest.next_cursor.is_none());
    // Cursors bind the order.
    more.order = aq_server::reviews::InboxOrder::Decision;
    assert!(env.service.inbox(f.actor, &more).await.is_err());
    assert!(
        env.service
            .inbox(
                f.actor,
                &InboxQuery {
                    state: InboxState::Closed,
                    order: aq_server::reviews::InboxOrder::Impact,
                    ..query()
                },
            )
            .await
            .is_err()
    );

    // X17/X18/X19 on the decision read.
    let path = format!("/api/reviews/decisions/{second}");
    let (code, view) = dashboard_call(&app, &path, None).await;
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["kind"], "review");
    assert_eq!(view["requested_by"]["user_id"], json!(f.actor));
    assert_eq!(view["submitted_at_ms"], NOW);
    assert_eq!(view["deferred_until_ms"], NOW + 20000);
    let previous = view["previous_responses"].as_array().unwrap();
    assert_eq!(previous.len(), 1, "{view}");
    assert_eq!(previous[0]["decision_id"], json!(first));
    assert_eq!(previous[0]["outcome"], "reject");
    assert_eq!(previous[0]["has_comment"], true);
    assert_eq!(previous[0]["responded_by"]["user_id"], json!(f.actor));
    assert_eq!(
        view["changes_since_previous"]["previous_decision_id"],
        json!(first)
    );
    assert_eq!(view["changes_since_previous"]["changes"], json!([]));
    assert!(
        view["what_happens_next"]
            .as_str()
            .unwrap()
            .contains("deferred")
    );
    let choices: Vec<_> = view["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap(), c["final"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        choices,
        [
            ("approve", true),
            ("request_changes", true),
            ("defer", false)
        ]
    );
    let evidence = view["evidence"].as_array().unwrap();
    let result = evidence
        .iter()
        .find(|e| e["evidence_id"] == "result_record")
        .unwrap();
    let report = env.report.report_id;
    assert!(
        result["link"]["href"]
            .as_str()
            .unwrap()
            .starts_with(&format!("/results/{report}/revisions/"))
    );
    assert!(result["viewed_at_ms"].is_null());
    assert!(
        evidence
            .iter()
            .find(|e| e["evidence_id"] == "task_context")
            .unwrap()["link"]["href"]
            == format!("/work/{}", env.prepared.subject().task.task_id)
    );
    let viewed = format!("/api/reviews/decisions/{second}/evidence/result_record/viewed");
    let stale = json!({"ref_digest":"0".repeat(64)});
    assert_eq!(dashboard_call(&app, &viewed, Some(stale)).await.0, 409);
    let missing = format!("/api/reviews/decisions/{second}/evidence/nope/viewed");
    let digest = json!({"ref_digest":result["ref"]["digest"]});
    assert_eq!(
        dashboard_call(&app, &missing, Some(digest.clone())).await.0,
        404
    );
    let (code, seen) = dashboard_call(&app, &viewed, Some(digest.clone())).await;
    assert_eq!(code, 200, "{seen}");
    assert_eq!(seen["viewed_at_ms"], NOW);
    f.time.store((NOW + 5) as i64, Ordering::SeqCst);
    assert_eq!(
        dashboard_call(&app, &viewed, Some(digest)).await.1["viewed_at_ms"],
        NOW
    );
    let (_, view) = dashboard_call(&app, &path, None).await;
    let result = view["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["evidence_id"] == "result_record")
        .unwrap()
        .clone();
    assert_eq!(result["viewed_at_ms"], NOW);
    // Clearing a deferral restores the plain pending view.
    let clear =
        json!({"operation_id":Uuid::new_v4(),"expected_decision_revision":1,"until_ms":null});
    assert_eq!(dashboard_call(&app, &defer, Some(clear)).await.0, 200);
    assert!(dashboard_call(&app, &path, None).await.1["deferred_until_ms"].is_null());
    env.peer.abort();
}
