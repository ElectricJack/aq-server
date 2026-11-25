use super::*;
pub(super) async fn membership(e: &Env) -> Value {
    let first = correction(e);
    let second = correction(e);
    assert_eq!(admit_only(e, &first).await.0, StatusCode::ACCEPTED);
    assert_eq!(admit_only(e, &second).await.0, StatusCode::ACCEPTED);
    let mut request = recovery(e, &first["operation_id"]);
    request["mode"] = json!("list");
    request.as_object_mut().unwrap().remove("operation_ids");
    request["limit"] = json!(1);
    request["cursor"] = Value::Null;
    let (status, page) = send(
        &e.app,
        "/v2/worker/agent-operations/query",
        Some(&e.node_token),
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["operations"][0]["operation_id"], first["operation_id"]);
    let opaque = page["next_cursor"].as_str().unwrap();
    assert!(opaque.starts_with("aqc_") && !opaque.starts_with("aqa_"));
    // A later UUID sorts below previous IDs but is beyond the frozen membership.
    let mut concurrent = correction(e);
    concurrent["operation_id"] = json!(uid(1));
    assert_eq!(admit_only(e, &concurrent).await.0, StatusCode::ACCEPTED);
    let mut next = request.clone();
    next["cursor"] = page["next_cursor"].clone();
    let (status, tail) = send(
        &e.app,
        "/v2/worker/agent-operations/query",
        Some(&e.node_token),
        Some(next.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tail}");
    assert_eq!(tail["operations"].as_array().unwrap().len(), 1);
    assert_eq!(
        tail["operations"][0]["operation_id"],
        second["operation_id"]
    );
    assert_eq!(tail["coverage"]["status"], "complete");
    next["limit"] = json!(2);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/agent-operations/query",
            Some(&e.node_token),
            Some(next)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    request
}
pub(super) async fn quota(e: &Env, request: Value) {
    // Reserve 127 valid opaque cursor rows, then race two allocations. Node locking
    // must let exactly one reach the 128 ceiling; no thousands-of-operation fixture.
    sqlx::query("INSERT INTO agent_operation_cursors(verifier,node_id,attempt_id,query_digest,after_sequence,upper_sequence,expires_at_ms) SELECT 'fixture-'||n,$1,$2,'fixture',0,0,2000000 FROM generate_series(1,126) n").bind(e.binding["node_id"].as_str().unwrap().parse::<Uuid>().unwrap()).bind(e.binding["attempt_id"].as_str().unwrap().parse::<Uuid>().unwrap()).execute(&e.pool).await.unwrap();
    let (a, b) = tokio::join!(
        send(
            &e.app,
            "/v2/worker/agent-operations/query",
            Some(&e.node_token),
            Some(request.clone())
        ),
        send(
            &e.app,
            "/v2/worker/agent-operations/query",
            Some(&e.node_token),
            Some(request)
        )
    );
    let statuses = [a.0, b.0];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::SERVICE_UNAVAILABLE));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_operation_cursors")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        128
    );
}
