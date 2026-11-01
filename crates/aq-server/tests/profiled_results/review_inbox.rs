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
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert!(env.service.inbox(f.actor, &query()).await.is_err());
    env.peer.abort();
}
