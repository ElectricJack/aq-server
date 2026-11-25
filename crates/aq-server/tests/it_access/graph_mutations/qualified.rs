use super::*;
async fn history(origin: &str, token: &str) -> Value {
    reqwest::Client::new()
        .get(format!(
            "{origin}/v1/workspaces/{}/nodes/{}/history?limit=64",
            uid(105),
            uid(2)
        ))
        .bearer_auth(token.trim())
        .header("X-AQ-Graph-API-Version", "1")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
pub(super) async fn verify_replay(e: &Env, origin: &str, token: &str) -> Value {
    let before = history(origin, token).await;
    e.fake
        .lose_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let request = correction(e);
    let (status, body) = submit(e, request.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "unknown");
    e.time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
    let retry = recovery(e, &request["operation_id"]);
    let (status, body) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(retry),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["operations"][0]["state"], "confirmed");
    let after = history(origin, token).await;
    assert_eq!(
        after["items"].as_array().unwrap().len(),
        before["items"].as_array().unwrap().len() + 1
    );
    let data = e.fake.data.lock().await;
    assert_eq!(data.posts.len(), 2);
    assert_eq!(data.posts[0], data.posts[1]);
    drop(data);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_outbox")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
    after
}
pub(super) async fn verify_stale(e: &mut Env, origin: &str, token: &str, after: Value) {
    assert_eq!(submit(e, correction(e)).await.0, StatusCode::UNAUTHORIZED);
    let issue = json!({"identity":{"node_id":e.binding["node_id"],"boot_id":e.binding["boot_id"]},"attempt":{"attempt_id":e.binding["attempt_id"],"session_id":e.binding["session_id"],"generation":e.binding["generation"]},"expected_credential_version":1});
    let (status, issued) = send(
        &e.app,
        "/v2/worker/agent-session",
        Some(&e.node_token),
        Some(issue),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    e.token = issued["token"].as_str().unwrap().into();
    assert_eq!(submit(e, correction(e)).await.0, StatusCode::CONFLICT);
    assert_eq!(
        history(origin, token).await,
        after,
        "stale operation never becomes blind overwrite"
    );
}
pub(super) async fn verify_finding_and_relation(e: &Env) {
    let finding = command(
        e,
        json!([{"kind":"create_finding","id":Uuid::new_v4(),"title":"AQ07 retained provenance","body":"Synthetic isolated qualification finding","labels":[],"content_type":"fact"}]),
    );
    let (status, result) = submit(e, finding).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let edge = Uuid::new_v4();
    let relation = command(
        e,
        json!([{"kind":"add_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"3","edge_type":"references","source_revision":null,"target_revision":null,"description":"AQ07 isolated qualification"}]),
    );
    let (status, added) = submit(e, relation).await;
    assert_eq!(status, StatusCode::OK, "{added}");
    let remove = command(
        e,
        json!([{"kind":"remove_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"4","expected_version":"1"}]),
    );
    let (status, removed) = submit(e, remove).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
}
pub(super) async fn verify_draft(e: &Env, origin: &str, token: &str) {
    let mut draft = proposal(e);
    let proposed = draft["commands"][0]["tasks"][0]["id"].clone();
    draft["commands"][0]["relationships"] =
        json!([{"id":Uuid::new_v4(),"source":uid(1),"target":proposed,"edge_type":"parent-child"}]);
    let proposed_id = draft["commands"][0]["tasks"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, receipt) = submit(e, draft).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["receipt"]["applied"], false);
    let unavailable = reqwest::Client::new()
        .get(format!(
            "{origin}/v1/workspaces/{}/nodes/{proposed_id}",
            uid(105)
        ))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(
        unavailable.status(),
        StatusCode::NOT_FOUND,
        "draft task must remain unapplied"
    );
}
pub(super) async fn qualified_proxy(
    fake: &FakeGraph,
    origin: &str,
    request: AxumRequest,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path_and_query().unwrap().as_str().to_string();
    let credential = request.headers().get("authorization").cloned().unwrap();
    let bytes = axum::body::to_bytes(request.into_body(), 1048576)
        .await
        .unwrap();
    if method == axum::http::Method::POST {
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        fake.data.lock().await.posts.push(value);
    }
    let response = reqwest::Client::new()
        .request(method.clone(), format!("{origin}{path}"))
        .header("authorization", credential)
        .header("X-AQ-Graph-API-Version", "1")
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["X-AQ-Graph-API-Version"], "1");
    let body: Value = response.json().await.unwrap();
    let committed_post = method == axum::http::Method::POST && status.is_success();
    if committed_post
        && fake
            .lose_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return graph_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"code":"internal"}}),
        );
    }
    graph_response(status, body)
}
