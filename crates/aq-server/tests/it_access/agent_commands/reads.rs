use super::*;
pub(super) async fn project(url: &str) {
    let e = setup_graph(
        Some(json!([
            {"operation":"task.read","scope":"project:self"},
            {"operation":"comment.read","scope":"project:self"}
        ])),
        url,
    )
    .await;
    let (status, own) = invoke(&e, "task.show", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert_eq!(own["result"]["node"]["id"], e.binding["task_id"]);
    assert_eq!(own["result"]["node"]["kind"], "task");
    let (status, other) = invoke(&e, "task.show", json!({"task_id":TASK_3})).await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(other["result"]["node"]["id"], TASK_3);
    // A knowledge record is not a task: the task grant does not read it.
    let (status, body) = invoke(&e, "task.show", json!({"task_id":KNOWLEDGE_2})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found")),
        "{body}"
    );
    let (status, children) = invoke(&e, "task.children", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{children}");
    let edges = children["result"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{children}");
    assert_eq!(edges[0]["target"], TASK_3);
    // Another project is refused before any read.
    let (status, body) = invoke(&e, "task.show", json!({"project_id":Uuid::new_v4()})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    // Comments of another task in scope.
    let (status, page) = invoke(&e, "comment.list", json!({"task_id":TASK_3})).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["task_id"], TASK_3);
    assert_eq!(
        audit_rows(&e, "task.show").await,
        vec![
            ("allow".into(), None),
            ("allow".into(), None),
            ("allow".into(), None),
            ("deny".into(), Some("scope_denied".into()))
        ]
    );
}
pub(super) async fn subtree(url: &str) {
    // task:self reads only the attempt's own task; task:subtree also the
    // tasks it created below it.
    let e = setup_graph(
        Some(json!([
            {"operation":"task.read","scope":"task:subtree"},
            {"operation":"comment.read","scope":"task:self"}
        ])),
        url,
    )
    .await;
    let (status, body) = invoke(&e, "task.show", json!({"task_id":TASK_3})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    assert_eq!(
        body["held"],
        json!([{"operation":"task.read","scope":"task:subtree"}])
    );
    let id = |k: &str| Uuid::parse_str(e.binding[k].as_str().unwrap()).unwrap();
    sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms) VALUES($1,$2,$3,$4,$4,1,$1,0)")
        .bind(Uuid::parse_str(TASK_3).unwrap()).bind(id("attempt_id")).bind(id("project_id")).bind(id("task_id"))
        .execute(&e.pool).await.unwrap();
    let (status, body) = invoke(&e, "task.show", json!({"task_id":TASK_3})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // comment.read at task:self: the own thread, not the child's, on the
    // command surface and on the route itself.
    let (status, _) = invoke(&e, "comment.list", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = invoke(&e, "comment.list", json!({"task_id":TASK_3})).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1,"task_id":TASK_3})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
}
