use super::*;
use aq_server::reviews::{InboxQuery, InboxState};
use tower::ServiceExt;
fn query() -> InboxQuery {
    InboxQuery {
        project_id: None,
        state: InboxState::Open,
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
