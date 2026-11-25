use super::*;
#[tokio::test]
async fn lookup_after_router_restart_preserves_unknown_and_other_principal_is_neutral() {
    use tower::ServiceExt;
    let (_db, state, user, registry, count) = setup().await;
    let request = request("lost");
    let router = routes(registry.clone()).with_state(state.clone());
    let response = router.oneshot(post_request(&request)).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value = response_json(response).await;
    assert_eq!(body["data"]["disposition"], "unknown");
    assert_eq!(body["api_version"], "dashboard-actions-1");
    let restarted = routes(registry.clone()).with_state(state.clone());
    let response = restarted
        .oneshot(get_request(request.operation_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let lookup: serde_json::Value = response_json(response).await;
    assert_eq!(lookup["data"], body["data"]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(user.id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    let revoked_lookup = routes(registry.clone())
        .with_state(state.clone())
        .oneshot(get_request(request.operation_id))
        .await
        .unwrap();
    assert_eq!(revoked_lookup.status(), StatusCode::NOT_FOUND);
    let response = routes(registry)
        .with_state(state.clone())
        .oneshot(post_request(&request))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let other = User {
        id: Uuid::new_v4(),
        ..user
    };
    let error = journal::lookup(&state.store.pool, other.id, request.operation_id)
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn outer_deadline_after_admission_returns_the_retained_operation() {
    use tower::ServiceExt;
    for revoked in [false, true] {
        let (_db, state, user, registry, count) = setup().await;
        let request = request("timeout");
        // Same cancellation semantics as the production admission gate, with a
        // short deterministic deadline after the adapter has begun its effect.
        let actor = user.id;
        let router = Deadline {
            started: count.clone(),
            revoked,
        }
        .router(&state, &user, registry.clone());
        let response = router.oneshot(post_request(&request)).await.unwrap();
        assert_eq!(
            response.status(),
            if revoked {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::ACCEPTED
            }
        );
        let body: serde_json::Value = response_json(response).await;
        if revoked {
            assert!(body.get("data").is_none(), "{body}");
            assert!(body.get("pending_operations").is_none(), "{body}");
            sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
                .bind(actor)
                .execute(&state.store.pool)
                .await
                .unwrap();
        } else {
            assert_eq!(
                body["data"]["operation_id"],
                request.operation_id.to_string()
            );
            assert_eq!(body["data"]["disposition"], "pending");
        }
        assert_eq!(
            submit(&state, &registry, &user, request)
                .await
                .unwrap()
                .disposition,
            Disposition::Pending
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn lookup_requires_current_policy_for_retired_and_unregistered_kinds() {
    use tower::ServiceExt;
    let (_db, state, user, registry, _) = setup().await;
    let request = request("ok");
    submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    journal::retire(
        &state.store.pool,
        (state.clock)() + chrono::Duration::days(91),
    )
    .await
    .unwrap();
    let lookup = || get_request(request.operation_id);
    assert_eq!(
        routes(registry.clone())
            .with_state(state.clone())
            .oneshot(lookup())
            .await
            .unwrap()
            .status(),
        StatusCode::GONE
    );
    assert_eq!(
        routes(Arc::new(Registry::default()))
            .with_state(state.clone())
            .oneshot(lookup())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(user.id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        routes(registry)
            .with_state(state)
            .oneshot(lookup())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}
