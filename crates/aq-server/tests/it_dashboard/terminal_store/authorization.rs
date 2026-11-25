use super::*;

#[tokio::test]
async fn viewer_inspection_is_exact_revocable_historical_and_never_control_authority() {
    use aq_terminal_relay::api::{OutputReply, ViewQuery};
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let pool = f.db.pool().await;
    let viewer = new_viewer(&pool).await;
    assert!(
        service::inspect(&f.state, viewer, r.terminal.terminal_id)
            .await
            .is_err()
    );
    let grant = Grant::Inspect.for_actor(&r, viewer);
    service::set_access(&f.state, f.actor, &grant)
        .await
        .unwrap();
    service::inspect(&f.state, viewer, r.terminal.terminal_id)
        .await
        .unwrap();
    assert!(
        service::mutate(
            &f.state,
            viewer,
            &service::Mutation::Control(control_request(&r, 2_000))
        )
        .await
        .is_err()
    );
    let subscription = watch(&f, &r, viewer, 15_000).await;
    let query = ViewQuery {
        subscription_id: subscription,
        after_sequence: 0,
    };
    assert!(matches!(
        service::output(&f.state, viewer, r.terminal.terminal_id, &query)
            .await
            .unwrap(),
        OutputReply::Pending { .. }
    ));
    let node = authenticated(&f).await;
    upload_private_snapshot(&f, &r, &node).await;
    f.time
        .store((NOW + 6_000) as i64, std::sync::atomic::Ordering::SeqCst);
    let OutputReply::Ready { view, .. } =
        service::output(&f.state, viewer, r.terminal.terminal_id, &query)
            .await
            .unwrap()
    else {
        panic!()
    };
    assert!(view.disconnected); // execution grant expired, historical ACL remains current
    let mut revoke = grant;
    revoke.operation_id = Uuid::new_v4();
    revoke.expected_inspection_revision = 1;
    revoke.expected_control_revision = 1;
    revoke.can_inspect = false;
    service::set_access(&f.state, f.actor, &revoke)
        .await
        .unwrap();
    assert!(
        service::output(&f.state, viewer, r.terminal.terminal_id, &query)
            .await
            .is_err()
    );
    let polled = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    assert!(!polled.effects.iter().any(|e|matches!(e,aq_terminal_relay::draft::Effect::Watch(w) if w.subscription_id==subscription)));
}

#[tokio::test]
async fn actual_http_separates_listeners_csrf_identity_binding_and_sensitive_errors() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let admin = aq_server::build_admin_router(f.state.clone());
    let worker = aq_server::build_worker_router(f.state.clone());
    let path = format!("/api/v1/terminals/{}", r.terminal.terminal_id);
    assert_listener_routing(&admin, &worker, &path).await;
    let request = service::Mutation::Control(control_request(&r, 4_000));
    let path = format!("{path}/mutations");
    let raw = serde_json::to_vec(&request).unwrap();
    let post = |csrf: bool, origin: &str, body: Vec<u8>| {
        let mut b = Request::post(&path)
            .header("content-type", "application/json")
            .header("x-aq-dev-email", "owner@localhost")
            .header("origin", origin);
        if csrf {
            b = b.header("x-aq-request", "1");
        }
        b.body(Body::from(body)).unwrap()
    };
    for (csrf, origin, expected) in [
        (false, "http://127.0.0.1:8080", StatusCode::FORBIDDEN),
        (true, "https://wrong.example", StatusCode::FORBIDDEN),
        (true, "http://127.0.0.1:8080", StatusCode::OK),
    ] {
        let response = admin
            .clone()
            .oneshot(post(csrf, origin, raw.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let response = admin
        .clone()
        .oneshot(post(
            true,
            "http://127.0.0.1:8080",
            br#"{"kind":"input","body":{"bytes":"private raw input sentinel"}}"#.to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        response
            .headers()
            .contains_key("aq-terminal-server-time-ms")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["code"], "invalid_request");
    assert!(error["request_id"].is_string());
    assert!(!String::from_utf8_lossy(&bytes).contains("sentinel"));
    assert_worker_registration(worker, &f.credential, &r).await;
}

#[tokio::test]
async fn control_only_principal_can_inspect_own_receipt_after_expiry_without_output_access() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let pool = f.db.pool().await;
    let actor = new_viewer(&pool).await;
    service::set_access(&f.state, f.actor, &Grant::Control.for_actor(&r, actor))
        .await
        .unwrap();
    assert!(
        service::inspect(&f.state, actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
    let input = claimed_input_as(&f, &r, actor).await;
    f.time
        .store((NOW + 6000) as i64, std::sync::atomic::Ordering::SeqCst);
    aq_server::terminal::runtime::maintenance(&f.state, Uuid::nil())
        .await
        .unwrap();
    let receipt = service::receipt(&f.state, actor, r.terminal.terminal_id, input.operation_id)
        .await
        .unwrap();
    assert_eq!(
        receipt.disposition,
        aq_session_protocol::InputDisposition::DeliveryUnknown
    );
    assert!(
        service::inspect(&f.state, actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sealed_output_only_terminal_registers_and_watches_but_cannot_acquire_control() {
    let mut policy = aq_session_protocol::TerminalPolicy {
        host: aq_session_protocol::TerminalHost::Tmux,
        input_enabled: false,
        max_input_bytes: 1,
        max_pending_input_bytes: 1,
        max_frame_bytes: 64,
        replay_bytes: 64,
    };
    let (f, _dir, r) = running_with_policy(Some(policy.clone())).await;
    register(&f, &r).await;
    let info = service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    assert!(!info.terminal_policy.input_enabled);
    assert!(
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::Control(control_request(&r, 2_000))
        )
        .await
        .is_err()
    );
    watch(&f, &r, f.actor, 10_000).await;
    policy.input_enabled = true;
    assert_ne!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .unwrap()
            .terminal_policy,
        policy
    );
}

async fn upload_private_snapshot(
    f: &Fixture,
    r: &Register,
    node: &aq_server::terminal::peer::AuthenticatedNode,
) {
    use aq_session_protocol::{ArtifactRef, Digest, Snapshot};
    use aq_terminal_relay::draft::PeerUpload;
    let raw = b"synthetic private output".to_vec();
    let snapshot = Snapshot {
        snapshot_id: Uuid::new_v4(),
        capture_request_id: Uuid::new_v4(),
        terminal: r.terminal.clone(),
        through_sequence: 0,
        columns: 80,
        rows: 24,
        renderer: ArtifactRef {
            id: Text::new("aq.synthetic.plaintext").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic renderer"),
        },
        digest: Digest::of(&raw),
        state: raw,
    };

    aq_server::terminal::peer::upload(
        &f.state,
        node,
        &PeerUpload {
            snapshots: vec![snapshot],
            ..empty_upload(r, 0)
        },
    )
    .await
    .unwrap();
}

async fn assert_worker_registration(worker: axum::Router, credential: &str, r: &Register) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let register = serde_json::to_vec(r).unwrap();
    assert_eq!(
        worker
            .clone()
            .oneshot(
                Request::post("/v1/terminal-relay/register")
                    .header("content-type", "application/json")
                    .body(Body::from(register.clone()))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        worker
            .oneshot(
                Request::post("/v1/terminal-relay/register")
                    .header("authorization", format!("Bearer {}", credential))
                    .header("content-type", "application/json")
                    .body(Body::from(register))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

async fn assert_listener_routing(admin: &axum::Router, worker: &axum::Router, path: &str) {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    assert_eq!(
        admin
            .clone()
            .oneshot(
                Request::get(path)
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        worker
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}
